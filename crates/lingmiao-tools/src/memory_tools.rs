//! Memory tools (Q6 / M4 binding): the four `lingmiao-memory` stores exposed as
//! [`Tool`]s so the model's tool loop can search and mutate memory.
//!
//! The `组织上下文` and `沉淀阶段` stages whitelist these names (see
//! `crates/lingmiao-core/assets/stages.json`); `工作阶段` also allows the two writers
//! (`update_memory` / `update_knowledge`). Without them the whitelist referenced
//! tools that the registry did not contain — the "binding gap" this module
//! closes.
//!
//! All tools share one [`Memory`] handle (chat zone) behind an `Arc`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use lingmiao_core::brand;
use lingmiao_memory::{
    Archive, KnowledgeGraph, Memory, NewNode, NewObservation, Node, Observation, Observations,
    Turn, Zone,
};

use crate::tool::{Tool, ToolError, ToolOutput, ToolRegistry, json_schema};

const DEFAULT_LIMIT: usize = 10;

/// Characters kept from a record's body when a memory read tool projects it
/// for the model.
///
/// These tools feed the model's tool loop directly, so what they return *is*
/// context. The Python original truncated every projection hard
/// (`user_msg[:300]`, `content[:300]`, `summary[:300]`); the Rust port initially
/// serialised the whole row instead — and a `turn` row carries `full_messages`
/// / `tool_calls` / `reasoning`, megabytes each. That is exactly how
/// `list_archive(limit=6)` pushed a 沉淀阶段 request to 1,622,701 tokens and
/// drew `HTTP 400 [上下文超限]` (cli 2026-09-28, screenshot 055716). Projections
/// are therefore bounded again, and the ids/`at` are kept so the model can go
/// back for a specific record.
pub const RECORD_BODY_CHARS: usize = 300;

fn to_output<T: serde::Serialize>(value: &T) -> ToolOutput {
    match serde_json::to_string_pretty(value) {
        Ok(s) => ToolOutput::ok(s),
        Err(e) => ToolOutput::error(format!("serialize failed: {e}")),
    }
}

fn limit_or_default(limit: usize) -> usize {
    if limit == 0 {
        DEFAULT_LIMIT
    } else {
        limit.min(100)
    }
}

fn mem_err(tool: &str, e: lingmiao_core::LingmiaoError) -> ToolError {
    ToolError::Other(format!("{tool}: {e}"))
}

// ── argument schemas ───────────────────────────────────────────

#[derive(Deserialize, schemars::JsonSchema)]
struct SearchArgs {
    /// Free-text keywords (multi-word OR match).
    keyword: String,
    /// Optional record kind filter (e.g. `constraint`, `decision`).
    #[serde(default)]
    kind: String,
    /// Maximum rows (default 10, capped at 100).
    #[serde(default)]
    limit: usize,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct KnowledgeSearchArgs {
    /// Free-text query for semantic node retrieval.
    keyword: String,
    /// Maximum nodes (default 10, capped at 100).
    #[serde(default)]
    limit: usize,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct ArchiveSearchArgs {
    /// Free-text keywords over the turn transcript.
    keyword: String,
    /// Maximum turns (default 10, capped at 100).
    #[serde(default)]
    limit: usize,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct ListArgs {
    /// Maximum rows (default 10, capped at 100).
    #[serde(default)]
    limit: usize,
    /// Offset for pagination.
    #[serde(default)]
    offset: usize,
    /// Optional kind filter.
    #[serde(default)]
    kind: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct UpdateMemoryArgs {
    /// Record kind (`fact` / `preference` / `decision` / `constraint` / …).
    kind: String,
    /// Grouping topic (defaults to `kind`).
    #[serde(default)]
    topic: String,
    /// Short unique label.
    name: String,
    /// Full body text.
    content: String,
    /// Comma-separated search keywords.
    #[serde(default)]
    keywords: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct UpdateKnowledgeArgs {
    /// Node kind (`fact` / `decision` / `technology` / `project` / …).
    kind: String,
    /// Unique-within-kind name (the upsert key).
    name: String,
    /// One-line summary.
    summary: String,
    /// Full content.
    content: String,
    /// Comma-separated keywords.
    #[serde(default)]
    keywords: String,
}

/// `link_entries` arguments（D 项 2026-10-05 双向链接广义化）。
///
/// `from` / `to` are `layer:id` pairs (`observations:obs-…`,
/// `knowledge:node-…`, `archive:turn-…`, `business:<rowid>`); a bare `obs-…` /
/// `node-…` / `turn-…` id is accepted and its layer inferred from the prefix.
/// `to` is a list, so one call can wire a whole fan-out.
#[derive(Deserialize, schemars::JsonSchema)]
struct LinkEntriesArgs {
    /// Source endpoint (`layer:id`, e.g. `observations:obs-1a2b`).
    from: String,
    /// Target endpoint(s) — same syntax as `from`. Accepts a list for a fan-out.
    to: Vec<String>,
    /// Relation verb (free text, e.g. `supports` / `refines` / `supersedes`).
    #[serde(default)]
    relation: String,
}

/// Parse one `link_entries` endpoint into `(layer, id)`.
///
/// Accepts the explicit `layer:id` form, and — for convenience — a bare id whose
/// layer is inferred from its prefix (`obs-` → observations, `node-` →
/// knowledge, `turn-` → archive). An unknown layer is a hard error so a typo
/// cannot silently create a dangling link.
fn parse_endpoint(raw: &str) -> Result<(String, String), ToolError> {
    let t = raw.trim();
    if t.is_empty() {
        return Err(ToolError::invalid(
            "link_entries",
            "endpoint must not be empty",
        ));
    }
    if let Some((layer, id)) = t.split_once(':') {
        let layer = layer.trim().to_ascii_lowercase();
        if !lingmiao_memory::LINK_LAYERS.contains(&layer.as_str()) {
            return Err(ToolError::invalid(
                "link_entries",
                format!(
                    "unknown layer `{layer}` (expected one of {})",
                    lingmiao_memory::LINK_LAYERS.join(" / ")
                ),
            ));
        }
        if id.trim().is_empty() {
            return Err(ToolError::invalid(
                "link_entries",
                format!("`{t}` has an empty id"),
            ));
        }
        return Ok((layer, id.trim().to_string()));
    }
    let layer = if t.starts_with("obs-") {
        "observations"
    } else if t.starts_with("node-") {
        "knowledge"
    } else if t.starts_with("turn-") {
        "archive"
    } else {
        return Err(ToolError::invalid(
            "link_entries",
            format!(
                "cannot infer the layer of `{t}` — use `layer:id` (layers: {})",
                lingmiao_memory::LINK_LAYERS.join(" / ")
            ),
        ));
    };
    Ok((layer.to_string(), t.to_string()))
}

fn parse_args<T: for<'de> Deserialize<'de>>(tool: &str, arguments: Value) -> Result<T, ToolError> {
    serde_json::from_value(arguments).map_err(|e| ToolError::invalid(tool, e.to_string()))
}

// ── compact projections (context safety) ───────────────────────
//
// Each memory read tool returns a *projection*, never the stored row: the
// archive row alone is megabytes (`full_messages` / `tool_calls` / `reasoning`),
// and these results are re-sent on every later round-trip of the stage's tool
// loop. `RECORD_BODY_CHARS` mirrors the Python original's `[:300]` caps; ids and
// timestamps survive so the model can still name a record precisely.

/// Project one archived turn (#1) into the compact shape the model sees.
fn turn_view(t: &Turn) -> Value {
    json!({
        "id": t.id,
        "at": t.at,
        "user_msg": preview(&t.user_msg, RECORD_BODY_CHARS),
        "assistant": preview(&t.assistant, RECORD_BODY_CHARS),
        "summary": preview(&t.summary, RECORD_BODY_CHARS),
        "tokens_total": t.tokens_total,
    })
}

/// Project one observation (#2).
fn observation_view(o: &Observation) -> Value {
    json!({
        "id": o.id,
        "at": o.at,
        "kind": o.kind,
        "topic": o.topic,
        "name": o.name,
        "content": preview(&o.content, RECORD_BODY_CHARS),
        "keywords": o.keywords,
    })
}

/// Project one knowledge node (#3).
fn node_view(n: &Node) -> Value {
    json!({
        "id": n.id,
        "kind": n.kind,
        "name": n.name,
        "summary": preview(&n.summary, RECORD_BODY_CHARS),
        "content": preview(&n.content, RECORD_BODY_CHARS),
        "keywords": n.keywords,
    })
}

// ── read tools ─────────────────────────────────────────────────

/// `search_observations` — keyword search over the #2 observation log.
pub struct SearchObservationsTool {
    memory: Arc<Memory>,
}

#[async_trait]
impl Tool for SearchObservationsTool {
    fn name(&self) -> &str {
        "search_observations"
    }
    fn description(&self) -> &str {
        "Search the observation log (#2) for facts, decisions, constraints, preferences. Multi-word OR match."
    }
    fn parameters(&self) -> Value {
        json_schema::<SearchArgs>()
    }
    async fn execute(&self, arguments: Value) -> Result<ToolOutput, ToolError> {
        let a: SearchArgs = parse_args("search_observations", arguments)?;
        if a.keyword.trim().is_empty() {
            return Err(ToolError::invalid(
                "search_observations",
                "`keyword` must not be empty",
            ));
        }
        let rows = self
            .memory
            .observations
            .search(&a.keyword, limit_or_default(a.limit), &a.kind)
            .map_err(|e| mem_err("search_observations", e))?;
        if rows.is_empty() {
            return Ok(ToolOutput::ok("No matching observations."));
        }
        let out: Vec<Value> = rows
            .iter()
            .map(|s| {
                json!({
                    "score": s.score,
                    "observation": observation_view(&s.observation),
                })
            })
            .collect();
        Ok(to_output(&out))
    }
}

/// `search_knowledge` — semantic search over the #3 knowledge graph.
pub struct SearchKnowledgeTool {
    memory: Arc<Memory>,
}

#[async_trait]
impl Tool for SearchKnowledgeTool {
    fn name(&self) -> &str {
        "search_knowledge"
    }
    fn description(&self) -> &str {
        "Semantic search over the knowledge graph (#3) for concepts, projects, people, technologies, decisions."
    }
    fn parameters(&self) -> Value {
        json_schema::<KnowledgeSearchArgs>()
    }
    async fn execute(&self, arguments: Value) -> Result<ToolOutput, ToolError> {
        let a: KnowledgeSearchArgs = parse_args("search_knowledge", arguments)?;
        if a.keyword.trim().is_empty() {
            return Err(ToolError::invalid(
                "search_knowledge",
                "`keyword` must not be empty",
            ));
        }
        let hits = self
            .memory
            .knowledge
            .search_semantic(&a.keyword, limit_or_default(a.limit))
            .map_err(|e| mem_err("search_knowledge", e))?;
        if hits.is_empty() {
            return Ok(ToolOutput::ok("No matching knowledge nodes."));
        }
        let out: Vec<Value> = hits
            .into_iter()
            .map(|(node, score)| json!({ "node": node_view(&node), "score": score }))
            .collect();
        Ok(to_output(&out))
    }
}

/// `search_archive` — keyword search over the #1 turn archive.
pub struct SearchArchiveTool {
    memory: Arc<Memory>,
}

#[async_trait]
impl Tool for SearchArchiveTool {
    fn name(&self) -> &str {
        "search_archive"
    }
    fn description(&self) -> &str {
        "Search the turn archive (#1) for past user/assistant conversation."
    }
    fn parameters(&self) -> Value {
        json_schema::<ArchiveSearchArgs>()
    }
    async fn execute(&self, arguments: Value) -> Result<ToolOutput, ToolError> {
        let a: ArchiveSearchArgs = parse_args("search_archive", arguments)?;
        if a.keyword.trim().is_empty() {
            return Err(ToolError::invalid(
                "search_archive",
                "`keyword` must not be empty",
            ));
        }
        let rows = self
            .memory
            .archive
            .search(&a.keyword, limit_or_default(a.limit))
            .map_err(|e| mem_err("search_archive", e))?;
        if rows.is_empty() {
            return Ok(ToolOutput::ok("No matching turns."));
        }
        let out: Vec<Value> = rows.iter().map(turn_view).collect();
        Ok(to_output(&out))
    }
}

/// `memory_stats` — counts across all four stores.
pub struct MemoryStatsTool {
    memory: Arc<Memory>,
}

#[async_trait]
impl Tool for MemoryStatsTool {
    fn name(&self) -> &str {
        "memory_stats"
    }
    fn description(&self) -> &str {
        "Exact record counts across all memory stores (observations / knowledge / archive / business)."
    }
    fn parameters(&self) -> Value {
        json_schema::<NoArgs>()
    }
    async fn execute(&self, _arguments: Value) -> Result<ToolOutput, ToolError> {
        let obs = self
            .memory
            .observations
            .stats()
            .map_err(|e| mem_err("memory_stats", e))?;
        let kg = self
            .memory
            .knowledge
            .stats()
            .map_err(|e| mem_err("memory_stats", e))?;
        let turns = self
            .memory
            .archive
            .count()
            .map_err(|e| mem_err("memory_stats", e))?;
        let tables = self
            .memory
            .business
            .tables()
            .map_err(|e| mem_err("memory_stats", e))?;
        Ok(to_output(&json!({
            "observations": obs,
            "knowledge_nodes": kg.nodes,
            "knowledge_edges": kg.edges,
            "archive_turns": turns,
            "business_tables": tables,
        })))
    }
}

/// `memory_kinds` — distinct observation + node kinds.
pub struct MemoryKindsTool {
    memory: Arc<Memory>,
}

#[async_trait]
impl Tool for MemoryKindsTool {
    fn name(&self) -> &str {
        "memory_kinds"
    }
    fn description(&self) -> &str {
        "List all distinct observation kinds and knowledge-graph node kinds."
    }
    fn parameters(&self) -> Value {
        json_schema::<NoArgs>()
    }
    async fn execute(&self, _arguments: Value) -> Result<ToolOutput, ToolError> {
        let obs = self
            .memory
            .observations
            .all_kinds()
            .map_err(|e| mem_err("memory_kinds", e))?;
        let kg = self
            .memory
            .knowledge
            .all_kinds()
            .map_err(|e| mem_err("memory_kinds", e))?;
        Ok(to_output(&json!({
            "observation_kinds": obs,
            "knowledge_kinds": kg,
        })))
    }
}

/// `list_observations` — paginate the observation log.
pub struct ListObservationsTool {
    memory: Arc<Memory>,
}

#[async_trait]
impl Tool for ListObservationsTool {
    fn name(&self) -> &str {
        "list_observations"
    }
    fn description(&self) -> &str {
        "List the newest observations (#2), newest first, with optional kind filter and pagination."
    }
    fn parameters(&self) -> Value {
        json_schema::<ListArgs>()
    }
    async fn execute(&self, arguments: Value) -> Result<ToolOutput, ToolError> {
        let a: ListArgs = parse_args("list_observations", arguments)?;
        let rows = self
            .memory
            .observations
            .list_all(limit_or_default(a.limit), a.offset, &a.kind)
            .map_err(|e| mem_err("list_observations", e))?;
        if rows.is_empty() {
            return Ok(ToolOutput::ok("No observations."));
        }
        let out: Vec<Value> = rows.iter().map(observation_view).collect();
        Ok(to_output(&out))
    }
}

/// `list_archive` — list the newest archived turns.
pub struct ListArchiveTool {
    memory: Arc<Memory>,
}

#[async_trait]
impl Tool for ListArchiveTool {
    fn name(&self) -> &str {
        "list_archive"
    }
    fn description(&self) -> &str {
        "List the newest archived turns (#1), newest first."
    }
    fn parameters(&self) -> Value {
        json_schema::<ListArgs>()
    }
    async fn execute(&self, arguments: Value) -> Result<ToolOutput, ToolError> {
        let a: ListArgs = parse_args("list_archive", arguments)?;
        let rows = self
            .memory
            .archive
            .recent(limit_or_default(a.limit))
            .map_err(|e| mem_err("list_archive", e))?;
        if rows.is_empty() {
            return Ok(ToolOutput::ok("No archived turns."));
        }
        let out: Vec<Value> = rows.iter().map(turn_view).collect();
        Ok(to_output(&out))
    }
}

// ── write tools ────────────────────────────────────────────────

/// The process-local list of observation ids written **this turn**, awaiting a
/// knowledge-graph node to attach to（D 项 2026-10-05，对齐原版
/// `tool_context.pending_obs_ids`）。
///
/// `update_memory` pushes each new id here; `update_knowledge` drains it into
/// the node's `obs_ids`. This is the link 原版 *intended* — the Python side had
/// `reset` + `pop` but **no producer**, so `nodes.obs_ids` stayed empty forever;
/// the Rust port hard-coded `"[]"`. Both halves are now wired.
///
/// Deliberately in-process (not persisted): a pending id is only meaningful
/// within the turn that produced it, and a crash mid-turn should not leave a
/// stale id to be attached to an unrelated later node.
#[derive(Default)]
pub struct PendingObsIds(Mutex<Vec<String>>);

impl PendingObsIds {
    /// Record a freshly written observation id.
    pub fn push(&self, id: String) {
        if let Ok(mut v) = self.0.lock() {
            v.push(id);
        }
    }

    /// Drain and return everything pending (the `_pop_pending_obs_ids` half).
    pub fn take(&self) -> Vec<String> {
        self.0
            .lock()
            .map(|mut v| std::mem::take(&mut *v))
            .unwrap_or_default()
    }
}

/// `update_memory` — append an observation (#2).
pub struct UpdateMemoryTool {
    memory: Arc<Memory>,
    pending: Arc<PendingObsIds>,
}

#[async_trait]
impl Tool for UpdateMemoryTool {
    fn name(&self) -> &str {
        "update_memory"
    }
    fn description(&self) -> &str {
        "Append an observation to memory (#2): a fact, preference, decision, constraint, or topic."
    }
    fn parameters(&self) -> Value {
        json_schema::<UpdateMemoryArgs>()
    }
    fn is_mutating(&self) -> bool {
        true
    }
    async fn execute(&self, arguments: Value) -> Result<ToolOutput, ToolError> {
        let a: UpdateMemoryArgs = parse_args("update_memory", arguments)?;
        if a.name.trim().is_empty() || a.content.trim().is_empty() {
            return Err(ToolError::invalid(
                "update_memory",
                "`name` and `content` must not be empty",
            ));
        }
        let topic = if a.topic.is_empty() {
            &a.kind
        } else {
            &a.topic
        };
        let new = NewObservation {
            kind: &a.kind,
            topic,
            name: &a.name,
            content: &a.content,
            keywords: &a.keywords,
            turn_id: "",
            source: "tool",
            stage: "工作阶段",
            topics: "[]",
        };
        let id = self
            .memory
            .observations
            .insert(&new)
            .map_err(|e| mem_err("update_memory", e))?;
        // D 项: remember this id so a following `update_knowledge` can attach it
        // to the node it describes (原版 `pending_obs_ids` 的写入端).
        self.pending.push(id.clone());
        Ok(ToolOutput::ok(format!("stored observation {id}")))
    }
}

/// `update_knowledge` — upsert a knowledge-graph node (#3).
pub struct UpdateKnowledgeTool {
    memory: Arc<Memory>,
    pending: Arc<PendingObsIds>,
}

#[async_trait]
impl Tool for UpdateKnowledgeTool {
    fn name(&self) -> &str {
        "update_knowledge"
    }
    fn description(&self) -> &str {
        "Upsert a knowledge-graph node (#3) — a project, person, technology, decision, or fact."
    }
    fn parameters(&self) -> Value {
        json_schema::<UpdateKnowledgeArgs>()
    }
    fn is_mutating(&self) -> bool {
        true
    }
    async fn execute(&self, arguments: Value) -> Result<ToolOutput, ToolError> {
        let a: UpdateKnowledgeArgs = parse_args("update_knowledge", arguments)?;
        if a.name.trim().is_empty() {
            return Err(ToolError::invalid(
                "update_knowledge",
                "`name` must not be empty",
            ));
        }
        let summary = if a.summary.is_empty() {
            a.content.chars().take(120).collect::<String>()
        } else {
            a.summary.clone()
        };
        // D 项: drain the observations written since the last node — they describe
        // this concept, so attach them (`obs_ids`). The store **merges** these
        // with whatever the node already carried (原版 extend 语义).
        let obs_ids = {
            let drained = self.pending.take();
            if drained.is_empty() {
                "[]".to_string()
            } else {
                serde_json::to_string(&drained).unwrap_or_else(|_| "[]".to_string())
            }
        };
        let new = NewNode {
            kind: &a.kind,
            name: &a.name,
            summary: &summary,
            content: &a.content,
            keywords: &a.keywords,
            topic: "",
            obs_ids: &obs_ids,
        };
        let id = self
            .memory
            .knowledge
            .upsert_node(&new)
            .map_err(|e| mem_err("update_knowledge", e))?;
        Ok(ToolOutput::ok(format!("upserted node {id}")))
    }
}

/// `link_entries` — create a cross-layer link between two memory entries（D 项
/// 2026-10-05 双向链接广义化）.
///
/// Decoupled from the writers on purpose: linking is a judgement call the model
/// makes about two *existing* records, not a side effect of writing them. One
/// call accepts several targets, so a fan-out (`obs-a` supports `node-1`,
/// `node-2`) is a single tool call.
pub struct LinkEntriesTool {
    memory: Arc<Memory>,
}

#[async_trait]
impl Tool for LinkEntriesTool {
    fn name(&self) -> &str {
        "link_entries"
    }
    fn description(&self) -> &str {
        "Create a cross-layer link between two memory entries (any of observations / knowledge / \
         archive / business). `from` is one endpoint, `to` may be several — each written as \
         `layer:id` (e.g. `observations:obs-1a2b`); a bare `obs-…` / `node-…` / `turn-…` id is \
         accepted and its layer inferred. Links are directional but recalled from both ends, so \
         use this to record that a decision rests on an observation, that a fact refines a node, \
         and so on."
    }
    fn parameters(&self) -> Value {
        json_schema::<LinkEntriesArgs>()
    }
    fn is_mutating(&self) -> bool {
        true
    }
    async fn execute(&self, arguments: Value) -> Result<ToolOutput, ToolError> {
        let a: LinkEntriesArgs = parse_args("link_entries", arguments)?;
        let (from_layer, from_id) = parse_endpoint(&a.from)?;
        if a.to.is_empty() {
            return Err(ToolError::invalid(
                "link_entries",
                "`to` must contain at least one endpoint",
            ));
        }
        let mut created: Vec<Value> = Vec::new();
        for target in &a.to {
            let (to_layer, to_id) = parse_endpoint(target)?;
            if from_layer == to_layer && from_id == to_id {
                return Err(ToolError::invalid(
                    "link_entries",
                    format!("cannot link `{}` to itself", a.from),
                ));
            }
            let rowid = self
                .memory
                .knowledge
                .add_link(&from_layer, &from_id, &to_layer, &to_id, &a.relation)
                .map_err(|e| mem_err("link_entries", e))?;
            created.push(json!({
                "id": rowid,
                "from": format!("{from_layer}:{from_id}"),
                "to": format!("{to_layer}:{to_id}"),
                "relation": a.relation,
            }));
        }
        Ok(to_output(&json!({
            "created": created.len(),
            "links": created,
        })))
    }
}

// ── aggregated search ──────────────────────────────────────────

/// `search_memory` — the aggregated, cross-layer memory search (原版对齐):
/// one call recalls from all three layers at once instead of three separate
/// tools. The C-stage system prompt promises `search_memory`, so it must exist.
#[derive(Deserialize, schemars::JsonSchema)]
struct SearchMemoryArgs {
    /// Primary search term (semantic + keyword).
    #[serde(default)]
    keyword: String,
    /// Alias for `keyword` — pass either (at least one required).
    #[serde(default)]
    query: String,
    /// Maximum merged results (default 10, capped at 20).
    #[serde(default)]
    limit: usize,
}

/// One merged hit from any memory layer.
///
/// `layer` is the plain layer name for the local search (`observations` /
/// `knowledge` / `archive`); the cross-project search prefixes the zone
/// (`chat/observations`, `main/knowledge`, …) so hits from different projects'
/// zones stay distinguishable.
#[derive(Clone)]
struct MemoryHit {
    layer: String,
    id: String,
    kind: String,
    title: String,
    snippet: String,
    /// **融合分** —— 六路召回经 RRF（[`rrf_fuse`]）后的排序依据，也是回给模型的
    /// `score`。不再是某一路的原始分（旧口径为「语义 cos 或关键词固定分 0.30」，
    /// 两者不同量纲、cos 恒压关键词分，是 knowledge 层被整层挤掉的根因）。
    score: f32,
    /// 该命中在本路召回里的原始分（语义 = cosine；关键词 = [`KEYWORD_SCORE`] 标记），
    /// 仅用于调试与断言，不参与排序。
    raw_score: f32,
    /// 召回路径标识（3 层 × 语义/关键词 = 六路）。RRF 按**路**分组算名次，所以
    /// 同一 `(layer,id)` 被多路召回时会有多条贡献、累加后排名上升。
    route: &'static str,
    /// 归一化正文指纹（压空白 / 去标点 / 小写）—— 同层**近似重复**判定用。
    /// 实测动因：本地库里 `kind=turn` 的观测有 23 条正文都是「你好」（同一句
    /// 寒暄被逐轮转写），它们的 MiniLM 向量**逐位相同**（cos = 1.0000），会把
    /// top-N 整段刷屏。指纹相同的同层命中只保留最高分那条。
    fingerprint: String,
}

/// 正文指纹：压掉空白与常见标点、统一小写。
///
/// 只做「同一句话的书写差异」归一（全角/半角标点、首尾空白、换行），不做
/// 语义近似 —— 语义近似需要向量，而 `search_*` 只回行不回向量。
fn fingerprint_of(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_whitespace() && !is_fingerprint_punct(*c))
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// 判定为「书写差异」的标点集合（指纹中丢弃）。
///
/// 用码点字面量而非字形字面量：弯引号有 ASCII / 全角 / 弯形三套写法，直接写
/// 字形容易在编辑中被规范化成同几个字符（编译器会报 unreachable pattern）。
fn is_fingerprint_punct(c: char) -> bool {
    matches!(
        c,
        '!' | '\u{FF01}'   // ！
            | '?'
            | '\u{FF1F}'   // ？
            | '.'
            | '\u{3002}'   // 。
            | ','
            | '\u{FF0C}'   // ，
            | ';'
            | '\u{FF1B}'   // ；
            | ':'
            | '\u{FF1A}'   // ：
            | '~'
            | '\u{FF5E}'   // ～
            | '\u{3001}'   // 、
            | '\u{201C}'   // “ 
            | '\u{201D}'   // ”
            | '\u{2018}'   // ‘
            | '\u{2019}'   // ’
            | '`'
    )
}

/// 构造一条合并命中（统一填指纹，六路召回共用）。
///
/// `fp_src` 是**指纹源文本**，通常就是 snippet；archive 例外（snippet 只是
/// assistant 回复，指纹要连 user_msg 一起算，否则「同一句回复、不同提问」会被
/// 误判成同一条）。
#[allow(clippy::too_many_arguments)]
fn make_hit(
    layer: &str,
    id: String,
    kind: &str,
    title: String,
    snippet: String,
    score: f32,
    fp_src: &str,
    route: &'static str,
) -> MemoryHit {
    let fingerprint = fingerprint_of(fp_src);
    MemoryHit {
        layer: layer.to_string(),
        id,
        kind: kind.to_string(),
        title,
        snippet,
        // 初始分 = 本路原始分；`rrf_fuse` 随后把它覆盖成融合分。
        score,
        raw_score: score,
        route,
        fingerprint,
    }
}

/// One candidate offered to a [`Reranker`] — the display fields the Python
/// `_llm_rerank_candidates` prompt needs (id + layer/kind/name/content/score).
pub struct RerankCandidate {
    /// Stable id (unique within its layer).
    pub id: String,
    /// Source layer (`observations` / `knowledge` / `archive`).
    pub layer: String,
    /// Record kind (`decision` / `fact` / `turn` / …).
    pub kind: String,
    /// Display name / title.
    pub title: String,
    /// Body snippet.
    pub content: String,
    /// Original cosine / keyword score.
    pub score: f32,
}

/// LLM-based memory re-ranker (原版对齐, `registration.py:17`
/// `_llm_rerank_candidates`).
///
/// `search_memory` runs this **optional** stage after dedup: it re-sorts the
/// candidates by relevance and drops the irrelevant ones. `lingmiao-tools` must not
/// depend on `lingmiao-llm`, so the actual LLM call sits behind this trait —
/// `lingmiao-engine` supplies the concrete `LlmReranker`.
#[async_trait]
pub trait Reranker: Send + Sync {
    /// Order `candidates` for `query`, best first. Returns indices into
    /// `candidates` (already deduped; may be topped up to `top_k`). `Err` → the
    /// tool falls back to plain score order.
    async fn rerank(
        &self,
        query: &str,
        candidates: &[RerankCandidate],
        top_k: usize,
    ) -> Result<Vec<usize>, String>;
}

/// The fixed score 原版 assigns to keyword-only (non-semantic) matches, so
/// they rank below genuine semantic hits but still beat nothing.
pub const KEYWORD_SCORE: f32 = 0.30;

/// Cap on a hit's snippet length (keeps the merged tool result readable).
pub const SNIPPET_CHARS: usize = 240;

/// The six recall routes `collect_hits` runs (3 layers × semantic / keyword).
/// RRF works on **per-route ranks**, so each hit remembers which route produced
/// it — a document recalled by both the semantic and the keyword pass
/// contributes twice and therefore climbs.
pub const ROUTE_OBS_SEMANTIC: &str = "observations.semantic";
/// Observations keyword pass.
pub const ROUTE_OBS_KEYWORD: &str = "observations.keyword";
/// Knowledge-graph semantic pass.
pub const ROUTE_KNOWLEDGE_SEMANTIC: &str = "knowledge.semantic";
/// Knowledge-graph keyword pass.
pub const ROUTE_KNOWLEDGE_KEYWORD: &str = "knowledge.keyword";
/// Archive semantic pass.
pub const ROUTE_ARCHIVE_SEMANTIC: &str = "archive.semantic";
/// Archive keyword pass.
pub const ROUTE_ARCHIVE_KEYWORD: &str = "archive.keyword";

/// RRF 平滑常数（Cormack et al. 2009 的 60）——名次越靠前贡献越大，但衰减平缓，
/// 让「多路都出现」比「单路第一」更有价值。
pub const RRF_K: f32 = 60.0;

/// Per-layer recall depth used by `search_memory` (需求⑤ `search`/`candidates`
/// view). Exposed so the meta platform reports the *actual* numbers instead of
/// a doc copy.
pub const RECALL_OBS_SEMANTIC: usize = 30;
/// Keyword-pass recall depth for observations.
pub const RECALL_OBS_KEYWORD: usize = 10;
/// Semantic recall depth for the knowledge graph.
pub const RECALL_KNOWLEDGE_SEMANTIC: usize = 30;
/// Keyword-pass recall depth for the knowledge graph (C 项 2026-10-05: the KG
/// carried `search_keyword` all along — `context.rs` used it for the Skills
/// section — but `collect_hits` never called it, so a node matching the term
/// verbatim could lose to a weaker cosine hit. 原版 runs both passes on every
/// layer).
pub const RECALL_KNOWLEDGE_KEYWORD: usize = 10;
/// Semantic recall depth for the archive (C 项 2026-10-05: `turns` has carried
/// an `embedding` column since ④真语义 — only the query method was missing, so
/// the archive was keyword-only).
pub const RECALL_ARCHIVE_SEMANTIC: usize = 30;
/// Keyword recall depth for the archive.
pub const RECALL_ARCHIVE_KEYWORD: usize = 10;

/// 同层**近似重复**合并开关：正文指纹相同的同层命中只保留最高分那条。
///
/// 实测动因（2026-09-30，`:99` 只读副本探针）：本地库 606 条观测里 233 条是
/// `kind=turn` 的逐轮转写，其中 **23 条正文就是「你好」**——MiniLM 对短中文串
/// 坍缩，这 23 条向量**逐位相同**（cos = 1.0000），任何一次语义召回都被它们
/// 占满 top-30，真实条目被挤出。指纹只归一「书写差异」（全角/半角标点、空白、
/// 大小写），不做语义近似。
pub const SAME_LAYER_DEDUP: bool = true;

/// **层配额** —— 每层在最终 top-N 里至少占据的条数（层无候选时不占位）。
///
/// 动因：三层合并后按分数全局排序时，observations 的语义分（cos≈0.66）恒高于
/// knowledge 的关键词固定分（[`KEYWORD_SCORE`]=0.30），`search_memory` top-10
/// 实测被 observations 整层刷满（knowledge 0 条）。保底条数保证「有候选的层」
/// 不会整层消失；层内与层间的实际排序仍交给 LLM 精排。
pub const PER_LAYER_QUOTA: usize = 2;

/// 送入 LLM 精排的候选池大小 = `min(limit × CANDIDATE_MULTIPLIER, MAX_CANDIDATES)`。
/// 层配额要求候选池比 `limit` 宽，否则每层保底会挤掉高分命中。
pub const CANDIDATE_MULTIPLIER: usize = 3;
/// 候选池硬上限（精排 prompt 的 token 预算兜底）。
pub const MAX_CANDIDATES: usize = 60;

// NOTE: the keyword rank field weights live in `lingmiao_memory::observations`
// (`FIELD_WEIGHT_NAME/TOPIC/CONTENT`) — the *single source of truth* consumed by
// `rank_by_relevance`. The 需求⑤ `meta_show search/ranking` view reports those,
// not a copy kept here, so the ranker and the platform can never drift.

fn preview(s: &str, max: usize) -> String {
    let mut out: String = s.chars().take(max).collect();
    if s.chars().count() > max {
        out.push('…');
    }
    out
}

/// Hybrid recall across observations (#2) + knowledge (#3) + archive (#1),
/// ignoring per-layer failures so one bad store cannot sink the whole search
/// (原版 collects `errors` likewise).
///
/// Each layer is optional: the local `search_memory` always supplies all three
/// (chat-zone stores), while the cross-project `search_external_memory` supplies
/// only the stores that actually exist in the probed zone (another project may
/// legitimately have no `knowledge.db` yet).
///
/// `semantic` gates the three **embedding-based** passes (observations /
/// knowledge / archive semantic recall). The lexical passes (observations /
/// knowledge / archive keyword) are always run. The cross-project search clears
/// it when the sibling store was written in a *different* vector space, so a
/// meaningless cosine can neither be computed nor pollute the ranking.
fn collect_hits(
    observations: Option<&Observations>,
    knowledge: Option<&KnowledgeGraph>,
    archive: Option<&Archive>,
    term: &str,
    semantic: bool,
    errors: &mut Vec<String>,
) -> Vec<MemoryHit> {
    let mut hits: Vec<MemoryHit> = Vec::new();

    if let Some(observations) = observations {
        // #2 observations — semantic recall, then keyword recall.
        if semantic {
            match observations.search_semantic(term, RECALL_OBS_SEMANTIC) {
                Ok(rows) => {
                    for s in rows {
                        let o = s.observation;
                        let body = preview(&o.content, SNIPPET_CHARS);
                        hits.push(make_hit(
                            "observations",
                            o.id,
                            &o.kind,
                            o.name,
                            body.clone(),
                            s.score,
                            &body,
                            ROUTE_OBS_SEMANTIC,
                        ));
                    }
                }
                Err(e) => errors.push(format!("observations.semantic: {e}")),
            }
        }
        match observations.search(term, RECALL_OBS_KEYWORD, "") {
            Ok(rows) => {
                for s in rows {
                    let o = s.observation;
                    let body = preview(&o.content, SNIPPET_CHARS);
                    hits.push(make_hit(
                        "observations",
                        o.id,
                        &o.kind,
                        o.name,
                        body.clone(),
                        KEYWORD_SCORE,
                        &body,
                        ROUTE_OBS_KEYWORD,
                    ));
                }
            }
            Err(e) => errors.push(format!("observations.keyword: {e}")),
        }
    }

    if let Some(knowledge) = knowledge {
        // #3 knowledge graph — semantic recall, then keyword recall（C 项
        // 2026-10-05: the keyword pass was the missing half — 原版 runs both）。
        if semantic {
            match knowledge.search_semantic(term, RECALL_KNOWLEDGE_SEMANTIC) {
                Ok(rows) => {
                    for (node, score) in rows {
                        let body = preview(&node.summary, SNIPPET_CHARS);
                        hits.push(make_hit(
                            "knowledge",
                            node.id,
                            &node.kind,
                            node.name,
                            body.clone(),
                            score,
                            &body,
                            ROUTE_KNOWLEDGE_SEMANTIC,
                        ));
                    }
                }
                Err(e) => errors.push(format!("knowledge.semantic: {e}")),
            }
        }
        match knowledge.search_keyword(term, RECALL_KNOWLEDGE_KEYWORD, "") {
            Ok(rows) => {
                for node in rows {
                    let body = preview(&node.summary, SNIPPET_CHARS);
                    hits.push(make_hit(
                        "knowledge",
                        node.id,
                        &node.kind,
                        node.name,
                        body.clone(),
                        KEYWORD_SCORE,
                        &body,
                        ROUTE_KNOWLEDGE_KEYWORD,
                    ));
                }
            }
            Err(e) => errors.push(format!("knowledge.keyword: {e}")),
        }
    }

    if let Some(archive) = archive {
        // #1 archive — semantic recall, then keyword recall（C 项 2026-10-05:
        // every turn has carried an embedding since ④真语义; only the query
        // method was missing, so the archive used to be keyword-only）.
        if semantic {
            match archive.search_semantic(term, RECALL_ARCHIVE_SEMANTIC) {
                Ok(rows) => {
                    for s in rows {
                        let t = s.turn;
                        let fp = format!("{}\u{1}{}", t.user_msg, t.assistant);
                        hits.push(make_hit(
                            "archive",
                            t.id,
                            "turn",
                            preview(&t.user_msg, 80),
                            preview(&t.assistant, SNIPPET_CHARS),
                            s.score,
                            &fp,
                            ROUTE_ARCHIVE_SEMANTIC,
                        ));
                    }
                }
                Err(e) => errors.push(format!("archive.semantic: {e}")),
            }
        }
        match archive.search(term, RECALL_ARCHIVE_KEYWORD) {
            Ok(rows) => {
                for t in rows {
                    let fp = format!("{}\u{1}{}", t.user_msg, t.assistant);
                    hits.push(make_hit(
                        "archive",
                        t.id,
                        "turn",
                        preview(&t.user_msg, 80),
                        preview(&t.assistant, SNIPPET_CHARS),
                        KEYWORD_SCORE,
                        &fp,
                        ROUTE_ARCHIVE_KEYWORD,
                    ));
                }
            }
            Err(e) => errors.push(format!("archive.keyword: {e}")),
        }
    }

    hits
}

/// **RRF 融合**（Reciprocal Rank Fusion，Cormack et al. 2009）—— 把六路召回从
/// 「分数拼接」换成「按名次求和」。
///
/// 旧口径把语义路的真实 cosine（≈0.4~0.9）与关键词路的固定分
/// [`KEYWORD_SCORE`]=0.30 直接放进同一个排序里比大小：两者**不同量纲**，cos 恒
/// 压关键词分，于是知识层/关键词命中被整层挤出 top-N（实测 2026-09-30：查「记忆
/// 合并」top-10 全 observations、knowledge 0 条）。RRF 只看**名次**，天然免疫量纲
/// 差异，且是 ES / Weaviate / Qdrant / Vespa 的内建做法。
///
/// 每条命中的融合分 = `Σ_路 1/(RRF_K + rank_路)`（`rank` 从 1 起，标准 RRF 不
/// 归一）。同一 `(layer,id)` 被多路召回时各路都贡献一份，因此「两路都命中」比
/// 「单路第一」更高 —— 这正是 hybrid search 想要的信号叠加。实测（2026-10-01 真库
/// 只读副本探针 `recall_algo_probe`）：top-10 内近似重复对 36~45 → 6~10。
///
/// 输入是**未去重**的原始六路命中（每路内部已按分降序），输出是**已按融合分
/// 降序**的、同一 `(layer,id)` 只留一条的命中。分数是「名次倒数之和」（单路首发
/// 约 0.0164，三路命中约 0.049），与旧的 cosine 不同量纲 —— 它只用于**排序**，
/// 不做绝对值比较。
fn rrf_fuse(hits: Vec<MemoryHit>) -> Vec<MemoryHit> {
    // ① 按 route 分组，组内名次 = 原序（每路召回本身已按分降序）。
    let mut per_route: std::collections::HashMap<&'static str, Vec<usize>> =
        std::collections::HashMap::new();
    for (i, h) in hits.iter().enumerate() {
        per_route.entry(h.route).or_default().push(i);
    }

    // ② 累计每条 (layer,id) 的 RRF 分；同一文档被多路召回会累加。代表条目取
    //    `raw_score` 最大的那条（通常来自语义路，信息量最大）。
    let mut fused: std::collections::HashMap<(String, String), (f32, MemoryHit)> =
        std::collections::HashMap::new();
    for idxs in per_route.values() {
        for (rank0, &i) in idxs.iter().enumerate() {
            let h = &hits[i];
            let contrib = 1.0 / (RRF_K + (rank0 + 1) as f32);
            let key = (h.layer.clone(), h.id.clone());
            fused
                .entry(key)
                .and_modify(|(sum, keep)| {
                    *sum += contrib;
                    if h.raw_score > keep.raw_score {
                        *keep = h.clone();
                    }
                })
                .or_insert_with(|| (contrib, h.clone()));
        }
    }

    // ③ 融合分为排序依据；分数相同时按 (layer,id) 稳定排序，避免 HashMap
    //    迭代序造成输出抖动。
    let mut out: Vec<MemoryHit> = fused
        .into_iter()
        .map(|(_, (sum, mut h))| {
            h.score = sum;
            h
        })
        .collect();
    out.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.layer.cmp(&b.layer))
            .then_with(|| a.id.cmp(&b.id))
    });
    out
}

/// RRF 融合 → 同层近似重复收敛（指纹相同只留最高分那条）。
///
/// Shared by the score-order path and the LLM re-rank path (a re-ranker only
/// reorders what is already deduped).
fn dedup_hits(hits: Vec<MemoryHit>) -> Vec<MemoryHit> {
    // ① RRF 融合：按名次求和（同一 (layer,id) 的多路贡献累加），已按融合分降序。
    let mut deduped = rrf_fuse(hits);
    // ② 同层近似重复：已按分降序，故首次出现的即该指纹的最高分那条。
    // 空指纹（正文全为标点/空白）不参与，避免把无信息条目一锅端。
    if SAME_LAYER_DEDUP {
        let mut seen_fp: std::collections::HashSet<(String, String)> =
            std::collections::HashSet::new();
        deduped.retain(|h| {
            h.fingerprint.is_empty() || seen_fp.insert((h.layer.clone(), h.fingerprint.clone()))
        });
    }
    deduped
}

/// 自适应层配额：`min(`[`PER_LAYER_QUOTA`]`, limit / 层数)`。
///
/// 上限由「保底总量 ≤ limit」反推 —— 否则层数多时（跨项目检索的
/// `zone/layer` 命名可达 9~12 层）保底会把高分命中整批挤掉。返回 `0` 表示
/// 该 `limit` 下容不下保底（层数 ≥ limit），此时退化为纯分数序。
fn per_layer_quota(limit: usize, layers: usize) -> usize {
    if layers == 0 || limit == 0 {
        return 0;
    }
    PER_LAYER_QUOTA.min(limit / layers)
}

/// **层配额**挑选：在已按分降序的 `hits` 里，先让每层保底
/// [`per_layer_quota`] 条，再按全局分数补齐到 `limit`，返回结果按分数降序。
///
/// 层配额解决「observations 的 cosine 恒高于 knowledge 的关键词固定分 →
/// knowledge 整层消失」；`limit` 内的实际排序仍以分数为纲，保底只保证**在集合
/// 里**（候选池够宽时不会挤压高分命中）。
fn apply_layer_quota(sorted: &[MemoryHit], limit: usize) -> Vec<MemoryHit> {
    let layers: std::collections::HashSet<&str> = sorted.iter().map(|h| h.layer.as_str()).collect();
    let quota = per_layer_quota(limit, layers.len());
    let mut out: Vec<MemoryHit> = Vec::with_capacity(limit.min(sorted.len()));
    let mut used: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
    let mut per_layer: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    // ① 每层保底（层无候选自然不占位；quota=0 时整段跳过）。
    for h in sorted {
        if out.len() >= limit || quota == 0 {
            break;
        }
        let cnt = per_layer.entry(h.layer.clone()).or_insert(0);
        if *cnt < quota {
            *cnt += 1;
            used.insert((h.layer.clone(), h.id.clone()));
            out.push(h.clone());
        }
    }
    // ② 按全局分数补齐。
    for h in sorted {
        if out.len() >= limit {
            break;
        }
        if used.insert((h.layer.clone(), h.id.clone())) {
            out.push(h.clone());
        }
    }
    out.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out
}

/// Dedup, rank by score descending, apply the per-layer quota, then truncate to
/// `limit`. Used where no LLM re-ranker is involved (the cross-project search,
/// and the fallback path).
fn rank_and_truncate(hits: Vec<MemoryHit>, limit: usize) -> Vec<MemoryHit> {
    let deduped = dedup_hits(hits);
    apply_layer_quota(&deduped, limit)
}

/// Re-order the candidate pool by an LLM's ranked indices, dropping unknown /
/// duplicate entries, then top up with the remaining score-ordered hits up to
/// `limit` (原版 `_llm_rerank_candidates` safety net — a short LLM ranking
/// never shrinks the result set below `limit`).
fn apply_rerank_order(pool: &[MemoryHit], ranked: &[usize], limit: usize) -> Vec<MemoryHit> {
    let mut out: Vec<MemoryHit> = Vec::new();
    let mut used: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
    for &idx in ranked {
        if let Some(h) = pool.get(idx)
            && used.insert((h.layer.clone(), h.id.clone()))
        {
            out.push(h.clone());
        }
    }
    if out.len() < limit {
        for h in pool {
            if out.len() >= limit {
                break;
            }
            if used.insert((h.layer.clone(), h.id.clone())) {
                out.push(h.clone());
            }
        }
    }
    out.truncate(limit);
    out
}

/// `search_memory` — hybrid search across observations (#2) + knowledge (#3) +
/// archive (#1), merged, deduped, then LLM-re-ranked (when a [`Reranker`] is
/// wired in) and truncated.
pub struct SearchMemoryTool {
    memory: Arc<Memory>,
    reranker: Option<Arc<dyn Reranker>>,
}

#[async_trait]
impl Tool for SearchMemoryTool {
    fn name(&self) -> &str {
        "search_memory"
    }
    fn description(&self) -> &str {
        "Primary memory search tool — pass the term in `keyword` OR `query` (at least one). \
         Unified hybrid search across ALL layers at once: #2 observations, #3 knowledge graph, \
         #1 archive (semantic + keyword recall, merged and deduped into one ranked top-N). \
         Use it to recall past decisions, facts, constraints, preferences, corrections, tasks, \
         and conversation. For single-layer access use search_observations / search_knowledge / \
         search_archive."
    }
    fn parameters(&self) -> Value {
        json_schema::<SearchMemoryArgs>()
    }
    async fn execute(&self, arguments: Value) -> Result<ToolOutput, ToolError> {
        let a: SearchMemoryArgs = parse_args("search_memory", arguments)?;
        let term = if a.keyword.trim().is_empty() {
            a.query.trim().to_string()
        } else {
            a.keyword.trim().to_string()
        };
        if term.is_empty() {
            return Err(ToolError::invalid(
                "search_memory",
                "`keyword` or `query` must not be empty",
            ));
        }
        let limit = if a.limit == 0 {
            DEFAULT_LIMIT
        } else {
            a.limit.min(20)
        };

        let mut errors: Vec<String> = Vec::new();
        let hits = collect_hits(
            Some(&self.memory.observations),
            Some(&self.memory.knowledge),
            Some(&self.memory.archive),
            &term,
            true,
            &mut errors,
        );
        // ① (layer,id) 去重 + 同层近似重复收敛 → 按分数降序。
        let deduped = dedup_hits(hits);
        // ② 候选池：层配额保证「有候选的层」都能进 LLM 视野（池比 limit 宽，
        //    否则每层保底会把高分命中挤出集合）。
        let pool_size = (limit * CANDIDATE_MULTIPLIER)
            .min(MAX_CANDIDATES)
            .max(limit);
        let pool = apply_layer_quota(&deduped, pool_size);

        // Phase 3 (原版 `registration.py:603`): LLM re-rank. 精排只在**候选池**
        // 内排序（LLM 可丢弃不相关项），结果截到 limit；无 reranker 或 LLM 失败
        // 时退回「层配额 + 分数序」。
        let deduped = match &self.reranker {
            Some(rr) if !pool.is_empty() => {
                let candidates: Vec<RerankCandidate> = pool
                    .iter()
                    .map(|h| RerankCandidate {
                        id: h.id.clone(),
                        layer: h.layer.clone(),
                        kind: h.kind.clone(),
                        title: h.title.clone(),
                        content: h.snippet.clone(),
                        score: h.score,
                    })
                    .collect();
                match rr.rerank(&term, &candidates, limit).await {
                    Ok(ranked) => apply_rerank_order(&pool, &ranked, limit),
                    Err(e) => {
                        errors.push(format!("llm rerank: {e}"));
                        apply_layer_quota(&pool, limit)
                    }
                }
            }
            _ => apply_layer_quota(&pool, limit),
        };

        if deduped.is_empty() {
            let detail = if errors.is_empty() {
                "no matches".to_string()
            } else {
                errors.join("; ")
            };
            return Ok(ToolOutput::ok(format!(
                "[search_memory] 全库搜索 '{term}' 无结果（{detail}）。\
                 建议：① 用更通用的关键词重试 ② memory_stats 看数据规模 \
                 ③ 新知识用 update_memory / update_knowledge 写入。"
            )));
        }

        let items: Vec<Value> = deduped
            .iter()
            .map(|h| {
                // D 项 ③: carry each hit's one-hop neighbours (both directions)
                // so the model can walk the link graph without a second call.
                let links: Vec<Value> = self
                    .memory
                    .knowledge
                    .links_touching(&h.layer, &h.id)
                    .map(|rows| {
                        rows.iter()
                            .filter_map(|l| {
                                l.other_end(&h.layer, &h.id).map(|(layer, id)| {
                                    json!({
                                        "layer": layer,
                                        "id": id,
                                        "relation": l.relation,
                                    })
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                json!({
                    "layer": h.layer,
                    "id": h.id,
                    "kind": h.kind,
                    "title": h.title,
                    "score": h.score,
                    "snippet": h.snippet,
                    "links": links,
                })
            })
            .collect();
        Ok(to_output(&json!({
            "term": term,
            "count": items.len(),
            "results": items,
        })))
    }
}

// ── cross-project search ───────────────────────────────────────

/// `search_external_memory` arguments (需求⑤ / lingmiao-parity cross-project
/// recall).
#[derive(Deserialize, schemars::JsonSchema)]
struct ExternalSearchArgs {
    /// Path to the other project's root — the directory that contains `.memory/`.
    /// Accepts `~`, an absolute path, or a path relative to the cwd. A bare name
    /// is resolved against the cwd / its parent / `~/ai`. Empty → list the
    /// sibling projects that are reachable.
    #[serde(default)]
    project_dir: String,
    /// Primary search term (semantic + keyword).
    #[serde(default)]
    keyword: String,
    /// Alias for `keyword` — pass either (at least one required).
    #[serde(default)]
    query: String,
    /// Maximum merged results (default 10, capped at 20).
    #[serde(default)]
    limit: usize,
    /// Comma list of zones to search: `chat`, `main`, `auditor`.
    /// Default: every zone present in the target project.
    #[serde(default)]
    zones: String,
}

/// Expand a leading `~` to `$HOME` (no other shell expansion).
fn expand_home(p: &str) -> Option<PathBuf> {
    if p == "~" {
        return std::env::var("HOME").ok().map(PathBuf::from);
    }
    if let Some(rest) = p.strip_prefix("~/") {
        return std::env::var("HOME")
            .ok()
            .map(|h| PathBuf::from(h).join(rest));
    }
    Some(PathBuf::from(p))
}

/// Directories scanned when resolving a bare project name / listing siblings.
fn discovery_parents() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        if let Some(p) = cwd.parent() {
            out.push(p.to_path_buf());
        }
        out.push(cwd);
    }
    if let Ok(home) = std::env::var("HOME") {
        out.push(PathBuf::from(home).join("ai"));
    }
    out
}

/// Resolve a `project_dir` argument to an existing directory.
///
/// Tries `~` / absolute / relative-to-cwd first, then — for a bare name with no
/// path separator — a discovery pass over the cwd, the cwd's parent and `~/ai`.
fn resolve_project_dir(input: &str) -> Option<PathBuf> {
    let t = input.trim();
    if t.is_empty() {
        return None;
    }
    if let Some(raw) = expand_home(t) {
        let cand = if raw.is_absolute() {
            raw
        } else {
            std::env::current_dir().ok()?.join(raw)
        };
        if cand.is_dir() {
            return Some(cand);
        }
    }
    if !t.contains('/') && !t.contains('\\') {
        for parent in discovery_parents() {
            let cand = parent.join(t);
            if cand.is_dir() {
                return Some(cand);
            }
        }
    }
    None
}

/// Projects discoverable next to this one: a sibling directory that already
/// holds a `.memory/`. Powers the `project_dir`-less "what can I search?" case.
fn discover_projects(limit: usize) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for parent in discovery_parents() {
        let Ok(rd) = std::fs::read_dir(&parent) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if !p.is_dir() || !p.join(brand::MEMORY_DIR).is_dir() {
                continue;
            }
            if seen.insert(p.display().to_string()) {
                out.push(p);
                if out.len() >= limit {
                    return out;
                }
            }
        }
    }
    out
}

/// Parse a zone list (`chat,main,auditor`); unknown entries are ignored and an
/// empty/unusable result falls back to every zone.
fn parse_zones(spec: &str) -> Vec<Zone> {
    if spec.trim().is_empty() {
        return Zone::ALL.to_vec();
    }
    let parsed: Vec<Zone> = spec
        .split(',')
        .filter_map(|z| match z.trim().to_ascii_lowercase().as_str() {
            "chat" => Some(Zone::Chat),
            "main" => Some(Zone::Main),
            "auditor" => Some(Zone::Auditor),
            _ => None,
        })
        .collect();
    if parsed.is_empty() {
        Zone::ALL.to_vec()
    } else {
        parsed
    }
}

/// Open whichever of a zone's three searchable stores exist, **read-only** (no
/// schema DDL, no migration) so probing another project's memory cannot mutate
/// it. A missing DB is not an error — a sibling project may have written only
/// some of the stores; the search simply skips the absent layers.
fn open_zone_readonly(
    zone_dir: &Path,
    embedder: Option<Arc<dyn lingmiao_memory::Embedder>>,
) -> (
    Option<Observations>,
    Option<KnowledgeGraph>,
    Option<Archive>,
) {
    (
        Observations::open_in_dir_readonly(zone_dir, embedder.clone()).ok(),
        KnowledgeGraph::open_in_dir_readonly(zone_dir, embedder.clone()).ok(),
        Archive::open_in_dir_readonly(zone_dir, embedder).ok(),
    )
}

/// `search_external_memory` — recall from **another project's** memory store.
pub struct SearchExternalMemoryTool {
    memory: Arc<Memory>,
}

#[async_trait]
impl Tool for SearchExternalMemoryTool {
    fn name(&self) -> &str {
        "search_external_memory"
    }
    fn description(&self) -> &str {
        "Search ANOTHER project's memory store (cross-project). Pass the other project's \
         `project_dir` (a directory containing `.memory/`; `~`, absolute and relative paths \
         work, a bare name is discovered next to this project) plus a term in `keyword` or \
         `query`. Hybrid-recalls observations (#2) + knowledge (#3) + archive (#1) across that \
         project's chat/main/auditor zones, read-only, merged into one ranked top-N. Call with \
         `project_dir` empty to list the sibling projects it can reach."
    }
    fn parameters(&self) -> Value {
        json_schema::<ExternalSearchArgs>()
    }
    async fn execute(&self, arguments: Value) -> Result<ToolOutput, ToolError> {
        let a: ExternalSearchArgs = parse_args("search_external_memory", arguments)?;

        // No target → report what is discoverable next to this project.
        if a.project_dir.trim().is_empty() {
            let found = discover_projects(20);
            if found.is_empty() {
                return Ok(ToolOutput::ok(
                    "[search_external_memory] 未在 cwd 父目录 / `~/ai` 发现含 `.memory/` 的项目。\
                     传入 `project_dir=<路径>` 直接指定另一个项目。"
                        .to_string(),
                ));
            }
            let list: Vec<String> = found.iter().map(|p| p.display().to_string()).collect();
            return Ok(to_output(&json!({
                "discovered_projects": list,
                "note": "传入 project_dir=<上述路径> 进行跨项目检索",
            })));
        }

        let term = if a.keyword.trim().is_empty() {
            a.query.trim().to_string()
        } else {
            a.keyword.trim().to_string()
        };
        if term.is_empty() {
            return Err(ToolError::invalid(
                "search_external_memory",
                "`keyword` or `query` must not be empty (or pass an empty `project_dir` to list projects)",
            ));
        }
        let limit = if a.limit == 0 {
            DEFAULT_LIMIT
        } else {
            a.limit.min(20)
        };

        let Some(root) = resolve_project_dir(&a.project_dir) else {
            return Ok(ToolOutput::ok(format!(
                "[search_external_memory] 找不到项目 `{}`。可用 `~`/绝对/相对路径；\
                 或留空 project_dir 列出可检索的兄弟项目。",
                a.project_dir
            )));
        };
        let mem_dir = root.join(brand::MEMORY_DIR);
        if !mem_dir.is_dir() {
            return Ok(ToolOutput::ok(format!(
                "[search_external_memory] `{}` 不是含 `.memory/` 的项目根（期望 {} 存在）。",
                root.display(),
                mem_dir.display()
            )));
        }

        let zones = parse_zones(&a.zones);
        // Reuse this process's embedder so the query lands in the same vector
        // space the sibling store was written with (keyword recall still works
        // when the bundle has no embedder).
        let embedder = self.memory.embedder();
        let current_backend = embedder.as_ref().map(|e| e.backend());

        let mut errors: Vec<String> = Vec::new();
        let mut hits: Vec<MemoryHit> = Vec::new();
        let mut searched: Vec<&str> = Vec::new();
        let mut space_warnings: Vec<String> = Vec::new();
        // Whether *any* zone actually ran the semantic/KG passes. Reported as
        // `semantic_recall` — it must be `false` whenever embedding recall could
        // not run, not merely when a cross-space mismatch was seen.
        let mut semantic_used = false;
        for zone in zones {
            let zdir = zone.dir(&mem_dir);
            if !zdir.is_dir() {
                continue;
            }
            let (obs, kg, arc) = open_zone_readonly(&zdir, embedder.clone());
            if obs.is_none() && kg.is_none() && arc.is_none() {
                continue; // zone dir exists but holds no searchable store
            }
            // Embedder-space guard: a sibling whose *recorded* backend differs
            // from ours stored its vectors in a different space, so a cosine
            // against our query is meaningless. Detect it from the live DB and
            // degrade this zone to keyword-only recall instead of ranking noise.
            let recorded = obs
                .as_ref()
                .and_then(|o| o.recorded_embedder_backend())
                .or_else(|| kg.as_ref().and_then(|k| k.recorded_embedder_backend()));
            // Semantic recall also needs *this* process to have an embedder at
            // all: with `embedder = None` there is nothing to embed the query
            // with, so the semantic/KG passes return nothing. Claiming
            // `semantic_recall: true` there was the "embedder=None 误报" blemish.
            let mut semantic = current_backend.is_some();
            if let (Some(cur), Some(rec)) = (current_backend, recorded.as_deref()) {
                if cur != rec {
                    semantic = false;
                    space_warnings
                        .push(format!("{}: 记录后端 `{rec}` ≠ 当前 `{cur}`", zone.name()));
                }
            }
            semantic_used |= semantic;
            let mut zone_hits = collect_hits(
                obs.as_ref(),
                kg.as_ref(),
                arc.as_ref(),
                &term,
                semantic,
                &mut errors,
            );
            for h in &mut zone_hits {
                h.layer = format!("{}/{}", zone.name(), h.layer);
            }
            searched.push(zone.name());
            hits.extend(zone_hits);
        }

        if hits.is_empty() {
            let detail = if errors.is_empty() {
                "无匹配".to_string()
            } else {
                errors.join("; ")
            };
            return Ok(ToolOutput::ok(format!(
                "[search_external_memory] 在 `{}` 搜索 '{term}' 无结果（{detail}）。已检索 zone：{}。",
                root.display(),
                if searched.is_empty() {
                    "无（目录不存在）".to_string()
                } else {
                    searched.join(", ")
                }
            )));
        }

        let deduped = rank_and_truncate(hits, limit);
        let items: Vec<Value> = deduped
            .iter()
            .map(|h| {
                json!({
                    "layer": h.layer,
                    "id": h.id,
                    "kind": h.kind,
                    "title": h.title,
                    "score": h.score,
                    "snippet": h.snippet,
                })
            })
            .collect();
        let mut note = "只读检索：目标项目记忆未被写入或迁移".to_string();
        if current_backend.is_none() {
            note.push_str(
                "；本进程无嵌入后端（embedder=none）→ 语义/KG 召回无查询向量，\
                 已跳过、仅返回关键词命中",
            );
        }
        if !space_warnings.is_empty() {
            note.push_str(
                "；⚠️ 目标 store 记录的嵌入后端与本进程不同 → 向量空间不一致，\
                 已跳过语义/KG 召回（cosine 会失真），仅返回关键词命中",
            );
        }
        Ok(to_output(&json!({
            "project": root.display().to_string(),
            "term": term,
            "zones": searched,
            "current_embedder": current_backend.unwrap_or("none"),
            "semantic_recall": semantic_used,
            "space_warning": if space_warnings.is_empty() {
                Value::Null
            } else {
                json!(space_warnings)
            },
            "count": items.len(),
            "results": items,
            "note": note,
        })))
    }
}

/// Names of the tools in this group.
pub const TOOL_NAMES: [&str; 12] = [
    "search_memory",
    "search_observations",
    "search_knowledge",
    "search_archive",
    "search_external_memory",
    "memory_stats",
    "memory_kinds",
    "list_observations",
    "list_archive",
    "update_memory",
    "update_knowledge",
    "link_entries",
];

/// No-argument tools (`memory_stats` / `memory_kinds`).
#[derive(Deserialize, schemars::JsonSchema)]
struct NoArgs {}

/// Register every memory tool against one shared [`Memory`] handle.
///
/// `reranker` is the optional LLM re-ranker wired into `search_memory`
/// (`None` → pure score order, fine for tests and embedder-less runs).
pub fn register(
    registry: &mut ToolRegistry,
    memory: Arc<Memory>,
    reranker: Option<Arc<dyn Reranker>>,
) {
    registry.register(SearchMemoryTool {
        memory: memory.clone(),
        reranker,
    });
    registry.register(SearchObservationsTool {
        memory: memory.clone(),
    });
    registry.register(SearchKnowledgeTool {
        memory: memory.clone(),
    });
    registry.register(SearchArchiveTool {
        memory: memory.clone(),
    });
    registry.register(SearchExternalMemoryTool {
        memory: memory.clone(),
    });
    registry.register(MemoryStatsTool {
        memory: memory.clone(),
    });
    registry.register(MemoryKindsTool {
        memory: memory.clone(),
    });
    registry.register(ListObservationsTool {
        memory: memory.clone(),
    });
    registry.register(ListArchiveTool {
        memory: memory.clone(),
    });
    // `update_memory` → `update_knowledge` share one pending-observation log so
    // the node a turn writes can attach the observations that motivated it
    // (D 项 2026-10-05). `link_entries` is the explicit cross-layer writer.
    let pending = Arc::new(PendingObsIds::default());
    registry.register(UpdateMemoryTool {
        memory: memory.clone(),
        pending: pending.clone(),
    });
    registry.register(UpdateKnowledgeTool {
        memory: memory.clone(),
        pending,
    });
    registry.register(LinkEntriesTool { memory });
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingmiao_memory::{Embedder, HashingEmbedder, Zone};

    fn temp_memory(tag: &str) -> (Arc<Memory>, std::path::PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("lingmiao-memtools-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mem = Memory::open_in_dir(&dir, Zone::Chat, Some(Arc::new(HashingEmbedder::new())))
            .expect("open memory");
        (Arc::new(mem), dir)
    }

    #[tokio::test]
    async fn update_then_search_roundtrip() {
        let (mem, dir) = temp_memory("rt");
        let mut reg = ToolRegistry::new();
        register(&mut reg, mem, None);
        reg.execute(
            "update_memory",
            json!({"kind": "fact", "name": "x", "content": "rust is fast"}),
        )
        .await
        .unwrap();
        let out = reg
            .execute("search_observations", json!({"keyword": "rust"}))
            .await
            .unwrap();
        assert!(out.content.contains("rust is fast"));
        let stats = reg.execute("memory_stats", json!({})).await.unwrap();
        assert!(stats.content.contains("\"observations\": 1"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn empty_keyword_is_rejected() {
        let (mem, dir) = temp_memory("empty");
        let mut reg = ToolRegistry::new();
        register(&mut reg, mem, None);
        let err = reg
            .execute("search_observations", json!({"keyword": "  "}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A turn whose stored extras are megabytes, as real turns are
    /// (`full_messages` / `tool_calls` / `reasoning` each carry a whole tool
    /// transcript — one local row held 1.4M + 2.0M characters).
    fn fat_turn(id: &str, at: &str) -> Turn {
        Turn {
            id: id.to_string(),
            at: at.to_string(),
            user_msg: "x".repeat(5_000),
            assistant: "y".repeat(5_000),
            system_prompt: "s".repeat(120_000),
            context_prefix: "c".repeat(120_000),
            full_messages: "f".repeat(500_000),
            tool_calls: "t".repeat(1_400_000),
            tokens_in: 1,
            tokens_out: 2,
            tokens_total: 3,
            chain_id: "chain-1".to_string(),
            chain_seq: 1,
            reasoning: "r".repeat(2_000_000),
            summary: "sum".to_string(),
        }
    }

    #[tokio::test]
    async fn archive_projections_stay_bounded() {
        // cli 2026-09-28 (screenshot 055716): `list_archive(limit=6)` serialised
        // whole rows, so the 沉淀阶段 request reached 1,622,701 tokens → HTTP 400
        // [上下文超限]. The archive row is the *one* store whose columns are
        // megabytes, so both archive tools must project instead of dumping.
        let (mem, dir) = temp_memory("archive-cap");
        for i in 0..6 {
            mem.archive
                .save(&fat_turn(&format!("turn-fat-{i}"), "2026-09-28T05:52:07Z"))
                .unwrap();
        }
        let mut reg = ToolRegistry::new();
        register(&mut reg, mem, None);

        let listed = reg
            .execute("list_archive", json!({"limit": 6}))
            .await
            .unwrap();
        // Whole rows would be 6 × ~4.2 MB ≈ 25 MB; the projection must be tiny.
        assert!(
            listed.content.chars().count() < 20_000,
            "list_archive still bloated: {} chars",
            listed.content.chars().count()
        );
        // Still useful: the id survives so the model can name a turn.
        assert!(listed.content.contains("turn-fat-0"), "{}", listed.content);
        for leaked in ["system_prompt", "full_messages", "tool_calls", "reasoning"] {
            assert!(
                !listed.content.contains(leaked),
                "list_archive leaked `{leaked}`"
            );
        }

        let searched = reg
            .execute("search_archive", json!({"keyword": "xxx"}))
            .await
            .unwrap();
        assert!(
            searched.content.chars().count() < 20_000,
            "search_archive still bloated: {} chars",
            searched.content.chars().count()
        );
        assert!(
            searched.content.contains("turn-fat-0"),
            "{}",
            searched.content
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn memory_projections_keep_ids_and_bound_bodies() {
        // Projections must stay useful: the id survives, and a body longer than
        // the cap is marked as cut rather than silently shortened.
        let (mem, dir) = temp_memory("proj");
        mem.observations
            .insert(&NewObservation::new(
                "fact",
                "big",
                &"z".repeat(RECORD_BODY_CHARS * 4),
            ))
            .unwrap();
        mem.knowledge
            .upsert_node(&NewNode {
                kind: "fact",
                name: "big-node",
                summary: "short summary",
                content: &"w".repeat(RECORD_BODY_CHARS * 4),
                keywords: "k",
                topic: "",
                obs_ids: "[]",
            })
            .unwrap();
        let mut reg = ToolRegistry::new();
        register(&mut reg, mem, None);

        let obs = reg
            .execute("list_observations", json!({"limit": 5}))
            .await
            .unwrap();
        let parsed: Value = serde_json::from_str(&obs.content).expect("JSON");
        let first = &parsed[0];
        assert!(
            first["id"].as_str().is_some_and(|s| s.starts_with("obs-")),
            "id kept: {first}"
        );
        let body = first["content"].as_str().unwrap();
        assert!(
            body.chars().count() <= RECORD_BODY_CHARS + 1,
            "body bounded: {}",
            body.chars().count()
        );
        assert!(body.ends_with('…'), "cut body is marked: {:?}", &body[..20]);

        let kg = reg
            .execute("search_knowledge", json!({"keyword": "big-node"}))
            .await
            .unwrap();
        assert!(kg.content.contains("big-node"), "{}", kg.content);
        let kgj: Value = serde_json::from_str(&kg.content).expect("JSON");
        let content = kgj[0]["node"]["content"].as_str().unwrap();
        assert!(
            content.chars().count() <= RECORD_BODY_CHARS + 1,
            "node content bounded: {}",
            content.chars().count()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn search_memory_merges_layers() {
        // The aggregated tool must surface hits from *all* layers in one call
        // (原版对齐): an observation and a knowledge node both tagged.
        let (mem, dir) = temp_memory("agg");
        mem.observations
            .insert(&NewObservation::new(
                "decision",
                "picked rust",
                "we picked rust for the rewrite",
            ))
            .unwrap();
        mem.knowledge
            .upsert_node(&NewNode {
                kind: "technology",
                name: "rust",
                summary: "systems language",
                content: "rust is a systems language",
                keywords: "",
                topic: "",
                obs_ids: "[]",
            })
            .unwrap();
        let mut reg = ToolRegistry::new();
        register(&mut reg, mem, None);
        let out = reg
            .execute("search_memory", json!({"keyword": "rust"}))
            .await
            .unwrap();
        assert!(
            out.content.contains("observations"),
            "missing observations layer: {}",
            out.content
        );
        assert!(
            out.content.contains("knowledge"),
            "missing knowledge layer: {}",
            out.content
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn search_memory_accepts_query_alias_and_rejects_empty() {
        let (mem, dir) = temp_memory("alias");
        let mut reg = ToolRegistry::new();
        register(&mut reg, mem, None);
        // `query` is accepted as an alias for `keyword`.
        let out = reg
            .execute("search_memory", json!({"query": "nothing-here"}))
            .await
            .unwrap();
        assert!(out.content.contains("无结果"), "{}", out.content);
        // Neither provided → invalid args.
        let err = reg.execute("search_memory", json!({})).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Build a sibling project rooted at `root` with a `.memory/` holding one
    /// observation (chat zone) and one knowledge node (main zone).
    fn seed_external_project(root: &std::path::Path) {
        let ext_mem = root.join(".memory");
        let emb: Arc<dyn lingmiao_memory::Embedder> = Arc::new(HashingEmbedder::new());
        let chat = Memory::open_in_dir(&ext_mem, Zone::Chat, Some(emb.clone())).unwrap();
        chat.observations
            .insert(&NewObservation::new(
                "decision",
                "zephyr cache",
                "the sibling project chose zephyr for caching",
            ))
            .unwrap();
        let main = Memory::open_in_dir(&ext_mem.join("main"), Zone::Main, Some(emb)).unwrap();
        main.knowledge
            .upsert_node(&NewNode::new(
                "project",
                "zephyr",
                "zephyr caching layer",
                "zephyr is a caching layer",
            ))
            .unwrap();
    }

    #[tokio::test]
    async fn cross_project_search_reads_a_siblings_zones() {
        let ext_root = std::env::temp_dir().join(format!("lingmiao-ext-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&ext_root);
        std::fs::create_dir_all(&ext_root).unwrap();
        seed_external_project(&ext_root);

        let (mem, dir) = temp_memory("ext");
        let mut reg = ToolRegistry::new();
        register(&mut reg, mem, None);
        let out = reg
            .execute(
                "search_external_memory",
                json!({"project_dir": ext_root.display().to_string(), "keyword": "zephyr"}),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("chat/observations"), "{}", out.content);
        assert!(out.content.contains("main/knowledge"), "{}", out.content);

        // A zone filter narrows the sweep to just the requested zone.
        let only_main = reg
            .execute(
                "search_external_memory",
                json!({
                    "project_dir": ext_root.display().to_string(),
                    "keyword": "zephyr",
                    "zones": "main",
                }),
            )
            .await
            .unwrap();
        assert!(
            only_main.content.contains("main/knowledge"),
            "{}",
            only_main.content
        );
        assert!(
            !only_main.content.contains("chat/observations"),
            "{}",
            only_main.content
        );

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&ext_root).ok();
    }

    #[tokio::test]
    async fn cross_project_search_handles_missing_and_empty_targets() {
        let (mem, dir) = temp_memory("ext-miss");
        let mut reg = ToolRegistry::new();
        register(&mut reg, mem, None);

        // Unknown path → guidance, not an error.
        let missing = reg
            .execute(
                "search_external_memory",
                json!({"project_dir": "/no/such/project/here", "keyword": "x"}),
            )
            .await
            .unwrap();
        assert!(
            missing.content.contains("找不到项目"),
            "{}",
            missing.content
        );

        // Empty project_dir → discovery listing (never an error).
        let listing = reg
            .execute("search_external_memory", json!({}))
            .await
            .unwrap();
        assert!(
            listing.content.contains("discovered_projects") || listing.content.contains("未在"),
            "{}",
            listing.content
        );

        // Path present but no term → invalid args.
        let err = reg
            .execute(
                "search_external_memory",
                json!({"project_dir": dir.display().to_string()}),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn cross_project_search_tolerates_a_partial_zone() {
        // A sibling that has only an observations.db (no knowledge.db / archive)
        // must still be searchable — a missing sibling store is not an error and
        // must not sink the whole zone (bug found in the Xvfb black-box run).
        let root =
            std::env::temp_dir().join(format!("lingmiao-ext-partial-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let ext_mem = root.join(".memory");
        std::fs::create_dir_all(&ext_mem).unwrap();
        let obs =
            Observations::open_in_dir(&ext_mem, Some(Arc::new(HashingEmbedder::new()))).unwrap();
        obs.insert(&NewObservation::new(
            "fact",
            "quokka",
            "quokka runs the partial store",
        ))
        .unwrap();
        drop(obs);

        let (mem, dir) = temp_memory("ext-partial");
        let mut reg = ToolRegistry::new();
        register(&mut reg, mem, None);
        let out = reg
            .execute(
                "search_external_memory",
                json!({"project_dir": root.display().to_string(), "keyword": "quokka"}),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("quokka"), "{}", out.content);
        assert!(out.content.contains("chat/observations"), "{}", out.content);

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&root).ok();
    }

    /// A re-ranker that returns a fixed index order (or a fixed error) so the
    /// tool's re-rank wiring can be exercised without an LLM.
    struct FakeReranker {
        order: Vec<usize>,
        fail: bool,
    }

    #[async_trait]
    impl Reranker for FakeReranker {
        async fn rerank(
            &self,
            _query: &str,
            _candidates: &[RerankCandidate],
            _top_k: usize,
        ) -> Result<Vec<usize>, String> {
            if self.fail {
                Err("boom".to_string())
            } else {
                Ok(self.order.clone())
            }
        }
    }

    /// Insert three observations that all match `alpha`; return their ids.
    fn seed_three(mem: &Memory) {
        for (name, body) in [
            ("first", "alpha one about rust"),
            ("second", "alpha two about cargo"),
            ("third", "alpha three about traits"),
        ] {
            mem.observations
                .insert(&NewObservation::new("fact", name, body))
                .unwrap();
        }
    }

    fn result_ids(content: &str) -> Vec<String> {
        let v: Value = serde_json::from_str(content).expect("search_memory output is JSON");
        v["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["id"].as_str().unwrap().to_string())
            .collect()
    }

    #[tokio::test]
    async fn search_memory_applies_the_reranker_order() {
        let (mem, dir) = temp_memory("rerank-order");
        seed_three(&mem);
        // Baseline: no re-ranker → plain score order.
        let mut plain = ToolRegistry::new();
        register(&mut plain, mem.clone(), None);
        let base = plain
            .execute("search_memory", json!({"keyword": "alpha", "limit": 3}))
            .await
            .unwrap();
        let base_ids = result_ids(&base.content);
        assert_eq!(base_ids.len(), 3, "{}", base.content);

        // A re-ranker that reverses the candidate order must reverse the output.
        struct Reverse;
        #[async_trait]
        impl Reranker for Reverse {
            async fn rerank(
                &self,
                _q: &str,
                c: &[RerankCandidate],
                _k: usize,
            ) -> Result<Vec<usize>, String> {
                Ok((0..c.len()).rev().collect())
            }
        }
        let mut re = ToolRegistry::new();
        register(&mut re, mem.clone(), Some(Arc::new(Reverse)));
        let out = re
            .execute("search_memory", json!({"keyword": "alpha", "limit": 3}))
            .await
            .unwrap();
        let mut reversed = base_ids.clone();
        reversed.reverse();
        assert_eq!(result_ids(&out.content), reversed, "{}", out.content);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn search_memory_falls_back_to_score_order_when_the_reranker_errors() {
        let (mem, dir) = temp_memory("rerank-fail");
        seed_three(&mem);
        let mut plain = ToolRegistry::new();
        register(&mut plain, mem.clone(), None);
        let base = plain
            .execute("search_memory", json!({"keyword": "alpha", "limit": 3}))
            .await
            .unwrap();

        let mut re = ToolRegistry::new();
        register(
            &mut re,
            mem.clone(),
            Some(Arc::new(FakeReranker {
                order: vec![],
                fail: true,
            })),
        );
        let out = re
            .execute("search_memory", json!({"keyword": "alpha", "limit": 3}))
            .await
            .unwrap();
        // Re-rank failed → identical to the plain score order (no shrink, no error).
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(result_ids(&out.content), result_ids(&base.content));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn apply_rerank_order_dedups_and_tops_up() {
        let mk = |layer: &str, id: &str, score: f32| MemoryHit {
            layer: layer.to_string(),
            id: id.to_string(),
            kind: "fact".to_string(),
            title: id.to_string(),
            snippet: String::new(),
            score,
            raw_score: score,
            route: ROUTE_OBS_SEMANTIC,
            fingerprint: id.to_string(),
        };
        let deduped = vec![mk("observations", "a", 0.9), mk("observations", "b", 0.8)];
        // Out-of-range + duplicate indices dropped, remaining topped up in order.
        let out = apply_rerank_order(&deduped, &[1, 1, 7], 2);
        assert_eq!(
            out.iter().map(|h| h.id.as_str()).collect::<Vec<_>>(),
            ["b", "a"]
        );
    }

    /// cli 2026-09-30「层配额也做上」：三层合并后按分数全局排序时，
    /// observations 的 cosine（≈0.66）恒高于 knowledge 的关键词固定分
    /// （0.30），top-N 会被 observations 整层刷满、knowledge 一条不剩。
    /// `apply_layer_quota` 必须让「有候选的层」都有保底位。
    #[test]
    fn layer_quota_keeps_every_layer_with_candidates() {
        let mk = |layer: &str, id: &str, score: f32| MemoryHit {
            layer: layer.to_string(),
            id: id.to_string(),
            kind: "fact".to_string(),
            title: id.to_string(),
            snippet: String::new(),
            score,
            raw_score: score,
            route: ROUTE_OBS_SEMANTIC,
            fingerprint: id.to_string(),
        };
        // 5 条 observations 高分 + 1 条 knowledge / 1 条 archive 低分。
        let sorted = vec![
            mk("observations", "o1", 0.90),
            mk("observations", "o2", 0.88),
            mk("observations", "o3", 0.86),
            mk("observations", "o4", 0.84),
            mk("observations", "o5", 0.82),
            mk("knowledge", "k1", 0.30),
            mk("archive", "a1", 0.30),
        ];
        let out = apply_layer_quota(&sorted, 5);
        assert_eq!(out.len(), 5);
        let layers: std::collections::BTreeSet<&str> =
            out.iter().map(|h| h.layer.as_str()).collect();
        assert!(
            layers.contains("knowledge") && layers.contains("archive"),
            "层配额没保住 knowledge/archive: {layers:?}"
        );
        // 结果仍按分数降序。
        for w in out.windows(2) {
            assert!(w[0].score >= w[1].score, "未按分数降序: {:?}", out.len());
        }
    }

    /// 自适应配额：层数多到保底装不下时，退化到 0（纯分数序），
    /// 不让「保底」把高分命中挤掉（跨项目检索最多 12 层 × 2 = 24 > limit）。
    #[test]
    fn layer_quota_shrinks_when_layers_outnumber_limit() {
        assert_eq!(super::per_layer_quota(10, 3), 2);
        assert_eq!(super::per_layer_quota(10, 12), 0);
        assert_eq!(super::per_layer_quota(5, 3), 1);
        assert_eq!(super::per_layer_quota(2, 3), 0);
        assert_eq!(super::per_layer_quota(0, 3), 0);
    }

    /// 「有候选的层才占位」：某层没有候选时，配额不得凭空造出空位。
    #[test]
    fn layer_quota_does_not_invent_empty_slots() {
        let mk = |layer: &str, id: &str, score: f32| MemoryHit {
            layer: layer.to_string(),
            id: id.to_string(),
            kind: "fact".to_string(),
            title: id.to_string(),
            snippet: String::new(),
            score,
            raw_score: score,
            route: ROUTE_OBS_SEMANTIC,
            fingerprint: id.to_string(),
        };
        let sorted = vec![
            mk("observations", "o1", 0.9),
            mk("observations", "o2", 0.8),
            mk("observations", "o3", 0.7),
        ];
        let out = apply_layer_quota(&sorted, 10);
        assert_eq!(out.len(), 3, "只有 3 条候选就该只回 3 条");
    }

    /// RRF：同一文档被**两路**召回时分数累加，排名高于只被单路召回的文档 ——
    /// 这正是「hybrid」要的信号叠加（旧口径做不到：cos 0.66 恒压关键词 0.30，
    /// 两路命中的文档只是被 (layer,id) 去重取最高分，融合信息被丢掉）。
    #[test]
    fn rrf_rewards_documents_hit_by_more_than_one_route() {
        let mk = |route: &'static str, id: &str, raw: f32| {
            make_hit(
                "observations",
                id.into(),
                "fact",
                id.into(),
                format!("body {id}"),
                raw,
                id,
                route,
            )
        };
        // a 两路都命中（语义 rank1 + 关键词 rank1）；b 单路 rank1；c 单路 rank2。
        let hits = vec![
            mk(ROUTE_OBS_SEMANTIC, "a", 0.66),
            mk(ROUTE_OBS_SEMANTIC, "b", 0.90),
            mk(ROUTE_OBS_SEMANTIC, "c", 0.55),
            mk(ROUTE_OBS_KEYWORD, "a", KEYWORD_SCORE),
            mk(ROUTE_OBS_KEYWORD, "d", KEYWORD_SCORE),
        ];
        let out = dedup_hits(hits);
        assert_eq!(out[0].id, "a", "两路命中应排第一: {:?}", ids_of(&out));
        assert_eq!(ids_of(&out).len(), 4, "a 在多路里只算一条");
        // a 的分 ≈ 1/61 + 1/61，严格大于单路首发的 b ≈ 1/61。
        let a = out.iter().find(|h| h.id == "a").unwrap();
        let b = out.iter().find(|h| h.id == "b").unwrap();
        assert!(a.score > b.score, "{} vs {}", a.score, b.score);
        // b（语义 0.90，排名 2）与 d（关键词，排名 2）名次相同 → 融合分相同：
        // RRF 只看名次，不再被两路的量纲差（0.90 vs 0.30）左右。
        let d = out.iter().find(|h| h.id == "d").unwrap();
        assert!(
            (b.score - d.score).abs() < 1e-6,
            "同排名应同分（量纲无关）: b={} d={}",
            b.score,
            d.score
        );
    }

    /// 旧口径回归陷阱：语义路 rank2 的 cos 0.90 与关键词路 rank2 的固定分 0.30
    /// 在「分数拼接」下差 0.6，在 RRF 下都记 `1/(60+2)` —— 完全相等。
    #[test]
    fn rrf_ignores_raw_score_magnitude() {
        let mk = |route: &'static str, id: &str, raw: f32| {
            make_hit(
                "knowledge",
                id.into(),
                "fact",
                id.into(),
                id.to_string(),
                raw,
                id,
                route,
            )
        };
        let out = rrf_fuse(vec![
            mk(ROUTE_KNOWLEDGE_SEMANTIC, "hi", 0.95),
            mk(ROUTE_KNOWLEDGE_KEYWORD, "lo", 0.30),
        ]);
        assert_eq!(ids_of(&out), ["hi", "lo"], "同排名 → 稳定序按 id 兜底");
        assert!(
            (out[0].score - out[1].score).abs() < 1e-9,
            "{:?}",
            out.len()
        );
    }

    fn ids_of(hits: &[MemoryHit]) -> Vec<String> {
        hits.iter().map(|h| h.id.clone()).collect()
    }

    /// cli 2026-09-30「同层近似去重」：本地库 23 条 `kind=turn` 的观测正文都是
    /// 「你好」，指纹相同 → 同层只留最高分那条，不再刷屏 top-N。
    #[test]
    fn same_layer_near_duplicates_collapse_to_one() {
        let hits = vec![
            make_hit(
                "observations",
                "o1".into(),
                "turn",
                "t1".into(),
                "你好".into(),
                0.66,
                "你好",
                ROUTE_OBS_SEMANTIC,
            ),
            make_hit(
                "observations",
                "o2".into(),
                "turn",
                "t2".into(),
                "你好！".into(),
                0.65,
                "你好！",
                ROUTE_OBS_SEMANTIC,
            ),
            make_hit(
                "observations",
                "o3".into(),
                "turn",
                "t3".into(),
                "  你好  ".into(),
                0.64,
                "  你好  ",
                ROUTE_OBS_SEMANTIC,
            ),
            make_hit(
                "observations",
                "o4".into(),
                "fact",
                "别的".into(),
                "并发模型".into(),
                0.30,
                "并发模型",
                ROUTE_OBS_KEYWORD,
            ),
        ];
        let out = dedup_hits(hits);
        assert_eq!(out.len(), 2, "三条「你好」应只剩 1 条: {:?}", out.len());
        // 三条「你好」同属 observations.semantic 一路，名次 1/2/3 → 融合分依次
        // 递减，故保留名次最靠前的 o1。
        assert_eq!(out[0].id, "o1", "保留的是名次最高那条");
        // 指纹不同层相同不合并：另一层同样的正文要留下。
        let cross = vec![
            make_hit(
                "observations",
                "o1".into(),
                "turn",
                "t".into(),
                "你好".into(),
                0.66,
                "你好",
                ROUTE_OBS_SEMANTIC,
            ),
            make_hit(
                "knowledge",
                "k1".into(),
                "fact",
                "k".into(),
                "你好".into(),
                0.30,
                "你好",
                ROUTE_KNOWLEDGE_SEMANTIC,
            ),
        ];
        assert_eq!(dedup_hits(cross).len(), 2, "跨层同名不合并");
    }

    /// An embedder whose vectors are the lexical ones but which reports a
    /// *different* backend id — used to simulate a sibling store written in
    /// another vector space.
    struct ForeignEmbedder;

    impl Embedder for ForeignEmbedder {
        fn embed(&self, text: &str) -> Vec<f32> {
            HashingEmbedder::new().embed(text)
        }
        fn backend(&self) -> &'static str {
            "fastembed/all-MiniLM-L6-v2"
        }
    }

    #[tokio::test]
    async fn cross_project_search_warns_on_embedder_space_mismatch() {
        // The sibling recorded a *different* backend than this process, so its
        // stored vectors live in another space: a cosine against our query is
        // meaningless. The tool must degrade to keyword-only recall and say so,
        // never rank the noise.
        let root = std::env::temp_dir().join(format!("lingmiao-ext-space-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let ext_mem = root.join(".memory");
        std::fs::create_dir_all(&ext_mem).unwrap();
        let foreign: Arc<dyn Embedder> = Arc::new(ForeignEmbedder);
        let chat = Memory::open_in_dir(&ext_mem, Zone::Chat, Some(foreign)).unwrap();
        chat.observations
            .insert(&NewObservation::new(
                "decision",
                "quasar",
                "quasar is the chosen name",
            ))
            .unwrap();
        drop(chat);

        // This process uses HashingEmbedder → mismatch with the recorded backend.
        let (mem, dir) = temp_memory("ext-space");
        let mut reg = ToolRegistry::new();
        register(&mut reg, mem, None);
        let out = reg
            .execute(
                "search_external_memory",
                json!({"project_dir": root.display().to_string(), "keyword": "quasar"}),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(
            out.content.contains("\"semantic_recall\": false"),
            "semantic recall not disabled: {}",
            out.content
        );
        assert!(
            out.content.contains("空间不一致"),
            "no space warning: {}",
            out.content
        );
        // Keyword recall still surfaces the hit across the space boundary.
        assert!(out.content.contains("quasar"), "{}", out.content);

        // Control: a sibling written in the *same* space keeps semantic recall on
        // and emits no warning.
        let same_root =
            std::env::temp_dir().join(format!("lingmiao-ext-same-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&same_root);
        std::fs::create_dir_all(&same_root).unwrap();
        seed_external_project(&same_root);
        let same = reg
            .execute(
                "search_external_memory",
                json!({"project_dir": same_root.display().to_string(), "keyword": "zephyr"}),
            )
            .await
            .unwrap();
        assert!(
            same.content.contains("\"semantic_recall\": true"),
            "{}",
            same.content
        );
        assert!(!same.content.contains("空间不一致"), "{}", same.content);

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&same_root).ok();
    }

    #[tokio::test]
    async fn cross_project_search_without_local_embedder_does_not_claim_semantic() {
        // This process was opened with `embedder = None`, so there is nothing to
        // embed the query with: the semantic/KG passes cannot run and
        // `semantic_recall` must be `false` even though the sibling store is
        // perfectly same-space. Claiming `true` was the "embedder=None 误报"
        // blemish.
        let root = std::env::temp_dir().join(format!("lingmiao-ext-noemb-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        seed_external_project(&root);

        let dir =
            std::env::temp_dir().join(format!("lingmiao-memtools-noemb-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mem = Memory::open_in_dir(&dir, Zone::Chat, None).expect("open memory");
        let mut reg = ToolRegistry::new();
        register(&mut reg, Arc::new(mem), None);
        let out = reg
            .execute(
                "search_external_memory",
                json!({"project_dir": root.display().to_string(), "keyword": "zephyr"}),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(
            out.content.contains("\"semantic_recall\": false"),
            "embedder=None still claimed semantic recall: {}",
            out.content
        );
        assert!(
            out.content.contains("无嵌入后端"),
            "no embedder-none note: {}",
            out.content
        );
        // Keyword recall still surfaces the hit across the (semantic-less) sweep.
        assert!(out.content.contains("zephyr"), "{}", out.content);

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn update_memory_then_update_knowledge_links_the_observation() {
        // D 项 ④ (原版 `pending_obs_ids` 链): the observation an update_memory
        // wrote must attach to the node a following update_knowledge creates.
        // The original's chain was broken at the source (no producer); this pins
        // the working replacement.
        let (mem, dir) = temp_memory("pending");
        let mut reg = ToolRegistry::new();
        register(&mut reg, mem.clone(), None);
        let obs = reg
            .execute(
                "update_memory",
                json!({"kind":"fact","name":"f","content":"the fact"}),
            )
            .await
            .unwrap();
        let obs_id = obs.content.rsplit(' ').next().unwrap().trim().to_string();
        assert!(obs_id.starts_with("obs-"), "{obs_id}");
        reg.execute(
            "update_knowledge",
            json!({"kind":"fact","name":"n","summary":"s","content":"the node"}),
        )
        .await
        .unwrap();
        let node = mem.knowledge.find_node("fact", "n").unwrap().unwrap();
        let ids: Vec<String> = serde_json::from_str(&node.obs_ids).unwrap();
        assert_eq!(ids, [obs_id], "the node attached the earlier observation");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn link_entries_creates_cross_layer_links_and_search_reports_neighbours() {
        // D 项 ②/③: the explicit cross-layer writer, and search_memory carrying
        // each hit's one-hop neighbours so the model can walk the graph.
        let (mem, dir) = temp_memory("links");
        let obs_id = mem
            .observations
            .insert(&NewObservation::new("fact", "anchor", "alpha observation"))
            .unwrap();
        let node_id = mem
            .knowledge
            .upsert_node(&NewNode::new("technology", "alpha-node", "s", "alpha node"))
            .unwrap();
        let mut reg = ToolRegistry::new();
        register(&mut reg, mem, None);
        let out = reg
            .execute(
                "link_entries",
                json!({
                    "from": format!("observations:{obs_id}"),
                    "to": [format!("knowledge:{node_id}")],
                    "relation": "supports",
                }),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("\"created\": 1"), "{}", out.content);

        // A bare id infers its layer from the prefix.
        let bare = reg
            .execute(
                "link_entries",
                json!({"from": node_id, "to": [obs_id], "relation": "refines"}),
            )
            .await
            .unwrap();
        assert!(
            bare.content.contains(&format!("knowledge:{node_id}")),
            "{}",
            bare.content
        );
        assert!(
            bare.content.contains(&format!("observations:{obs_id}")),
            "{}",
            bare.content
        );

        // Unknown layer / self-link / empty target are rejected.
        assert!(
            reg.execute("link_entries", json!({"from":"nope:x","to":[obs_id]}))
                .await
                .is_err()
        );
        assert!(
            reg.execute("link_entries", json!({"from": obs_id, "to": [obs_id]}))
                .await
                .is_err()
        );
        assert!(
            reg.execute("link_entries", json!({"from": obs_id, "to": []}))
                .await
                .is_err()
        );

        // search_memory reports one-hop neighbours on each hit, from both ends.
        let hit = reg
            .execute("search_memory", json!({"keyword": "alpha"}))
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&hit.content).expect("JSON");
        let links_of = |id: &str| -> Vec<Value> {
            v["results"]
                .as_array()
                .unwrap()
                .iter()
                .find(|r| r["id"] == id)
                .map(|r| r["links"].as_array().cloned().unwrap_or_default())
                .unwrap_or_default()
        };
        let obs_links = links_of(&obs_id);
        assert_eq!(obs_links.len(), 2, "obs has both links: {obs_links:?}");
        assert!(
            obs_links
                .iter()
                .any(|l| l["relation"] == "supports" && l["id"] == node_id),
            "{obs_links:?}"
        );
        assert!(
            obs_links
                .iter()
                .any(|l| l["relation"] == "refines" && l["id"] == node_id),
            "{obs_links:?}"
        );
        let node_links = links_of(&node_id);
        assert_eq!(
            node_links.len(),
            2,
            "node sees the same rows: {node_links:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}

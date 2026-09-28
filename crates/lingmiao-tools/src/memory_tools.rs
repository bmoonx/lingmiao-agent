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
use std::sync::Arc;

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

/// `update_memory` — append an observation (#2).
pub struct UpdateMemoryTool {
    memory: Arc<Memory>,
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
        Ok(ToolOutput::ok(format!("stored observation {id}")))
    }
}

/// `update_knowledge` — upsert a knowledge-graph node (#3).
pub struct UpdateKnowledgeTool {
    memory: Arc<Memory>,
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
        let new = NewNode {
            kind: &a.kind,
            name: &a.name,
            summary: &summary,
            content: &a.content,
            keywords: &a.keywords,
            topic: "",
            obs_ids: "[]",
        };
        let id = self
            .memory
            .knowledge
            .upsert_node(&new)
            .map_err(|e| mem_err("update_knowledge", e))?;
        Ok(ToolOutput::ok(format!("upserted node {id}")))
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
    score: f32,
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

/// Per-layer recall depth used by `search_memory` (需求⑤ `search`/`candidates`
/// view). Exposed so the meta platform reports the *actual* numbers instead of
/// a doc copy.
pub const RECALL_OBS_SEMANTIC: usize = 30;
/// Keyword-pass recall depth for observations.
pub const RECALL_OBS_KEYWORD: usize = 10;
/// Semantic recall depth for the knowledge graph.
pub const RECALL_KNOWLEDGE_SEMANTIC: usize = 30;
/// Keyword recall depth for the archive.
pub const RECALL_ARCHIVE_KEYWORD: usize = 10;

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
/// `semantic` gates the two **embedding-based** passes (observations +
/// knowledge semantic recall). The lexical passes (observations/archive keyword)
/// are always run. The cross-project search clears it when the sibling store was
/// written in a *different* vector space, so a meaningless cosine can neither be
/// computed nor pollute the ranking.
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
                        hits.push(MemoryHit {
                            layer: "observations".to_string(),
                            id: o.id,
                            kind: o.kind,
                            title: o.name,
                            snippet: preview(&o.content, SNIPPET_CHARS),
                            score: s.score,
                        });
                    }
                }
                Err(e) => errors.push(format!("observations.semantic: {e}")),
            }
        }
        match observations.search(term, RECALL_OBS_KEYWORD, "") {
            Ok(rows) => {
                for s in rows {
                    let o = s.observation;
                    hits.push(MemoryHit {
                        layer: "observations".to_string(),
                        id: o.id,
                        kind: o.kind,
                        title: o.name,
                        snippet: preview(&o.content, SNIPPET_CHARS),
                        score: KEYWORD_SCORE,
                    });
                }
            }
            Err(e) => errors.push(format!("observations.keyword: {e}")),
        }
    }

    if let Some(knowledge) = knowledge {
        // #3 knowledge graph — semantic recall only (no keyword pass exists);
        // skipped entirely when the vector space is not shared.
        if semantic {
            match knowledge.search_semantic(term, RECALL_KNOWLEDGE_SEMANTIC) {
                Ok(rows) => {
                    for (node, score) in rows {
                        hits.push(MemoryHit {
                            layer: "knowledge".to_string(),
                            id: node.id,
                            kind: node.kind,
                            title: node.name,
                            snippet: preview(&node.summary, SNIPPET_CHARS),
                            score,
                        });
                    }
                }
                Err(e) => errors.push(format!("knowledge.semantic: {e}")),
            }
        }
    }

    if let Some(archive) = archive {
        // #1 archive — keyword recall over the raw transcript.
        match archive.search(term, RECALL_ARCHIVE_KEYWORD) {
            Ok(rows) => {
                for t in rows {
                    hits.push(MemoryHit {
                        layer: "archive".to_string(),
                        id: t.id,
                        kind: "turn".to_string(),
                        title: preview(&t.user_msg, 80),
                        snippet: preview(&t.assistant, SNIPPET_CHARS),
                        score: KEYWORD_SCORE,
                    });
                }
            }
            Err(e) => errors.push(format!("archive.keyword: {e}")),
        }
    }

    hits
}

/// Dedup hits by `(layer, id)` keeping the highest score, then sort descending.
/// Shared by the score-order path and the LLM re-rank path (a re-ranker only
/// reorders what is already deduped).
fn dedup_hits(hits: Vec<MemoryHit>) -> Vec<MemoryHit> {
    let mut seen: std::collections::HashMap<(String, String), usize> =
        std::collections::HashMap::new();
    let mut deduped: Vec<MemoryHit> = Vec::with_capacity(hits.len());
    for h in hits {
        let key = (h.layer.clone(), h.id.clone());
        match seen.get(&key) {
            Some(&i) => {
                if h.score > deduped[i].score {
                    deduped[i] = h;
                }
            }
            None => {
                seen.insert(key, deduped.len());
                deduped.push(h);
            }
        }
    }
    deduped.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    deduped
}

/// Dedup, rank by score descending, then truncate to `limit`. Used where no LLM
/// re-ranker is involved (the cross-project search, and the fallback path).
fn rank_and_truncate(hits: Vec<MemoryHit>, limit: usize) -> Vec<MemoryHit> {
    let mut deduped = dedup_hits(hits);
    deduped.truncate(limit);
    deduped
}

/// Re-order `deduped` hits by an LLM's ranked indices, dropping unknown /
/// duplicate entries, then top up with the remaining score-ordered hits up to
/// `limit` (原版 `_llm_rerank_candidates` safety net — a short LLM ranking
/// never shrinks the result set below `limit`).
fn apply_rerank_order(deduped: &[MemoryHit], ranked: &[usize], limit: usize) -> Vec<MemoryHit> {
    let mut out: Vec<MemoryHit> = Vec::new();
    let mut used: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
    for &idx in ranked {
        if let Some(h) = deduped.get(idx)
            && used.insert((h.layer.clone(), h.id.clone()))
        {
            out.push(h.clone());
        }
    }
    if out.len() < limit {
        for h in deduped {
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
        let deduped = dedup_hits(hits);

        // Phase 3 (原版 `registration.py:603`): LLM re-rank. Falls back to
        // plain score order when no re-ranker is wired or the LLM call fails.
        let deduped = match &self.reranker {
            Some(rr) if !deduped.is_empty() => {
                let candidates: Vec<RerankCandidate> = deduped
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
                    Ok(ranked) => apply_rerank_order(&deduped, &ranked, limit),
                    Err(e) => {
                        errors.push(format!("llm rerank: {e}"));
                        let mut d = deduped;
                        d.truncate(limit);
                        d
                    }
                }
            }
            _ => {
                let mut d = deduped;
                d.truncate(limit);
                d
            }
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
pub const TOOL_NAMES: [&str; 11] = [
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
    registry.register(UpdateMemoryTool {
        memory: memory.clone(),
    });
    registry.register(UpdateKnowledgeTool { memory });
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
        };
        let deduped = vec![mk("observations", "a", 0.9), mk("observations", "b", 0.8)];
        // Out-of-range + duplicate indices dropped, remaining topped up in order.
        let out = apply_rerank_order(&deduped, &[1, 1, 7], 2);
        assert_eq!(
            out.iter().map(|h| h.id.as_str()).collect::<Vec<_>>(),
            ["b", "a"]
        );
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
}

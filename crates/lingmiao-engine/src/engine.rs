//! The QL turn engine — 组织上下文 → 工作阶段 → 沉淀阶段.
//!
//! **The engine holds no conversation state**（原版对齐，cli 2026-10-05「按原版
//! 设计来」）: every turn's message list starts from **blank**, and continuity comes
//! only from the memory layer — the 组织上下文 stage selects records and
//! [`crate::context::assemble`] rebuilds the recent-turns section from the #1
//! archive (`archive.recent(n)`) each turn. The original Python
//! (`core/query_loop.py::_run_impl`) threaded no message container at all, and its
//! `J-清空` stage had nothing to clear; the Rust port briefly kept an
//! `Engine.history` buffer, which this restores away.
//!
//! Every observable step is emitted through the shared [`EventBus`], so the TUI
//! (and later the auditor) observe the same stream the Python event bus produced.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};

use lingmiao_core::events::{Event, Usage};
use lingmiao_core::polling::{Guarded, Progress, WaitClass, guard_at, with_wait_scope};
use lingmiao_core::{Config, EventBus, LingmiaoError};
use lingmiao_llm::{Client, LlmResponse, Message, ModelJudge, ModelRegistry, UserConfig};
use lingmiao_tools::ToolRegistry;
use serde_json::{Value, json};

use crate::context::{self, SummaryReport, TurnContext};
use crate::stage_agent::StageAgent;

/// Stage name for the conversation stage (mirrors the Python `C-对话`).
pub const STAGE_C: &str = lingmiao_core::config::STAGE_C_DIALOG;
/// Stage name for the context-selection stage.
pub const STAGE_B: &str = lingmiao_core::config::STAGE_B_CONTEXT;
/// Stage name for the consolidated stage (Q9). As of the 2026-09-28 rename the
/// algorithmic memory-graph update (原版 `I-知识图谱更新` / MG evolution) is
/// **folded into this stage** — it runs right after the LLM call and reports the
/// same stage name, so the pipeline is exactly three named stages
/// (`组织上下文 → 工作阶段 → 沉淀阶段`).
pub const STAGE_SUMMARY: &str = lingmiao_core::config::STAGE_CONSOLIDATE;

/// Stage name for the **embedding retrieval** half of 组织上下文. The original
/// ran it as a separate algorithmic stage (`A-嵌入检索`: `knowledge.search` →
/// top-50 candidates) with no LLM; Rust runs it as the deterministic prologue of
/// [`Engine::stage_b`], which is why it is a *sub*-stage rather than a fourth
/// pipeline stage (the 2026-09-28 three-name pipeline is unchanged).
pub const STAGE_A: &str = "嵌入检索";

/// How many knowledge-graph candidates the embedding retrieval feeds the base
/// reference（原版 `A-嵌入检索`: `knowledge.search(user_input, top_k=50)`）。
const A_TOP_K: usize = 50;

/// Where the selected-context block reaches the model.
///
/// **Injection is system-only**（2026-10-05 B 项回归原版 `context.py:335`）：the
/// block is substituted into the `## 上下文` slot of 工作阶段's **system** prompt,
/// exactly as `ContextAssembler.assemble` did. The earlier dual injection
/// (system `{prefix}` **plus** a `[上下文检索]` user message) doubled the block
/// and left the user copy outside every §8.5 segment — both fixed here.
///
/// Named (rather than an inline literal) because the A→B **handoff notice**
/// ([`Event::ContextHandoff`]) must report the *real* destination to the UI — a
/// copy-pasted string in the TUI could silently drift from what actually went on
/// the wire. cli 2026-09-30; re-pointed at the system slot 2026-10-05.
pub const CONTEXT_INJECT_PREFIX: &str = "## 上下文";

/// Wire role carrying the selected context: the **`system`** prompt（原版对齐 ——
/// `template.format(prefix=…)` 填的正是 system 模板的 `{prefix}` 槽）. Reported
/// verbatim by [`Event::ContextHandoff`].
pub const CONTEXT_INJECT_ROLE: &str = "system";

/// Where the block sits: the tail of 工作阶段's **system** prompt, right before
/// the `_search_strategy` block. Reported verbatim by
/// [`Event::ContextHandoff`].
pub const CONTEXT_INJECT_POSITION: &str = "工作阶段 system 尾部";

/// The behaviour discipline the original prepends to **工作阶段's first user
/// message**（`c.py:82 _build_initial_messages`）.
///
/// The Rust port had dropped it entirely (`grep TOOL DISCIPLINE` was empty), so
/// "don't retry a failing resource / stop when you have enough" reached the model
/// only as a paraphrase inside the system prompt. Restored verbatim (2026-10-05
/// B 项 B1=b), keeping 原版 wording so it matches the reference behaviour.
pub const TOOL_DISCIPLINE: &str = "[system: TOOL DISCIPLINE — If a resource returns 'not found' or errors, \
tell the user, don't retry. Stop when you have enough info. Productive multi-step work is fine.]";

/// Default per-stage time budget (seconds) when `stages.json` omits one.
const DEFAULT_STAGE_TIMEOUT: u64 = 600;

/// Local ISO-8601 timestamp with milliseconds (event `ts` field parity).
fn now_iso() -> String {
    chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, false)
}

/// A fresh session chain id (`chain-<8hex>`), generated once per engine and
/// held for its lifetime (③ E存档). Turns of one engine thus share a chain id
/// and differ by `chain_seq`.
fn new_chain_id() -> String {
    let id = lingmiao_memory::short_id("chain");
    let hex = id.rsplit('-').next().unwrap_or(&id);
    let short: String = hex.chars().take(8).collect();
    format!("chain-{short}")
}

/// Serialise a message list into the archive's `full_messages` JSON (the raw
/// wire shape sent to the model). Falls back to `[]` — never panics.
fn messages_to_json(messages: &[Message]) -> String {
    let wire: Vec<Value> = messages.iter().map(Message::to_wire).collect();
    serde_json::to_string(&wire).unwrap_or_else(|_| "[]".to_string())
}

/// Substitute `{key}` placeholders, leaving unknown ones untouched.
pub fn fill_prompt(template: &str, vars: &[(&str, &str)]) -> String {
    let mut out = template.to_string();
    for (k, v) in vars {
        out = out.replace(&format!("{{{k}}}"), v);
    }
    out
}

/// The conversation engine.
pub struct Engine {
    llm: Client,
    /// Per-stage model overrides from `config.json`'s `stages` block (cli
    /// 2026-09-27). A stage absent here runs on [`Self::llm`] (the default), so
    /// an unrouted stage — or no `config.json` at all — behaves exactly as
    /// before. Because a group *is* an API source, one stage can talk to a
    /// different provider than the next (e.g. 沉淀阶段 on Kimi).
    llm_by_stage: std::collections::HashMap<String, Client>,
    bus: Arc<EventBus>,
    /// The **raw** `C`-stage system template (`prompts.json` → `C.system`), kept
    /// unfilled on purpose: 原版 rendered the whole thing once per turn
    /// (`ContextAssembler.assemble` → `template.format(...)`), so `{local_now}` is
    /// this turn's clock rather than the process start. Every placeholder is
    /// filled in [`Engine::render_stage_system`].
    system_template: String,
    /// The shared `_env` block (`prompts.json` → `_env`), pre-rendered with the
    /// process-level values (cwd / brand / dirs). Injected through `{env_block}`.
    env_block: String,
    /// Process start time (`{startup_time}`), fixed for the engine's lifetime —
    /// unlike `{local_now}`, which advances with each turn.
    startup_time: String,
    /// Resolved configuration (prompts + stage whitelists + timeouts), cloned
    /// into each [`TurnContext`].
    cfg: Config,
    /// The tool registry the M4 stages draw their whitelists from and execute
    /// against. Empty in model-less tests; the full registry in `from_env`.
    registry: Arc<ToolRegistry>,
    /// #1/#2 memory layer (chat zone), wired in by [`Engine::from_env`] so a
    /// completed turn is persisted. `None` in model-less tests / when the
    /// memory layer cannot be opened.
    memory: Option<Arc<lingmiao_memory::Memory>>,
    /// ③ E存档: the session's chain id (constant for the engine's lifetime) and
    /// a monotonic per-turn sequence. Together they let turns of one logical
    /// conversation be aggregated by `(chain_id, chain_seq)` — the schema's
    /// `idx_turns_chain` index.
    chain_id: String,
    chain_seq: AtomicI64,
}

/// The full-turn extras persisted into the #1 archive row (③ E存档补齐全量).
///
/// Bundles everything the engine can supply beyond the reply text itself, so
/// [`Engine::record_turn`] stays a two-source call (reply + this struct) rather
/// than a wide argument list. All fields are the wire-ready strings the archive
/// stores as-is.
struct TurnRecordInput<'a> {
    /// System prompt handed to the model for this turn.
    system_prompt: &'a str,
    /// Injected context prefix (the B stage's selected context).
    context_prefix: &'a str,
    /// Raw message list (JSON) sent to the model.
    full_messages: &'a str,
    /// Model reasoning (`reasoning_content`) accumulated over the turn.
    reasoning: &'a str,
    /// Tool-call records (JSON array string) executed during the turn.
    tool_calls: &'a str,
}

/// Build the per-stage clients declared in `config.json`'s `stages` block.
///
/// Each entry routes one pipeline stage to its own `group/model` — and, since a
/// group *is* an API source, to its own provider. Entries that cannot be
/// resolved (unknown group/model, or a missing `api_key_env`) are **skipped
/// with a warning**, so the stage silently falls back to the default model
/// rather than failing the whole boot — the same optimistic-startup stance the
/// engine takes for the memory layer.
fn build_stage_clients(
    registry: &ModelRegistry,
    uc: &UserConfig,
    default: &Client,
) -> std::collections::HashMap<String, Client> {
    let mut map = std::collections::HashMap::new();
    for (stage, sel) in &uc.stages {
        // Resolve a legacy stage name (pre-2026-09-28) to its canonical name so
        // `llm_for(canonical)` finds the override; without this a user file
        // still keyed `C-对话` would silently route nothing.
        let stage = lingmiao_core::config::canonical_stage(stage);
        let Some(resolved) = registry.resolve_checked(&sel.group, &sel.model) else {
            tracing::warn!(
                stage = %stage,
                group = %sel.group,
                model = %sel.model,
                "config.json stage route names an unknown group/model; using the default model"
            );
            continue;
        };
        match resolved {
            Ok(spec) => {
                // An entry that resolves to the default model is redundant — skip
                // it so the map only ever holds real overrides.
                if spec.group_id == default.group_id() && spec.model == default.model() {
                    continue;
                }
                match Client::new(spec) {
                    Ok(c) => {
                        map.insert(stage.to_string(), c);
                    }
                    Err(e) => tracing::warn!(
                        stage = %stage,
                        "config.json stage route could not build a client ({e}); using the default model"
                    ),
                }
            }
            Err(e) => tracing::warn!(
                stage = %stage,
                group = %sel.group,
                "config.json stage route has no usable API key ({e}); using the default model"
            ),
        }
    }
    map
}

impl Engine {
    /// Build an engine over an existing client and bus (no memory layer, empty
    /// tool registry, embedded default config).
    pub fn new(llm: Client, bus: Arc<EventBus>) -> Self {
        Self {
            llm,
            llm_by_stage: std::collections::HashMap::new(),
            bus,
            system_template: String::new(),
            env_block: String::new(),
            startup_time: now_iso(),
            cfg: Config::load_default().expect("embedded default config"),
            registry: Arc::new(ToolRegistry::new()),
            memory: None,
            chain_id: new_chain_id(),
            chain_seq: AtomicI64::new(0),
        }
    }

    /// Build from the environment, seeding the `C`-stage system prompt from the
    /// embedded config (with the common placeholders filled).
    pub fn from_env(bus: Arc<EventBus>, cfg: &Config) -> Result<Self, LingmiaoError> {
        // The model catalogue comes from `config.json` (its `models` block) or,
        // failing that, the `models.json` chain (see [`ModelRegistry::load`]).
        let registry_models = ModelRegistry::load()?;
        let llm = Client::new(registry_models.default_spec()?)?;
        // Per-stage routing (cli 2026-09-27): `config.json`'s `stages` block lets
        // each pipeline stage run on its own model/API source. A malformed or
        // unresolvable entry is skipped with a warning; the stage then uses the
        // default client above, exactly as before this feature existed.
        let uc = UserConfig::load().unwrap_or_default();
        let llm_by_stage = build_stage_clients(&registry_models, &uc, &llm);
        if !llm_by_stage.is_empty() {
            let routes: Vec<String> = {
                let mut v: Vec<String> = llm_by_stage
                    .iter()
                    .map(|(s, c)| format!("{s}={}/{}", c.group_id(), c.model()))
                    .collect();
                v.sort();
                v
            };
            tracing::info!(routes = %routes.join(", "), "config.json stage routing active");
        }
        let cwd = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| ".".to_string());
        let system_template = cfg
            .prompt(
                lingmiao_core::config::stage_prompt_key(STAGE_C),
                lingmiao_core::config::PromptField::System,
            )
            .to_string();
        let cache_rel = lingmiao_core::brand::cache_rel();
        let env_block = fill_prompt(
            cfg.env_prompt(),
            &[
                ("cwd", &cwd),
                ("version", lingmiao_core::VERSION),
                ("model", llm.model()),
                ("brand", lingmiao_core::brand::NAME),
                ("memory_dir", lingmiao_core::brand::MEMORY_DIR),
                ("cache_dir", &cache_rel),
            ],
        );
        // Wire the memory layer (M2). Paths come from `lingmiao_core::paths` — never
        // hard-coded. A failure to open memory is non-fatal: the turn still runs
        // (just without persistence), matching the engine's optimistic startup.
        let paths = lingmiao_core::paths::global();
        if let Err(e) = paths.prepare() {
            tracing::warn!("memory: paths.prepare() failed: {e}");
        }
        let memory = match lingmiao_memory::open_default_zone(paths, lingmiao_memory::Zone::Chat) {
            Ok(m) => Some(Arc::new(m)),
            Err(e) => {
                tracing::warn!("memory disabled for this run: {e}");
                None
            }
        };
        // Build the tool registry (M4). With memory wired it is the full
        // registry — memory + business groups included — so the `stages.json`
        // whitelists all resolve; without it, the file + whiteboard groups.
        let project_root =
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        // ② search_memory LLM 精排 (原版 `registration.py:17`): the memory
        // tool's ranking stage is an LLM call, supplied here (lingmiao-tools only
        // knows the `Reranker` trait — it must not depend on lingmiao-llm).
        let reranker: Option<Arc<dyn lingmiao_tools::Reranker>> =
            Some(Arc::new(crate::rerank::LlmReranker::new(llm.clone())));
        let registry = match &memory {
            Some(m) => lingmiao_tools::full_registry(&project_root, m.clone(), cfg, reranker),
            None => lingmiao_tools::default_registry(&project_root),
        };
        Ok(Self {
            llm,
            llm_by_stage,
            bus,
            system_template,
            env_block,
            startup_time: now_iso(),
            cfg: cfg.clone(),
            registry: Arc::new(registry),
            memory,
            chain_id: new_chain_id(),
            chain_seq: AtomicI64::new(0),
        })
    }

    /// The LLM client a stage should run on: its `config.json` route when one is
    /// declared, else the default client ([`Self::llm`]).
    pub fn llm_for(&self, stage: &str) -> &Client {
        self.llm_by_stage.get(stage).unwrap_or(&self.llm)
    }

    /// The configured `stage=group/model` routes (sorted), for diagnostics.
    pub fn stage_routes(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .llm_by_stage
            .iter()
            .map(|(s, c)| format!("{s}={}/{}", c.group_id(), c.model()))
            .collect();
        v.sort();
        v
    }

    /// The underlying LLM client (model / provider introspection).
    pub fn llm(&self) -> &Client {
        &self.llm
    }

    /// The wired memory bundle, if any (chat zone).
    pub fn memory(&self) -> Option<&Arc<lingmiao_memory::Memory>> {
        self.memory.as_ref()
    }

    /// The tool registry the stages draw from.
    pub fn registry(&self) -> &Arc<ToolRegistry> {
        &self.registry
    }

    /// The resolved configuration.
    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// ⑤ MCP 接上 (**non-blocking**): connect every server declared in `mcp.json`
    /// **in the background** and register its discovered tools into the engine's
    /// registry as they arrive. Returns the spawned task's handle, or `None` when
    /// there is nothing to attach.
    ///
    /// Startup never waits on MCP — mirroring CC's lazy / non-blocking connect —
    /// so a slow or hung server can no longer delay boot (previously a single
    /// server could hold the serial attach for the full per-server timeout).
    /// Servers are connected **in parallel**, each with its own budget; a server
    /// that cannot spawn or discover is logged (`warn`) and skipped. Call once,
    /// after [`from_env`](Self::from_env) and before the first turn.
    pub fn spawn_mcp_attach(&self) -> Option<tokio::task::JoinHandle<usize>> {
        let servers: Vec<(String, lingmiao_core::config::McpServer)> = self
            .cfg
            .mcp_servers()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        if servers.is_empty() {
            return None;
        }
        // The registry is shared (`Arc`) with interior mutability, so the
        // background task can register tools into it after boot.
        let registry = self.registry.clone();
        Some(tokio::spawn(async move {
            let added = attach_mcp_servers(&registry, &servers).await;
            if added > 0 {
                tracing::info!("mcp: {added} remote tools attached");
            }
            added
        }))
    }

    /// Persist a completed turn: one #1 archive row plus one #2 observation of
    /// the assistant output. Best-effort — a memory failure must not fail the
    /// turn, so errors are logged and swallowed. `turn_id` is shared with the
    /// turn's 沉淀阶段 observations so archive ↔ #2 ↔ MG-staging reference one id.
    ///
    /// ③ E存档: `rec` carries the full-turn extras (system prompt / injected
    /// context / raw message list / reasoning / tool transcript) so the archive
    /// row is a complete transcript, not a five-field shell.
    fn record_turn(
        &self,
        turn_id: &str,
        input: &str,
        resp: &LlmResponse,
        rec: &TurnRecordInput<'_>,
    ) {
        let Some(m) = &self.memory else { return };
        let mut turn = lingmiao_memory::Turn::new(input, resp.content.clone());
        turn.id = turn_id.to_string();
        turn.system_prompt = rec.system_prompt.to_string();
        turn.context_prefix = rec.context_prefix.to_string();
        turn.full_messages = rec.full_messages.to_string();
        turn.reasoning = rec.reasoning.to_string();
        turn.tool_calls = if rec.tool_calls.trim().is_empty() {
            "[]".to_string()
        } else {
            rec.tool_calls.to_string()
        };
        turn.tokens_in = resp.usage.input_tokens as i64;
        turn.tokens_out = resp.usage.output_tokens as i64;
        turn.tokens_total = resp.usage.total_tokens as i64;
        turn.chain_id = self.chain_id.clone();
        // 1-based: first persisted turn → seq 1, next → 2, …
        turn.chain_seq = self.chain_seq.fetch_add(1, Ordering::SeqCst) + 1;
        if let Err(e) = m.archive.save(&turn) {
            tracing::warn!("memory: archive save failed: {e}");
        }
        let name = turn.id.clone();
        let obs = lingmiao_memory::NewObservation {
            kind: "turn",
            topic: "conversation",
            name: &name,
            content: &resp.content,
            keywords: "",
            turn_id,
            source: "engine",
            stage: STAGE_C,
            topics: "[]",
        };
        if let Err(e) = m.observations.insert(&obs) {
            tracing::warn!("memory: observation insert failed: {e}");
        }
    }

    /// ③ E存档: backfill this turn's archive row with the 沉淀阶段 summary. A
    /// targeted UPDATE (never `INSERT OR REPLACE`) so the row's embedding is
    /// preserved and no re-embedding is triggered. Best-effort; empty summaries
    /// and an unwired memory layer are no-ops.
    fn backfill_summary(&self, turn_id: &str, summary: &str) {
        if summary.trim().is_empty() {
            return;
        }
        let Some(m) = &self.memory else { return };
        if let Err(e) = m.archive.set_summary(turn_id, summary) {
            tracing::warn!("memory: archive summary backfill failed: {e}");
        }
    }

    /// Persist the 沉淀阶段 observation items into #2 — the 原版 F-要点记录
    /// `observations.insert_batch` write. These are the `fact` / `preference` /
    /// `decision` / `constraint` rows stage I (MG evolution) later reads back.
    /// Best-effort: a memory failure is logged, never fatal. Returns the count
    /// written (0 when memory is unwired or the model proposed no items).
    fn persist_summary_observations(&self, report: &SummaryReport, turn_id: &str) -> usize {
        let Some(m) = &self.memory else { return 0 };
        let items = report.items();
        if items.is_empty() {
            return 0;
        }
        let batch: Vec<lingmiao_memory::NewObservation<'_>> = items
            .iter()
            .map(|it| lingmiao_memory::NewObservation {
                kind: &it.kind,
                topic: &it.topic,
                name: &it.name,
                content: &it.content,
                keywords: &it.keywords,
                turn_id,
                source: STAGE_SUMMARY,
                stage: STAGE_SUMMARY,
                topics: "[]",
            })
            .collect();
        match m
            .observations
            .insert_batch(&batch, turn_id, STAGE_SUMMARY, STAGE_SUMMARY)
        {
            Ok(ids) => ids.len(),
            Err(e) => {
                tracing::warn!("memory: summary observation batch insert failed: {e}");
                0
            }
        }
    }

    // ── 管线：组织上下文 → 工作阶段 → 沉淀阶段（Q7 / Q9） ──────

    /// The tool names a stage may use, from `stages.json`.
    fn stage_tools(&self, stage: &str) -> Vec<String> {
        self.cfg
            .stage(stage)
            .map(|s| s.tools.clone())
            .unwrap_or_default()
    }

    /// Build a [`StageAgent`] for `stage` with its configured time budget.
    fn stage_agent(&self, stage: &str) -> StageAgent {
        // `timeout: 0` means "no timeout" (工作阶段 may legitimately run for a
        // very long time across many tool calls); a *missing* `timeout` field
        // still falls back to the default per-stage budget.
        let timeout = match self.cfg.stage(stage).map(|s| s.timeout) {
            Some(0) => Duration::ZERO,
            Some(secs) => Duration::from_secs(secs),
            None => Duration::from_secs(DEFAULT_STAGE_TIMEOUT),
        };
        StageAgent::new(
            self.llm_for(stage).clone(),
            self.bus.clone(),
            self.registry.clone(),
            timeout,
        )
        .with_core_lock(self.cfg.full_core_lock(stage))
        // F 项（cli 2026-10-05 拍板）: 每个阶段的等待都由**模型**决策是否停摆
        // （① 等模型吐字那一处除外——见 `LLM_STREAM_STALL`）。裁判跑在本阶段
        // 自己的 client 上，所以它的决策走的是该阶段同一个 API 来源。
        .with_wait_judge(Some(ModelJudge::handle(self.llm_for(stage).clone())))
    }

    /// Render the `工作阶段` system prompt for **this turn**.
    ///
    /// The process-wide half (`{env_block}` / `{local_now}` / cwd / brand) was
    /// rendered once in [`Engine::from_env`]; here the per-turn placeholders are
    /// filled — most importantly `{prefix}`, the six-section context block the
    /// 组织上下文 stage selected（原版 `ContextAssembler.assemble` 的 `{prefix}`）。
    /// It is re-rendered 每轮 because **the engine carries no conversation
    /// state**: the continuity that a history buffer used to provide now comes
    /// from `archive.recent(n)` inside that very block.
    fn render_stage_system(&self, stage: &str, ctx: &TurnContext) -> String {
        if stage != STAGE_C {
            let raw = self.cfg.prompt(
                lingmiao_core::config::stage_prompt_key(stage),
                lingmiao_core::config::PromptField::System,
            );
            if raw.is_empty() {
                return String::new();
            }
            let cwd = std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_default();
            let cache_rel = lingmiao_core::brand::cache_rel();
            let mut s = fill_prompt(
                raw,
                &[
                    ("version", lingmiao_core::VERSION),
                    ("cwd", &cwd),
                    // The `{model}` placeholder names the model this stage will
                    // actually talk to (a `config.json` stage route may differ from
                    // the default, cli 2026-09-27).
                    ("model", self.llm_for(stage).model()),
                    ("local_time", &now_iso()),
                    ("startup_time", &now_iso()),
                    ("brand", lingmiao_core::brand::NAME),
                    ("memory_dir", lingmiao_core::brand::MEMORY_DIR),
                    ("cache_dir", &cache_rel),
                    ("env_prefix", lingmiao_core::brand::ENV_PREFIX),
                ],
            );
            let skill = self
                .cfg
                .prompt(stage, lingmiao_core::config::PromptField::Skill);
            if !skill.is_empty() {
                s.push_str("\n\n");
                s.push_str(skill);
            }
            let json_rule = self
                .cfg
                .prompt(stage, lingmiao_core::config::PromptField::JsonRule);
            if !json_rule.is_empty() {
                s.push_str("\n\n");
                s.push_str(json_rule);
            }
            // `_search_strategy` 对**所有** stage 追加（原版
            // `stage_agent._build_system` 即如此）—— Rust 此前只挂在 C 分支，
            // B / 沉淀阶段拿不到检索策略（B 项 2026-10-05 B3=a）。
            let strategy = self.cfg.search_strategy();
            if !strategy.is_empty() {
                s.push_str("\n\n");
                s.push_str(strategy);
            }
            return s;
        }

        // 工作阶段: render the `C` template **this turn**（原版 `assemble` 的
        // `template.format(...)`）—— `{local_now}` 是此刻的时间，`{prefix}` 是
        // 本轮组织上下文装配出的上下文块。进程级值（brand / cwd / `_env`）已固化在
        // `self` 里，不再重算。
        if self.system_template.is_empty() {
            return String::new();
        }
        let prefix = ctx.b_context.clone().unwrap_or_default();
        let reply_instruction = "用中文直接回复用户。";
        let project_root_block = String::new();
        // `_base` (自我描述) 是**每轮**渲染的：它含 `{local_now}`（此刻时间）。
        // 原版链路（`engine.py::set_env_prompt` → `_format_env_fields` →
        // `c.py::_build_initial_messages` 的 `[system: {env_prompt}]`）在 Rust 里
        // 从未接上——`base_prompt()` 只有单测引用，模型永远看不到「需要 help /
        // meta 时去查」这些自省指引。这里把它填进 `C.system` 的 `{base}` 槽位。
        let base = fill_prompt(
            self.cfg.base_prompt(),
            &[
                ("brand", lingmiao_core::brand::NAME),
                ("version", lingmiao_core::VERSION),
                ("startup_time", &self.startup_time),
                ("local_now", &now_iso()),
            ],
        );
        // 原版 `assemble` 把 `tool_registry.describe_all()` 填进 `{tools}` —— 一份给
        // 模型读的散文工具清单（schema 另走请求体的 `tools` 字段）。Rust 此前把该槽
        // 位留空，`## 可用工具` 下就是空白；这里按阶段白名单重新生成。
        let tools_prose = self.describe_stage_tools(STAGE_C);
        let mut s = fill_prompt(
            &self.system_template,
            &[
                ("version", lingmiao_core::VERSION),
                ("brand", lingmiao_core::brand::NAME),
                ("memory_dir", lingmiao_core::brand::MEMORY_DIR),
                ("cache_dir", &lingmiao_core::brand::cache_rel()),
                ("env_prefix", lingmiao_core::brand::ENV_PREFIX),
                ("startup_time", &self.startup_time),
                ("local_now", &now_iso()),
                ("env_block", &self.env_block),
                ("base", &base),
                ("prefix", &prefix),
                ("reply_instruction", reply_instruction),
                ("project_root_block", &project_root_block),
                // 「角色循环」按 Q2 砍掉，无独立 role block。
                ("role_block", ""),
                ("tools", &tools_prose),
            ],
        );
        // Belt and braces: a template that does not spell `{env_block}` still gets
        // the shared env block appended (never dropped silently).
        if !self.env_block.is_empty() && !s.contains(&self.env_block) {
            s.push('\n');
            s.push_str(&self.env_block);
        }
        // The `_search_strategy` block is appended after assembly（原版
        // `assemble` 尾部）, so the retrieval advice sits with the context it
        // describes rather than inside the template.
        let strategy = self.cfg.search_strategy();
        if !strategy.is_empty() {
            s.push_str("\n\n");
            s.push_str(strategy);
        }
        s
    }

    /// The prose tool list for `stage`'s `{tools}` prompt slot — 原版
    /// `ToolRegistry.describe_all()`, narrowed to the stage whitelist.
    ///
    /// The machine-readable schemas already ride the request's `tools` field;
    /// this is the human-readable companion the `C` template prints under
    /// `## 可用工具`（原版两个都发：schema 走请求体，散文清单走 system）。 An
    /// empty whitelist yields an empty string, so `{tools}` collapses cleanly.
    /// The `mcp:*` pattern is skipped here — a remote tool's schema still rides
    /// the request, and its dynamic name is not enumerable at prompt time.
    fn describe_stage_tools(&self, stage: &str) -> String {
        self.stage_tools(stage)
            .iter()
            .filter_map(|name| {
                self.registry
                    .get(name)
                    .map(|t| format!("- {name}: {}", t.description()))
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// `组织上下文` — select the context block for this turn.
    ///
    /// Three deterministic steps, mirroring the original `_run_impl` +
    /// `_select_context`:
    /// 1. **A-嵌入检索** (algorithmic, no LLM) — [`KnowledgeGraph::search_semantic`]
    ///    over the user input → top-[`A_TOP_K`] candidates.
    /// 2. **底座参考**（[`context::build_base`]）— 约束 / KG top-10 / 观测 /
    ///    **最近 3 轮（来自归档库）** / 技能 — the model picks context with the
    ///    institutional constraints already in view.
    /// 3. **assembler**（[`context::assemble`]）— turn the stage's `load…` JSON
    ///    into the six-section block that becomes 工作阶段's `{prefix}`, and
    ///    record its **real** per-section character volumes for the §8.5 bar.
    pub async fn stage_b(&self, ctx: &mut TurnContext) -> Result<(), LingmiaoError> {
        // ── A-嵌入检索（纯算法，无 LLM）──
        self.bus.push(Event::StageStarted {
            stage: STAGE_A.to_string(),
            ts: now_iso(),
        });
        let a_started = Instant::now();
        let candidates: Vec<(lingmiao_memory::Node, f32)> = ctx
            .memory
            .as_ref()
            .and_then(|m| m.knowledge.search_semantic(&ctx.input, A_TOP_K).ok())
            .unwrap_or_default();
        ctx.a_candidates = candidates.len() as u64;
        self.bus.push(Event::StageResultReported {
            stage: STAGE_A.to_string(),
            ok: true,
            data: json!({"candidates": ctx.a_candidates}),
            fault_type: String::new(),
            fault_detail: String::new(),
            tokens: Value::Null,
            tool_calls: 0,
            elapsed_ms: a_started.elapsed().as_secs_f64() * 1000.0,
        });

        // ── B-上下文选择（底座参考 + 模型挑选）──
        let system = self.render_stage_system(STAGE_B, ctx);
        let tools = ctx.registry.schemas_for(&self.stage_tools(STAGE_B));
        let base = ctx
            .memory
            .as_ref()
            .map(|m| context::build_base(m, &candidates))
            .unwrap_or_default();
        let messages = vec![Message::user(format!(
            "{base}\n── 用户输入 ──\n{}\n",
            ctx.input
        ))];
        let out = self
            .stage_agent(STAGE_B)
            .work(STAGE_B, system, messages, tools)
            .await?;

        // §8.5: attribute the retrieval by *real* source. The B stage searches
        // the knowledge graph and the observation/archive logs with distinct
        // tools; summing their returned characters gives an honest split for
        // the context bar's 知识记忆 / 历史观测 segments (no magic coefficient).
        for (tool, chars) in &out.tool_result_chars {
            if tool.contains("knowledge") {
                ctx.b_knowledge_chars += chars;
            } else if tool.contains("observation") || tool.contains("archive") {
                ctx.b_history_chars += chars;
            }
        }

        // ── assembler：把选择拼成上下文块，作为工作阶段的 {prefix}（原版 assemble）──
        let assembled = ctx
            .memory
            .as_ref()
            .map(|m| context::assemble(m, &out.json))
            .unwrap_or_default();
        // The assembled block's own section volumes are the honest figures — they
        // are what actually goes into 工作阶段's system prompt (the tool-result
        // totals above count hits that were retrieved but *not* injected).
        ctx.b_knowledge_chars = assembled.knowledge_chars;
        ctx.b_history_chars = assembled.observation_chars;
        ctx.b_recent_turn_chars = assembled.recent_turn_chars;

        // cli 2026-09-30 — the A→B **handoff notice**. The stage has just chosen
        // its context; report *what* it picked and *how* the next stage receives
        // it, straight off the real values this function is about to store:
        // the counts come from the model's own `load…` JSON (and, for the turns,
        // from the archive rows actually injected), and the format description
        // comes from the very constants `stage_c` injects with — so the notice can
        // never describe a contract the injection does not honour.
        self.bus.push(handoff_event(
            &out.json,
            assembled.text.chars().count() as u64,
            assembled.recent_turns,
        ));
        ctx.b_context = Some(assembled.text);
        Ok(())
    }

    /// `工作阶段` — the tool-enabled conversation loop, writing
    /// [`TurnContext::c_answer`] and persisting the turn.
    pub async fn stage_c(&self, ctx: &mut TurnContext) -> Result<(), LingmiaoError> {
        let system = self.render_stage_system(STAGE_C, ctx);
        let tools = ctx.registry.schemas_for(&self.stage_tools(STAGE_C));
        // **The message list starts from blank**（原版对齐）: no carried-over
        // history — the only continuity is the context block above, whose
        // 最近对话 section was rebuilt from the archive this very turn.
        //
        // 原版 `c.py:82 _build_initial_messages` 的首条 user = TOOL DISCIPLINE 段 +
        // 本轮原始输入。上下文块**不**在这里重复一份：它已经进 system 的
        // `## 上下文` 槽（B 项 2026-10-05 回归原版；此前 system + user 双注入会把
        // 同一块递两遍，且 user 那份不计入任何 §8.5 分段）。
        let mut messages: Vec<Message> = Vec::new();
        messages.push(Message::user(format!("{TOOL_DISCIPLINE}\n\n{}", ctx.input)));

        // §8.5 上下文组成条 — account the *real* per-section character counts of
        // this first injection (user-facing names, 去黑话). The B-stage retrieval
        // is split by source into 知识记忆 (KG hits) and 历史观测 (observation /
        // archive hits) — matching the seven-segment design in ux-design.md
        // §8.5/§8.6（`规则底座` 再扣掉注入的 `{prefix}` 与散文 `{tools}`，保证分段
        // 不重叠、分项之和 == provider 实测总量）, rather than collapsing both into
        // one 上下文检索 segment. `lock` is the composed base+stage core lock the
        // stage agent re-injects (原版对齐). The UI derives each section's
        // tokens as `chars × (provider_input_tokens ÷ total_chars)`, so the parts
        // always sum to the provider-measured total.
        let lock_chars = self.cfg.full_core_lock(STAGE_C).chars().count();
        // §8.5 分段不许重叠：`规则底座` = system 模板**扣掉**注入的 `{prefix}` 与
        // `{tools}` 散文清单（前者已按来源拆成 知识记忆/历史观测/最近对话，后者归
        // `工具能力`），否则同一段文字会被计两次，分项之和就与 provider 实测对不上。
        let tools_prose = self.describe_stage_tools(STAGE_C);
        let prefix_chars = ctx
            .b_context
            .as_deref()
            .map(|b| b.chars().count())
            .unwrap_or(0);
        // 首条 user 里的行为纪律（TOOL DISCIPLINE）属**静态规则**，归 `规则底座`；
        // 不加上它，分段之和就会比 provider 实测少这一段的字数（§8.5 要求分项之和
        // == provider 实测总量）。
        let discipline_chars = TOOL_DISCIPLINE.chars().count();
        let system_chars = system
            .chars()
            .count()
            .saturating_sub(prefix_chars + tools_prose.chars().count())
            + discipline_chars;
        // 工具能力 = schema（请求体 `tools`）+ 散文清单（system `{tools}`）——两者
        // 都在请求里，合并计入才与 provider 实测对得上。
        let tools_chars = tools.to_string().chars().count() + tools_prose.chars().count();
        let sections = context_sections(
            system_chars,
            lock_chars,
            ctx.b_knowledge_chars,
            ctx.b_history_chars,
            ctx.b_recent_turn_chars as usize,
            tools_chars,
            ctx.input.chars().count(),
        );
        let total_chars: u64 = sections
            .as_array()
            .map(|a| a.iter().map(|s| s["chars"].as_u64().unwrap_or(0)).sum())
            .unwrap_or(0);
        self.bus.push(Event::ContextUsage {
            stage: STAGE_C.to_string(),
            sections,
            total_chars,
        });

        let system_prompt = system.clone();
        // ③ E存档: capture the raw message list before `work` consumes it.
        let full_messages = messages_to_json(&messages);
        let out = self
            .stage_agent(STAGE_C)
            .work(STAGE_C, system, messages, tools)
            .await?;

        // Persist the completed turn (#1 archive + #2 observation). Nothing is
        // *retained* in the engine: the archive row written here is what the
        // next turn's 组织上下文 reads back via `archive.recent(n)`.
        let resp = LlmResponse {
            content: out.content.clone(),
            usage: out.usage.clone(),
            ..Default::default()
        };
        let turn_id = ctx.turn_id.clone();
        let tool_calls =
            serde_json::to_string(&out.tool_call_records).unwrap_or_else(|_| "[]".to_string());
        let context_prefix = ctx.b_context.clone().unwrap_or_default();
        self.record_turn(
            &turn_id,
            &ctx.input,
            &resp,
            &TurnRecordInput {
                system_prompt: &system_prompt,
                context_prefix: &context_prefix,
                full_messages: &full_messages,
                reasoning: &out.reasoning,
                tool_calls: &tool_calls,
            },
        );
        ctx.c_answer = Some(out.content);
        Ok(())
    }

    /// 沉淀阶段 — one consolidated LLM call producing observations + audit +
    /// quality + task, emitted as [`Event::SummaryReported`].
    pub async fn stage_summary(
        &self,
        ctx: &mut TurnContext,
    ) -> Result<SummaryReport, LingmiaoError> {
        let system = self.render_stage_system(STAGE_SUMMARY, ctx);
        let tools = ctx.registry.schemas_for(&self.stage_tools(STAGE_SUMMARY));
        let b = ctx.b_context.clone().unwrap_or_default();
        let answer = ctx.c_answer.clone().unwrap_or_default();
        let messages = vec![Message::user(format!(
            "[组织上下文加载]\n{b}\n\n[本轮对话]\nUser: {}\nAssistant: {answer}",
            ctx.input
        ))];
        let out = self
            .stage_agent(STAGE_SUMMARY)
            .work(STAGE_SUMMARY, system, messages, tools)
            .await?;
        let report = SummaryReport::from_json(&out.json);
        // Persist the proposed observation items (#2) — the 原版 F-要点记录
        // write. Stage I (MG evolution) reads these back to evolve the graph.
        let written = self.persist_summary_observations(&report, &ctx.turn_id);
        if written > 0 {
            tracing::debug!("沉淀阶段 persisted {written} observation items");
        }
        // ③ E存档: backfill this turn's archive row with the consolidated summary.
        self.backfill_summary(&ctx.turn_id, &report.summary);
        self.bus.push(summary_event(&report));
        ctx.summary = Some(report.clone());
        Ok(report)
    }

    /// Memory-graph update (原版 stage I / MG evolution) — evolve the knowledge
    /// graph from this turn's observations. Purely algorithmic, no LLM.
    /// **Folded into the 沉淀阶段 stage** since the 2026-09-28 rename: it reports
    /// the same stage name, so the pipeline stays three named stages. Best-effort:
    /// a memory failure degrades (logged) but never aborts the turn. Emits the
    /// stage's `stage_started` / `stage_result_reported` and, on success, the
    /// `mg_updated` event.
    pub fn stage_i(&self) -> Option<lingmiao_memory::MgUpdateReport> {
        let m = self.memory.as_ref()?;
        self.bus.push(Event::StageStarted {
            stage: STAGE_SUMMARY.to_string(),
            ts: now_iso(),
        });
        let started = Instant::now();
        match m.evolve_memory_graph() {
            Ok(report) => {
                self.bus.push(Event::MgUpdated {
                    nodes_updated: report.nodes_updated,
                    communities: report.communities,
                });
                self.bus.push(Event::StageResultReported {
                    stage: STAGE_SUMMARY.to_string(),
                    ok: true,
                    data: serde_json::to_value(&report).unwrap_or(Value::Null),
                    fault_type: String::new(),
                    fault_detail: String::new(),
                    tokens: Value::Null,
                    tool_calls: 0,
                    elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
                });
                Some(report)
            }
            Err(e) => {
                tracing::warn!("stage `{STAGE_SUMMARY}` (MG update) failed: {e}");
                self.bus.push(Event::StageResultReported {
                    stage: STAGE_SUMMARY.to_string(),
                    ok: false,
                    data: Value::Null,
                    fault_type: "memory".to_string(),
                    fault_detail: e.message().to_string(),
                    tokens: Value::Null,
                    tool_calls: 0,
                    elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
                });
                None
            }
        }
    }

    /// Run one full M4 turn: `组织上下文 → 工作阶段 → 沉淀阶段`.
    ///
    /// Each stage is a `Result` error boundary (Q7): a failing B or C stage
    /// degrades the turn but does not abort it, and a failing consolidation
    /// stage still reports a summary event. Returns the consolidated report.
    pub async fn run_turn(&self, input: &str) -> Result<SummaryReport, LingmiaoError> {
        // F 项（cli 2026-10-05 拍板）: arm the wait judge for this whole turn.
        // `tokio::task_local!` scope — every wait inside the turn (the 9 sites,
        // including the ones buried in tools and the MCP client) sees it; code
        // outside a turn (unit tests driving a tool directly) sees none and keeps
        // the pre-polling behaviour, so no test is silently re-routed.
        //
        // cli 2026-10-05 第二轮（「要进界面的，这是核心体验」）: the same scope
        // arms the **event bus**, so every poll heartbeat is published as
        // [`Event::WaitPolled`] and the wait becomes visible in the transcript —
        // previously the 5s sampling ran and decided correctly but left no trace
        // anywhere the user could see (only a `wait judge ruling` log line), so a
        // silent 60s `cargo fmt` still read as a hang.
        let judge = ModelJudge::handle(self.llm.clone());
        with_wait_scope(
            Some(judge),
            Some(self.bus.clone()),
            self.run_turn_inner(input),
        )
        .await
    }

    /// The turn body; [`Engine::run_turn`] wraps it in the wait-judge scope.
    async fn run_turn_inner(&self, input: &str) -> Result<SummaryReport, LingmiaoError> {
        let mut ctx = TurnContext::new(
            input.to_string(),
            self.memory.clone(),
            self.registry.clone(),
            self.cfg.clone(),
        );

        if let Err(e) = self.stage_b(&mut ctx).await {
            tracing::warn!("stage `{STAGE_B}` failed: {e}");
        }
        if let Err(e) = self.stage_c(&mut ctx).await {
            tracing::warn!("stage `{STAGE_C}` failed: {e}");
        }
        let report = match self.stage_summary(&mut ctx).await {
            Ok(report) => report,
            Err(e) => {
                let report = SummaryReport::failed(e.message().to_string());
                self.bus.push(summary_event(&report));
                ctx.summary = Some(report.clone());
                report
            }
        };
        // Memory-graph update: evolve the knowledge graph from this turn's
        // observations (原版 I-知识图谱更新). Folded into 沉淀阶段 since the
        // 2026-09-28 rename; runs after the consolidated LLM call regardless of
        // its outcome. A missing memory layer or an error is swallowed inside
        // `stage_i`.
        self.stage_i();
        Ok(report)
    }
}

/// ⑤ MCP 接上: per-server attach budget. Since the attach now runs **in the
/// background** (see [`Engine::spawn_mcp_attach`]) this only bounds how long a
/// hung child is retried before it is dropped — it never delays startup. A
/// server that neither initialises nor errors within this window is treated as
/// a failure (`warn` + skip).
const MCP_ATTACH_TIMEOUT: Duration = Duration::from_secs(15);

/// ⑤ MCP 接上: attach a list of declared servers to `registry` **in parallel**,
/// skipping (with a `warn`) any server whose command is empty/disabled or that
/// fails to spawn/discover. Returns the total number of tools added.
///
/// Each server is connected on its own task; discovered tools are registered as
/// each server finishes. The registry takes `&self` (interior mutability) so it
/// can be mutated while already shared.
async fn attach_mcp_servers(
    registry: &ToolRegistry,
    servers: &[(String, lingmiao_core::config::McpServer)],
) -> usize {
    let mut tasks = tokio::task::JoinSet::new();
    for (name, server) in servers {
        if server.command.trim().is_empty() {
            // A `{}` entry (or one whose command was removed) is the config's
            // "disabled" convention — skip silently.
            continue;
        }
        let name = name.clone();
        let server = server.clone();
        tasks.spawn(async move {
            // F 项 ③: the attach wait is polled. No judge in this background task
            // → `MCP_ATTACH_TIMEOUT` keeps its exact old meaning (a hard cap);
            // the call site is ready for a judge to be armed here without any
            // further change (`guard` picks it up from the task scope).
            let outcome = match guard_at(
                lingmiao_core::polling::current_judge(),
                WaitClass::Network,
                format!("MCP `{name}` 连接"),
                MCP_ATTACH_TIMEOUT,
                Progress::new(),
                attach_one_mcp_server(&server),
            )
            .await
            {
                Guarded::Done(v) => v,
                Guarded::Aborted(a) => Err(format!("中断：{}", a.message())),
                Guarded::TimedOut => {
                    Err(format!("timed out after {}s", MCP_ATTACH_TIMEOUT.as_secs()))
                }
            };
            (name, outcome)
        });
    }
    let mut added = 0;
    while let Some(joined) = tasks.join_next().await {
        match joined {
            Ok((name, Ok(tools))) => {
                let n = tools.len();
                for tool in tools {
                    registry.register_arc_remote(Arc::new(tool));
                }
                tracing::info!("mcp: `{name}` attached ({n} tools)");
                added += n;
            }
            Ok((name, Err(e))) => tracing::warn!("mcp: `{name}` skipped: {e}"),
            Err(e) => tracing::warn!("mcp: attach task failed: {e}"),
        }
    }
    added
}

/// Connect one MCP server over stdio and **return** its discovered tools (the
/// caller registers them, so the registry is only touched on the main task).
async fn attach_one_mcp_server(
    server: &lingmiao_core::config::McpServer,
) -> Result<Vec<lingmiao_tools::RemoteTool>, String> {
    use lingmiao_tools::mcp::client::RmcpSource;
    let source = RmcpSource::connect_with_env(&server.command, &server.args, &server.env).await?;
    let source: Arc<dyn lingmiao_tools::McpSource> = Arc::new(source);
    lingmiao_tools::mcp::discover(source).await
}

/// Build the §8.5 context-bar segments: fixed, user-facing, fixed-order
/// segments (ux-design.md §8.5/§8.6). Real character counts of this turn's
/// first injection — the B-stage retrieval split by source (`知识记忆` = KG,
/// `历史观测` = observations/archive) rather than a single `上下文检索` blob,
/// plus `lock` (the composed base+stage core lock the stage agent re-injects).
fn context_sections(
    system_chars: usize,
    lock_chars: usize,
    knowledge_chars: u64,
    history_chars: u64,
    dialog_chars: usize,
    tools_chars: usize,
    input_chars: usize,
) -> Value {
    Value::Array(vec![
        json!({"name": "规则底座", "chars": system_chars}),
        json!({"name": "lock", "chars": lock_chars}),
        json!({"name": "知识记忆", "chars": knowledge_chars}),
        json!({"name": "历史观测", "chars": history_chars}),
        json!({"name": "最近对话", "chars": dialog_chars}),
        json!({"name": "工具能力", "chars": tools_chars}),
        json!({"name": "当前提问", "chars": input_chars}),
    ])
}

/// Build the A→B **handoff event** from the context stage's own JSON output,
/// the length of the block it assembled, and the number of archive turns that
/// block actually carries.
///
/// Kept a free function (like [`summary_event`]) so the shape is testable
/// without a live model: feed it the real `load…` payload and the same
/// constants [`Engine::stage_c`] injects with, and assert the notice adds up.
///
/// `recent_turns` comes from the **archive lookup** (`assemble` returned how many
/// rows it injected), not from the model's `loadRecentTurns` request — a model
/// that asks for 3 turns against an empty archive injected 0, and the notice must
/// say 0 rather than promise continuity that is not there.
fn handoff_event(b_out: &Value, chars: u64, recent_turns: u64) -> Event {
    let count = |key: &str| {
        b_out
            .get(key)
            .and_then(Value::as_array)
            .map(|a| a.len() as u64)
            .unwrap_or(0)
    };
    Event::ContextHandoff {
        from: STAGE_B.to_string(),
        to: STAGE_C.to_string(),
        observations: count("loadObservations"),
        nodes: count("loadNodes"),
        recent_turns,
        chars,
        prefix: CONTEXT_INJECT_PREFIX.to_string(),
        role: CONTEXT_INJECT_ROLE.to_string(),
        position: CONTEXT_INJECT_POSITION.to_string(),
    }
}

/// Build the [`Event::SummaryReported`] for a consolidated report.
fn summary_event(r: &SummaryReport) -> Event {
    Event::SummaryReported {
        stage: STAGE_SUMMARY.to_string(),
        prompt: String::new(),
        response: r.summary.clone(),
        turn_type: r.obs_type.clone(),
        summary: r.summary.clone(),
        observations: r.observations,
        task_status: r.task_status.clone(),
        next_steps: r.next_steps.clone(),
        audit_grade: r.audit_grade.clone(),
        issues: r.issues,
        quality_grade: r.quality_grade.clone(),
        corrections: r.corrections,
        error: r.error.clone(),
    }
}

/// Empty token accounting helper (used by the TUI status line).
pub fn empty_usage() -> Usage {
    Usage::default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A two-group catalogue with inline keys (so no env is needed in tests).
    fn test_registry() -> ModelRegistry {
        let v: Value = serde_json::from_str(
            r#"{
              "default": {"group":"deepseek","model":"a"},
              "groups": [
                {"id":"deepseek","protocol":"openai","base_url":"http://ds",
                 "api_key":"k1","models":[{"id":"a"},{"id":"b"}]},
                {"id":"kimi","protocol":"openai","base_url":"http://kimi",
                 "api_key":"k2","models":[{"id":"kimi-for-coding"}]}
              ]
            }"#,
        )
        .unwrap();
        ModelRegistry::from_json(&v).unwrap()
    }

    fn client_for(reg: &ModelRegistry, group: &str, model: &str) -> Client {
        Client::new(reg.resolve(group, model).unwrap()).unwrap()
    }

    fn uc_with(stages: &[(&str, &str, &str)]) -> UserConfig {
        let v: Value = serde_json::json!({
            "stages": stages
                .iter()
                .map(|(s, g, m)| (s.to_string(), serde_json::json!({"group": g, "model": m})))
                .collect::<serde_json::Map<_, _>>()
        });
        UserConfig::from_json(&v, None).unwrap()
    }

    #[test]
    fn stage_routing_builds_a_client_per_stage() {
        // cli 2026-09-27: `config.json` routes each stage to its own model — and
        // because a group is an API source, to its own provider.
        let reg = test_registry();
        let default = client_for(&reg, "deepseek", "a");
        let uc = uc_with(&[
            ("组织上下文", "deepseek", "b"),
            ("工作阶段", "kimi", "kimi-for-coding"),
            ("沉淀阶段", "deepseek", "a"),
        ]);
        let map = build_stage_clients(&reg, &uc, &default);
        // B and C get real overrides…
        assert_eq!(map.len(), 2, "routes: {:?}", map.keys().collect::<Vec<_>>());
        assert_eq!(map["组织上下文"].model(), "b");
        assert_eq!(map["工作阶段"].group_id(), "kimi");
        assert_eq!(map["工作阶段"].model(), "kimi-for-coding");
        // …and a route equal to the default is dropped (it is not an override:
        // 沉淀阶段 stays on the default client).
        assert!(!map.contains_key("沉淀阶段"));
    }

    #[test]
    fn unresolvable_stage_routes_fall_back_to_the_default() {
        let reg = test_registry();
        let default = client_for(&reg, "deepseek", "a");
        let uc = uc_with(&[
            ("工作阶段", "nope", "x"),      // unknown group
            ("组织上下文", "kimi", "nope"), // unknown model
        ]);
        let map = build_stage_clients(&reg, &uc, &default);
        assert!(
            map.is_empty(),
            "a bad route must be skipped so the stage falls back: {map:?}"
        );
    }

    #[test]
    fn llm_for_prefers_the_stage_route_and_defaults_otherwise() {
        let reg = test_registry();
        let default = client_for(&reg, "deepseek", "a");
        let uc = uc_with(&[("工作阶段", "kimi", "kimi-for-coding")]);
        let map = build_stage_clients(&reg, &uc, &default);
        let engine = Engine {
            llm: default,
            llm_by_stage: map,
            bus: Arc::new(EventBus::default()),
            system_template: String::new(),
            env_block: String::new(),
            startup_time: String::new(),
            cfg: Config::load_default().unwrap(),
            registry: Arc::new(ToolRegistry::new()),
            memory: None,
            chain_id: "chain-test".into(),
            chain_seq: AtomicI64::new(0),
        };
        assert_eq!(engine.llm_for("工作阶段").group_id(), "kimi");
        assert_eq!(engine.llm_for("组织上下文").group_id(), "deepseek");
        assert_eq!(engine.llm_for("工作阶段").model(), "kimi-for-coding");
        assert_eq!(engine.stage_routes(), vec!["工作阶段=kimi/kimi-for-coding"]);
    }

    #[test]
    fn fill_prompt_replaces_known_keys_only() {
        let s = fill_prompt(
            "v{version} at {cwd} {unknown}",
            &[("version", "1.2"), ("cwd", "/p")],
        );
        assert_eq!(s, "v1.2 at /p {unknown}");
    }

    #[test]
    fn c_system_prompt_renders_identity_placeholders() {
        let cfg = Config::load_default().unwrap();
        let raw = cfg.prompt("C", lingmiao_core::config::PromptField::System);
        let base = fill_prompt(
            cfg.base_prompt(),
            &[
                ("brand", lingmiao_core::brand::NAME),
                ("version", lingmiao_core::VERSION),
                ("startup_time", "2026-10-05T00:00:00+08:00"),
                ("local_now", "2026-10-05T00:00:00+08:00"),
            ],
        );
        let rendered = fill_prompt(
            raw,
            &[
                ("brand", lingmiao_core::brand::NAME),
                ("memory_dir", lingmiao_core::brand::MEMORY_DIR),
                ("cache_dir", &lingmiao_core::brand::cache_rel()),
                ("env_prefix", lingmiao_core::brand::ENV_PREFIX),
                ("version", lingmiao_core::VERSION),
                ("base", &base),
            ],
        );
        assert!(
            rendered.contains(&format!("你是 {} 编程助手", lingmiao_core::brand::NAME)),
            "_base（身份块）没被填进 `{{base}}`：{rendered}"
        );
        assert!(!rendered.contains("{base}"));
        assert!(!rendered.contains("{brand}"));
        assert!(!rendered.contains("{memory_dir}"));
        assert!(!rendered.contains("{cache_dir}"));
        assert!(!rendered.contains("{env_prefix}"));
    }

    #[test]
    fn c_system_prompt_has_no_unfilled_placeholder_after_rendering() {
        // cli 2026-10-05「按原版设计来」轮实测：模板里的 `{prefix}` 曾经**从未被替换**
        // —— 模型读到的字面量就是 `{prefix}`，上下文块根本没进 system。本测试钉住
        // 占位符契约：模板新增槽位而渲染不跟进，这里即失败。
        let cfg = Config::load_default().unwrap();
        let raw = cfg.prompt("C", lingmiao_core::config::PromptField::System);
        let rendered = fill_prompt(
            raw,
            &[
                // from_env 的进程级槽位。
                ("version", lingmiao_core::VERSION),
                ("cwd", "/p"),
                ("model", "m"),
                ("local_now", "2026-10-05T00:00:00+08:00"),
                ("startup_time", "2026-10-05T00:00:00+08:00"),
                ("brand", lingmiao_core::brand::NAME),
                ("memory_dir", lingmiao_core::brand::MEMORY_DIR),
                ("cache_dir", &lingmiao_core::brand::cache_rel()),
                ("env_prefix", lingmiao_core::brand::ENV_PREFIX),
                ("env_block", "（运行环境）"),
                // render_stage_system 的每轮槽位。
                ("base", "（身份块）"),
                ("prefix", "（组织上下文）"),
                ("reply_instruction", "用中文直接回复用户。"),
                ("project_root_block", ""),
                ("role_block", ""),
                ("tools", ""),
            ],
        );
        assert!(!rendered.contains("{prefix}"), "上下文块没进 system");
        assert!(!rendered.contains("{local_now}"));
        assert!(!rendered.contains("{role_block}"));
        assert!(!rendered.contains("{tools}"));
        assert!(!rendered.contains("{reply_instruction}"));
        assert!(rendered.contains("（组织上下文）"));
    }

    #[test]
    fn c_system_template_placeholders_are_all_filled_by_render_stage_system() {
        // 契约：`C.system` 里出现的**每一个** `{slot}` 都必须由
        // `render_stage_system` 填（或由 from_env 固化）。模板加了槽位而渲染不跟进
        // ——历史上 `{prefix}` / `{base}` 正是这样留在 prompt 里给模型看的——
        // 这里会直接失败，不靠人眼。
        let cfg = Config::load_default().unwrap();
        let raw = cfg.prompt("C", lingmiao_core::config::PromptField::System);
        let known: Vec<&str> = vec![
            // from_env 固化的进程级槽位 + render_stage_system 的每轮槽位。
            "version",
            "cwd",
            "model",
            "local_now",
            "startup_time",
            "brand",
            "memory_dir",
            "cache_dir",
            "env_prefix",
            "env_block",
            "base",
            "prefix",
            "reply_instruction",
            "project_root_block",
            "role_block",
            "tools",
        ];
        let mut found: Vec<String> = Vec::new();
        let mut i = 0;
        while let Some(open) = raw[i..].find('{') {
            let start = i + open + 1;
            if let Some(close) = raw[start..].find('}') {
                let slot = &raw[start..start + close];
                if !slot.is_empty()
                    && slot.chars().all(|c| c.is_ascii_lowercase() || c == '_')
                    && raw.as_bytes().get(start + close + 1) != Some(&b'}')
                {
                    assert!(
                        known.contains(&slot),
                        "`C.system` 有未接线占位符 `{{{slot}}}`：要么在 render_stage_system 里填它，要么从模板删掉"
                    );
                    found.push(slot.to_string());
                }
                i = start + close + 1;
            } else {
                break;
            }
        }
        assert!(
            found.iter().any(|s| s == "prefix"),
            "上下文块槽位必须仍在模板里：{found:?}"
        );
        assert!(found.iter().any(|s| s == "base"), "身份块槽位：{found:?}");
    }

    #[test]
    fn pipeline_stage_system_prompts_are_present() {
        use lingmiao_core::config::{PromptField, stage_prompt_key};
        let cfg = Config::load_default().unwrap();
        for stage in [STAGE_B, STAGE_C, STAGE_SUMMARY] {
            let key = stage_prompt_key(stage);
            let p = cfg.prompt(key, PromptField::System);
            assert!(
                p.chars().count() > 100,
                "stage `{stage}` (prompt key `{key}`) system prompt is empty/too short"
            );
        }
        assert!(
            cfg.has_pipeline(),
            "the embedded config must define the pipeline"
        );
    }

    #[test]
    fn describe_stage_tools_lists_the_stage_whitelist() {
        // 原版 `describe_all()` 的 Rust 对齐：`{tools}` 槽位要填一份真实工具清单
        // （此前留空，`## 可用工具` 下是空白）。
        let cfg = Config::load_default().unwrap();
        let root = std::env::temp_dir().join(format!("lingmiao-tools-{}", std::process::id()));
        let registry = lingmiao_tools::default_registry(&root);
        let client = Client::new(lingmiao_llm::ModelSpec::openai(
            "test-key",
            "http://localhost:1",
            "test-model",
        ))
        .unwrap();
        let engine = Engine {
            llm: client,
            llm_by_stage: Default::default(),
            bus: Arc::new(EventBus::default()),
            system_template: String::new(),
            env_block: String::new(),
            startup_time: String::new(),
            cfg,
            registry: Arc::new(registry),
            memory: None,
            chain_id: "chain-test".into(),
            chain_seq: AtomicI64::new(0),
        };
        let prose = engine.describe_stage_tools(STAGE_C);
        assert!(
            prose.contains("- read_file: "),
            "工作阶段的散文清单应列出 read_file: {prose}"
        );
        assert!(prose.contains("- bash: "), "{prose}");
        // 白名单外的不出现（`default_registry` 无 memory 组）。
        assert!(!prose.contains("search_memory"), "{prose}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn context_sections_are_seven_segments_in_fixed_order() {
        // §8.5/§8.6: fixed user-facing segments, fixed names + order. `lock`
        // sits right after the rule base (both are static guidance, distinct
        // from the retrieved/dynamic segments).
        let s = context_sections(8000, 220, 5100, 9000, 22400, 4800, 2500);
        let names: Vec<&str> = s
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "规则底座",
                "lock",
                "知识记忆",
                "历史观测",
                "最近对话",
                "工具能力",
                "当前提问"
            ]
        );
        // 知识记忆 / 历史观测 are the real retrieved volumes, not one blob.
        assert_eq!(s[1]["chars"], 220);
        assert_eq!(s[2]["chars"], 5100);
        assert_eq!(s[3]["chars"], 9000);
    }

    #[test]
    fn stage_b_attributes_retrieval_by_source() {
        // The B-stage tool→segment mapping: KG tools → 知识记忆, observation /
        // archive tools → 历史观测, everything else ignored.
        let mut kg = 0u64;
        let mut hist = 0u64;
        for (tool, chars) in [
            ("search_knowledge", 120u64),
            ("search_observations", 300u64),
            ("search_archive", 40u64),
            ("list_observations", 10u64),
            ("memory_stats", 999u64),
        ] {
            if tool.contains("knowledge") {
                kg += chars;
            } else if tool.contains("observation") || tool.contains("archive") {
                hist += chars;
            }
        }
        assert_eq!(kg, 120);
        assert_eq!(hist, 350);
    }

    #[test]
    fn engine_holds_no_conversation_state() {
        // 原版对齐 (cli 2026-10-05「按原版设计来」): the engine carries no history
        // buffer. Continuity comes from the #1 archive via 组织上下文, so there is
        // nothing for a `/clear` to reset engine-side — pinned here as the public
        // surface simply not existing any more.
        let bus = Arc::new(EventBus::default());
        let cfg = Config::load_default().unwrap();
        // Construct without network: empty key still builds a client object.
        let client = Client::new(lingmiao_llm::ModelSpec::openai(
            "test-key",
            "http://localhost:1",
            "test-model",
        ))
        .unwrap();
        let engine = Engine::new(client, bus);
        // No state to inspect: the assertion is the *type* below (a `history`
        // field would not compile against this destructuring).
        let Engine {
            memory, chain_id, ..
        } = &engine;
        assert!(memory.is_none(), "a bare engine wires no memory layer");
        assert!(chain_id.starts_with("chain-"), "{chain_id}");
        let _ = fill_prompt(cfg.base_prompt(), &[]);
    }

    #[test]
    fn c_stage_timeout_zero_means_no_timeout() {
        // `stages.json` sets 工作阶段's `timeout` to 0 → `Duration::ZERO` (the
        // stage may run for a very long time); B/沉淀阶段 keep their 600s budget.
        let client = Client::new(lingmiao_llm::ModelSpec::openai(
            "test-key",
            "http://localhost:1",
            "test-model",
        ))
        .unwrap();
        let engine = Engine::new(client, Arc::new(EventBus::default()));
        assert_eq!(engine.stage_agent(STAGE_C).timeout(), Duration::ZERO);
        assert_eq!(
            engine.stage_agent(STAGE_B).timeout(),
            Duration::from_secs(600)
        );
        assert_eq!(
            engine.stage_agent(STAGE_SUMMARY).timeout(),
            Duration::from_secs(600)
        );
    }

    #[test]
    fn record_turn_persists_archive_and_observation() {
        use lingmiao_memory::{Memory, Zone};
        let root = std::env::temp_dir().join(format!("lingmiao-eng-mem-{}", std::process::id()));
        let paths = lingmiao_core::paths::Paths::at(&root);
        paths.ensure_dirs().unwrap();
        let mem = Memory::open(&paths, Zone::Chat, None).expect("open memory");

        let client = Client::new(lingmiao_llm::ModelSpec::openai(
            "test-key",
            "http://localhost:1",
            "test-model",
        ))
        .unwrap();
        let mut engine = Engine::new(client, Arc::new(EventBus::default()));
        engine.memory = Some(Arc::new(mem));

        let resp = LlmResponse {
            content: "hello there".to_string(),
            usage: Usage {
                input_tokens: 5,
                output_tokens: 3,
                total_tokens: 8,
                ..Default::default()
            },
            ..Default::default()
        };
        engine.record_turn(
            "turn-test-1",
            "hi",
            &resp,
            &TurnRecordInput {
                system_prompt: "sys",
                context_prefix: "",
                full_messages: "[]",
                reasoning: "",
                tool_calls: "[]",
            },
        );

        let m = engine.memory.as_ref().unwrap();
        assert_eq!(m.archive.count().unwrap(), 1);
        assert_eq!(m.observations.stats().unwrap(), 1);
        let recent = m.archive.recent(1).unwrap();
        assert_eq!(recent[0].id, "turn-test-1");
        assert_eq!(recent[0].user_msg, "hi");
        assert_eq!(recent[0].assistant, "hello there");
        assert_eq!(recent[0].tokens_total, 8);
        // The #2 observation shares the archive turn id (one id per turn).
        let obs = m.observations.recent(1).unwrap();
        assert_eq!(obs[0].turn_id, "turn-test-1");
        std::fs::remove_dir_all(&root).ok();
    }

    /// Build an engine wired to a fresh temp chat-zone memory with a test
    /// embedder (so archived rows carry vectors, as in production).
    fn engine_with_memory(root: &std::path::Path) -> Engine {
        use lingmiao_memory::{HashingEmbedder, Memory, Zone};
        let paths = lingmiao_core::paths::Paths::at(root);
        paths.ensure_dirs().unwrap();
        let mem = Memory::open(&paths, Zone::Chat, Some(Arc::new(HashingEmbedder::new())))
            .expect("open memory");
        let client = Client::new(lingmiao_llm::ModelSpec::openai(
            "test-key",
            "http://localhost:1",
            "test-model",
        ))
        .unwrap();
        let mut engine = Engine::new(client, Arc::new(EventBus::default()));
        engine.memory = Some(Arc::new(mem));
        engine
    }

    fn sample_reply() -> LlmResponse {
        LlmResponse {
            content: "answer".to_string(),
            reasoning_content: "because reasons".to_string(),
            usage: Usage {
                input_tokens: 9,
                output_tokens: 4,
                total_tokens: 13,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn record_turn_archives_the_full_turn() {
        // ③ E存档: a completed turn archives system prompt / injected context /
        // raw messages / reasoning / tool transcript — not a five-field shell.
        let root = std::env::temp_dir().join(format!("lingmiao-eng-full-{}", std::process::id()));
        let engine = engine_with_memory(&root);
        let resp = sample_reply();
        let tool_calls = r#"[{"name":"read_file","arguments":{"path":"a"},"result":"file body"}]"#;
        let full_messages = r#"[{"role":"user","content":"hi"}]"#;
        engine.record_turn(
            "turn-full-1",
            "hi",
            &resp,
            &TurnRecordInput {
                system_prompt: "SYS",
                context_prefix: "CTX",
                full_messages,
                reasoning: &resp.reasoning_content,
                tool_calls,
            },
        );
        let m = engine.memory.as_ref().unwrap();
        let row = &m.archive.recent(1).unwrap()[0];
        assert_eq!(row.id, "turn-full-1");
        assert_eq!(row.system_prompt, "SYS");
        assert_eq!(row.context_prefix, "CTX");
        assert_eq!(row.reasoning, "because reasons");
        assert_eq!(row.tokens_total, 13);
        // The JSON columns are parseable and carry the real transcript.
        assert!(serde_json::from_str::<Value>(&row.full_messages).is_ok());
        let calls = serde_json::from_str::<Value>(&row.tool_calls).unwrap();
        assert_eq!(calls[0]["name"], "read_file");
        assert_eq!(calls[0]["result"], "file body");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn summary_backfill_updates_the_archive_row() {
        // ③ E存档: 沉淀阶段 backfills the turn's `summary` via a targeted UPDATE
        // that preserves the row's embedding (no re-embed side effect).
        let root = std::env::temp_dir().join(format!("lingmiao-eng-sum-{}", std::process::id()));
        let engine = engine_with_memory(&root);
        engine.record_turn(
            "turn-sum-1",
            "hi",
            &sample_reply(),
            &TurnRecordInput {
                system_prompt: "SYS",
                context_prefix: "",
                full_messages: "[]",
                reasoning: "",
                tool_calls: "[]",
            },
        );
        let m = engine.memory.as_ref().unwrap();
        // The saved row already carries an embedding (test embedder wired).
        assert_eq!(m.archive.missing_embeddings().unwrap(), 0);

        engine.backfill_summary("turn-sum-1", "本轮完成 X");
        assert_eq!(m.archive.recent(1).unwrap()[0].summary, "本轮完成 X");
        // The UPDATE did not clear the embedding column.
        assert_eq!(m.archive.missing_embeddings().unwrap(), 0);
        // An empty summary (e.g. a failed 沉淀阶段) is a no-op, not a wipe.
        engine.backfill_summary("turn-sum-1", "");
        assert_eq!(m.archive.recent(1).unwrap()[0].summary, "本轮完成 X");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn chain_id_is_constant_and_seq_increments() {
        // ③ E存档: turns of one engine share a chain id and differ by chain_seq.
        let root = std::env::temp_dir().join(format!("lingmiao-eng-chain-{}", std::process::id()));
        let engine = engine_with_memory(&root);
        let rec = TurnRecordInput {
            system_prompt: "SYS",
            context_prefix: "",
            full_messages: "[]",
            reasoning: "",
            tool_calls: "[]",
        };
        engine.record_turn("turn-c-1", "one", &sample_reply(), &rec);
        engine.record_turn("turn-c-2", "two", &sample_reply(), &rec);
        let m = engine.memory.as_ref().unwrap();
        let row1 = m.archive.get("turn-c-1").unwrap().unwrap();
        let row2 = m.archive.get("turn-c-2").unwrap().unwrap();
        assert!(row1.chain_id.starts_with("chain-"), "{}", row1.chain_id);
        assert_eq!(
            row1.chain_id, row2.chain_id,
            "chain id is constant per engine"
        );
        assert_eq!(row1.chain_seq, 1);
        assert_eq!(row2.chain_seq, 2);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn summary_items_are_persisted_and_stage_i_evolves_the_graph() {
        use lingmiao_memory::{Memory, Zone};
        use std::sync::Mutex as StdMutex;

        let root = std::env::temp_dir().join(format!("lingmiao-eng-i-{}", std::process::id()));
        let paths = lingmiao_core::paths::Paths::at(&root);
        paths.ensure_dirs().unwrap();
        let mem = Memory::open(&paths, Zone::Chat, None).expect("open memory");

        let bus = Arc::new(EventBus::default());
        let seen: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
        let sink = seen.clone();
        bus.listen(move |e| {
            if let Ok(mut v) = sink.lock() {
                v.push(e.event_type().to_string());
            }
        });

        let client = Client::new(lingmiao_llm::ModelSpec::openai(
            "test-key",
            "http://localhost:1",
            "test-model",
        ))
        .unwrap();
        let mut engine = Engine::new(client, bus);
        engine.memory = Some(Arc::new(mem));

        // 沉淀阶段 proposed two memory-worthy items (原版 F-要点记录 items).
        let raw = json!({
            "items": [
                {"kind": "fact", "topic": "rust", "name": "rust-mem",
                 "content": "rust knowledge graph evolves from observations"},
                {"kind": "decision", "topic": "arch", "name": "arch-decide",
                 "content": "use union-find for community detection instead of leiden"}
            ]
        });
        let report = SummaryReport::from_json(&raw);
        assert_eq!(engine.persist_summary_observations(&report, "turn-i-1"), 2);

        // Stage I reads those back and (with an empty graph) adds both as nodes.
        let mg = engine.stage_i().expect("memory wired → Some");
        assert_eq!(mg.observations, 2, "{mg:?}");
        assert_eq!(mg.nodes_added, 2, "{mg:?}");

        let m = engine.memory.as_ref().unwrap();
        assert_eq!(m.knowledge.stats().unwrap().nodes, 2);
        // The stage emits its started / result / mg_updated events.
        let events = seen.lock().unwrap().clone();
        assert!(events.iter().any(|e| e == "stage_started"));
        assert!(events.iter().any(|e| e == "stage_result_reported"));
        assert!(events.iter().any(|e| e == "mg_updated"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn stage_i_without_memory_is_a_noop() {
        let client = Client::new(lingmiao_llm::ModelSpec::openai(
            "test-key",
            "http://localhost:1",
            "test-model",
        ))
        .unwrap();
        let bus = Arc::new(EventBus::default());
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let h2 = hits.clone();
        bus.listen(move |_| {
            h2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        });
        let engine = Engine::new(client, bus);
        assert!(engine.stage_i().is_none());
        // No memory → no events emitted.
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[test]
    fn handoff_event_reports_the_real_selection_and_format() {
        // cli 2026-09-30: the A→B notice must describe *what was picked* and *how*
        // it reaches 工作阶段 — counts off the stage's own JSON, format off the very
        // constants the injection uses (so the text can never drift from the wire).
        let b_out = json!({
            "loadObservations": ["obs-a", "obs-b", "obs-c"],
            "loadNodes": ["node-1"],
            "loadRecentTurns": 3,
            "reasoning": "…"
        });
        let ev = handoff_event(&b_out, 1234, 3);
        match ev {
            Event::ContextHandoff {
                from,
                to,
                observations,
                nodes,
                recent_turns,
                chars,
                prefix,
                role,
                position,
            } => {
                assert_eq!(from, STAGE_B);
                assert_eq!(to, STAGE_C);
                assert_eq!(observations, 3);
                assert_eq!(nodes, 1);
                assert_eq!(recent_turns, 3);
                assert_eq!(chars, 1234);
                assert_eq!(prefix, CONTEXT_INJECT_PREFIX);
                assert_eq!(role, CONTEXT_INJECT_ROLE);
                assert_eq!(position, CONTEXT_INJECT_POSITION);
                // The engine is stateless, so there is no carried-over history —
                // the block rides the **system** prompt (2026-10-05 B 项).
                assert_eq!(position, "工作阶段 system 尾部");
                // The reported format is exactly what `render_stage_system` fills
                // into the receiver's `## 上下文` slot.
                assert_eq!(prefix, "## 上下文");
            }
            other => panic!("expected a handoff event, got {other:?}"),
        }
        // A stage that returned prose instead of the JSON still yields a notice
        // (all zeros) rather than a panic — the pipeline degrades, never breaks.
        let ev = handoff_event(&Value::String("no json here".into()), 0, 0);
        assert_eq!(ev.event_type(), "context_handoff");
    }

    #[tokio::test]
    async fn run_turn_emits_stage_and_summary_events_in_order() {
        use std::sync::Mutex as StdMutex;
        let bus = Arc::new(EventBus::default());
        let seen: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
        let sink = seen.clone();
        bus.listen(move |e| {
            let stage = match e {
                Event::StageStarted { stage, .. } => stage.clone(),
                Event::StageResultReported { stage, .. } => stage.clone(),
                Event::SummaryReported { stage, .. } => stage.clone(),
                _ => String::new(),
            };
            if let Ok(mut v) = sink.lock() {
                v.push(format!("{}:{stage}", e.event_type()));
            }
        });

        let client = Client::new(lingmiao_llm::ModelSpec::openai(
            "test-key",
            "http://localhost:1",
            "test-model",
        ))
        .unwrap();
        let engine = Engine::new(client, bus);
        // Every stage fails (unreachable endpoint), yet the pipeline still
        // runs and reports — the Q7 Result error boundary.
        let report = engine.run_turn("hello").await.expect("turn stays Ok");
        assert!(!report.error.is_empty(), "summary reports the fault");

        let events = seen.lock().unwrap().clone();
        let pos = |needle: &str| {
            events
                .iter()
                .position(|e| e == needle)
                .unwrap_or(usize::MAX)
        };
        let b = pos("stage_started:组织上下文");
        let c = pos("stage_started:工作阶段");
        let s = pos("stage_started:沉淀阶段");
        let sum = pos("summary_reported:沉淀阶段");
        assert!(
            b < c && c < s && s < sum,
            "unexpected event order: {events:?}"
        );
    }

    /// ⑤ MCP 接上: a server that cannot spawn (bad command) or is disabled
    /// (`{}` entry) never fails the attach — it is warn-skipped and the registry
    /// is left untouched. This is the failure path `attach_mcp` delegates to.
    #[tokio::test]
    async fn attach_mcp_servers_skips_broken_and_disabled_servers() {
        use lingmiao_core::config::McpServer;
        let reg = ToolRegistry::new();
        let servers = vec![
            (
                "broken".to_string(),
                McpServer {
                    command: "lingmiao-not-a-real-mcp-server-xyz".to_string(),
                    args: vec!["--stdio".to_string()],
                    env: Default::default(),
                },
            ),
            // Disabled convention: an empty `{}` entry carries no command.
            ("disabled".to_string(), McpServer::default()),
        ];
        let added = attach_mcp_servers(&reg, &servers).await;
        assert_eq!(
            added, 0,
            "no tools should come from broken/disabled servers"
        );
        assert!(reg.is_empty(), "registry must be untouched on failure");
    }
}

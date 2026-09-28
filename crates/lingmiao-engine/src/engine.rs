//! The QL turn engine — M1 vertical slice.
//!
//! M1 implements one honest end-to-end turn: user text → streaming LLM call →
//! `llm_delta` events on the bus → assembled reply. This is the seed the M4
//! pipeline grows from (decision Q7 keeps each stage an `async fn` over a
//! `TurnContext`; M1 is the single `C`-stage skeleton).
//!
//! Design notes:
//! * History lives in the engine behind a `Mutex`, snapshotted per turn — the
//!   lock is never held across an `await`, so a std mutex is safe here.
//! * Every observable step is emitted through the shared [`EventBus`], so the
//!   TUI (and later the auditor) observe the same stream the Python event bus
//!   produced.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lingmiao_core::events::{Event, Usage};
use lingmiao_core::{Config, EventBus, LingmiaoError};
use lingmiao_llm::{
    ChatRequest, Client, LlmResponse, Message, ModelRegistry, StreamAggregator, StreamDelta,
    UserConfig,
};
use lingmiao_tools::ToolRegistry;
use serde_json::{Value, json};

use crate::context::{SummaryReport, TurnContext};
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

/// The literal prefix [`Engine::stage_c`] puts in front of the `组织上下文`
/// block when it injects that block into the conversation request.
///
/// Named (rather than an inline literal at the injection site) because the A→B
/// **handoff notice** ([`Event::ContextHandoff`]) must report the *real* format
/// to the UI — a copy-pasted string in the TUI could silently drift from what
/// actually went on the wire. cli 2026-09-30.
pub const CONTEXT_INJECT_PREFIX: &str = "[上下文检索]";

/// Wire role of the injected context message: the receiving stage sends it as a
/// **`user`** message (原版对齐 — the selection is framed as part of the
/// user's request, never as a system instruction). Reported verbatim by
/// [`Event::ContextHandoff`].
pub const CONTEXT_INJECT_ROLE: &str = "user";

/// Where the injected context message sits in the receiving stage's message
/// list: after the snapshotted conversation history and immediately before this
/// turn's raw user input. Reported verbatim by [`Event::ContextHandoff`].
pub const CONTEXT_INJECT_POSITION: &str = "对话历史之后、本轮提问之前";

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
    history: Mutex<Vec<Message>>,
    system: String,
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
            history: Mutex::new(Vec::new()),
            system: String::new(),
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
        let raw = cfg.prompt(
            lingmiao_core::config::stage_prompt_key(STAGE_C),
            lingmiao_core::config::PromptField::System,
        );
        let cache_rel = lingmiao_core::brand::cache_rel();
        let env = fill_prompt(
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
        let system = if raw.is_empty() {
            String::new()
        } else {
            let mut s = fill_prompt(
                raw,
                &[
                    ("version", lingmiao_core::VERSION),
                    ("cwd", &cwd),
                    ("model", llm.model()),
                    ("local_time", &now_iso()),
                    ("startup_time", &now_iso()),
                    ("brand", lingmiao_core::brand::NAME),
                    ("memory_dir", lingmiao_core::brand::MEMORY_DIR),
                    ("cache_dir", &cache_rel),
                    ("env_prefix", lingmiao_core::brand::ENV_PREFIX),
                ],
            );
            if !env.is_empty() {
                s.push('\n');
                s.push_str(&env);
            }
            s
        };
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
            history: Mutex::new(Vec::new()),
            system,
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

    /// Drop all conversation history.
    pub fn clear_history(&self) {
        if let Ok(mut h) = self.history.lock() {
            h.clear();
        }
    }

    /// Number of stored messages.
    pub fn history_len(&self) -> usize {
        self.history.lock().map(|h| h.len()).unwrap_or(0)
    }

    /// Run one turn: stream the reply, emit events, return the assembled response.
    pub async fn turn(&self, input: &str) -> Result<LlmResponse, LingmiaoError> {
        self.bus.push(Event::StageStarted {
            stage: STAGE_C.to_string(),
            ts: now_iso(),
        });

        // Snapshot history with the new user message appended.
        let messages = {
            let mut h = self
                .history
                .lock()
                .map_err(|_| LingmiaoError::fatal("engine history poisoned"))?;
            h.push(Message::user(input));
            h.clone()
        };

        // Snapshot the call's shape *before* the request consumes `messages`, so
        // a failure can render an accurate diagnostic panel (`error_classify`).
        // The M1 single-stage path is the `工作阶段` stage, so it honours a
        // `config.json` route for `工作阶段` when one is declared.
        let client = self.llm_for(STAGE_C);
        let params_snapshot =
            lingmiao_llm::collect_llm_params(Some(client), &self.system, &messages, 0, true);
        // ③ E存档: capture the raw message list before the request consumes it.
        let full_messages = messages_to_json(&messages);
        let req = ChatRequest::chat(self.system.clone(), messages);
        let started = Instant::now();

        let mut rx = client.chat_stream(req).await?;
        let mut agg = StreamAggregator::default();
        let mut stream_error: Option<String> = None;

        while let Some(delta) = rx.recv().await {
            match &delta {
                StreamDelta::Content(text) => self.bus.push(Event::LlmDelta {
                    stage: STAGE_C.to_string(),
                    kind: "content".to_string(),
                    text: text.clone(),
                }),
                StreamDelta::Reasoning(text) => self.bus.push(Event::LlmDelta {
                    stage: STAGE_C.to_string(),
                    kind: "reasoning".to_string(),
                    text: text.clone(),
                }),
                StreamDelta::Error(e) => stream_error = Some(e.clone()),
                _ => {}
            }
            agg.apply(&delta);
        }

        let resp = agg.finish();
        {
            let mut h = self
                .history
                .lock()
                .map_err(|_| LingmiaoError::fatal("engine history poisoned"))?;
            h.push(Message::assistant(resp.content.clone()));
        }

        // Classify a mid-stream failure (`error_classify`) into a user-facing
        // diagnostic panel — category label + advice + the call params — instead
        // of surfacing the raw transport string.
        let fault_detail = stream_error
            .as_deref()
            .map(|e| {
                let info = lingmiao_llm::classify_llm_error(None, e);
                lingmiao_llm::format_error_panel(&info, &params_snapshot, e, "AI 服务调用失败")
            })
            .unwrap_or_default();
        let ok = stream_error.is_none();
        self.bus.push(Event::StageResultReported {
            stage: STAGE_C.to_string(),
            ok,
            data: json!({"content": resp.content}),
            fault_type: if ok { String::new() } else { "llm".to_string() },
            fault_detail: fault_detail.clone(),
            tokens: serde_json::to_value(&resp.usage).unwrap_or(Value::Null),
            tool_calls: resp.tool_calls.len() as u64,
            elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
        });

        if stream_error.is_some() {
            Err(LingmiaoError::llm(fault_detail, 0, 1))
        } else {
            // Persist the completed turn (#1 archive + #2 observation). The
            // single-stage path has no B-stage context prefix; its reasoning and
            // tool calls come straight off the reply.
            let turn_id = lingmiao_memory::short_id("turn");
            let tool_calls =
                serde_json::to_string(&resp.tool_calls).unwrap_or_else(|_| "[]".to_string());
            self.record_turn(
                &turn_id,
                input,
                &resp,
                &TurnRecordInput {
                    system_prompt: &self.system,
                    context_prefix: "",
                    full_messages: &full_messages,
                    reasoning: &resp.reasoning_content,
                    tool_calls: &tool_calls,
                },
            );
            Ok(resp)
        }
    }

    // ── M4 pipeline: 组织上下文 → 工作阶段 → 沉淀阶段 (Q7 / Q9) ──────

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
    }

    /// Render a stage's system prompt (system + skill + json_rule), filling the
    /// common placeholders. The `C` stage reuses the env-seeded [`Self::system`].
    fn render_stage_system(&self, stage: &str) -> String {
        if stage == STAGE_C {
            return self.system.clone();
        }
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
        s
    }

    /// Snapshot conversation history (never holds the lock across an await).
    fn snapshot_history(&self) -> Vec<Message> {
        self.history.lock().map(|h| h.clone()).unwrap_or_default()
    }

    /// `组织上下文` — select the context block for this turn, writing
    /// [`TurnContext::b_context`] plus the §8.5 per-source retrieval volumes.
    pub async fn stage_b(&self, ctx: &mut TurnContext) -> Result<(), LingmiaoError> {
        let system = self.render_stage_system(STAGE_B);
        let tools = ctx.registry.schemas_for(&self.stage_tools(STAGE_B));
        let messages = vec![Message::user(format!("用户请求：\n{}", ctx.input))];
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
        // cli 2026-09-30 — the A→B **handoff notice**. The stage has just chosen
        // its context; report *what* it picked and *how* the next stage receives
        // it, straight off the real values this function is about to store:
        // the counts come from the model's own `load…` JSON, and the format
        // description comes from the very constants `stage_c` injects with — so
        // the notice can never describe a contract the injection does not honour.
        self.bus
            .push(handoff_event(&out.json, out.content.chars().count() as u64));
        ctx.b_context = Some(out.content);
        Ok(())
    }

    /// `工作阶段` — the tool-enabled conversation loop, writing
    /// [`TurnContext::c_answer`] and persisting the turn.
    pub async fn stage_c(&self, ctx: &mut TurnContext) -> Result<(), LingmiaoError> {
        let system = self.render_stage_system(STAGE_C);
        let tools = ctx.registry.schemas_for(&self.stage_tools(STAGE_C));
        let mut messages = ctx.history.clone();
        // The receiver's half of the A→B contract: the block is appended **after**
        // the snapshotted history (i.e. right before this turn's user input) with
        // [`CONTEXT_INJECT_PREFIX`], as a `user` message. The handoff notice
        // ([`Event::ContextHandoff`]) reports exactly this shape.
        let history_chars: usize = ctx
            .history
            .iter()
            .map(|m| m.content.as_deref().unwrap_or("").chars().count())
            .sum();
        let b_context = ctx.b_context.as_deref().filter(|b| !b.trim().is_empty());
        if let Some(b) = b_context {
            messages.push(Message::user(format!("{CONTEXT_INJECT_PREFIX}\n{b}")));
        }
        messages.push(Message::user(ctx.input.clone()));

        // §8.5 上下文组成条 — account the *real* per-section character counts of
        // this first injection (user-facing names, 去黑话). The B-stage retrieval
        // is split by source into 知识记忆 (KG hits) and 历史观测 (observation /
        // archive hits) — matching the six-segment design in ux-design.md §8.5/§8.6
        // and `examples/preview.rs`, rather than collapsing both into one
        // 上下文检索 segment. `lock` is the composed base+stage core lock the
        // stage agent re-injects (原版对齐). The UI derives each section's
        // tokens as `chars × (provider_input_tokens ÷ total_chars)`, so the parts
        // always sum to the provider-measured total.
        let lock_chars = self.cfg.full_core_lock(STAGE_C).chars().count();
        let sections = context_sections(
            system.chars().count(),
            lock_chars,
            ctx.b_knowledge_chars,
            ctx.b_history_chars,
            history_chars,
            tools.to_string().chars().count(),
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

        // Commit history + persist (#1 archive + #2 observation).
        if let Ok(mut h) = self.history.lock() {
            h.push(Message::user(ctx.input.clone()));
            h.push(Message::assistant(out.content.clone()));
        }
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
        let system = self.render_stage_system(STAGE_SUMMARY);
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
        let mut ctx = TurnContext::new(
            input.to_string(),
            self.snapshot_history(),
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
            let outcome =
                tokio::time::timeout(MCP_ATTACH_TIMEOUT, attach_one_mcp_server(&server)).await;
            (name, outcome)
        });
    }
    let mut added = 0;
    while let Some(joined) = tasks.join_next().await {
        match joined {
            Ok((name, Ok(Ok(tools)))) => {
                let n = tools.len();
                for tool in tools {
                    registry.register_arc_remote(Arc::new(tool));
                }
                tracing::info!("mcp: `{name}` attached ({n} tools)");
                added += n;
            }
            Ok((name, Ok(Err(e)))) => tracing::warn!("mcp: `{name}` skipped: {e}"),
            Ok((name, Err(_))) => tracing::warn!(
                "mcp: `{name}` skipped: timed out after {}s",
                MCP_ATTACH_TIMEOUT.as_secs()
            ),
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

/// Build the A→B **handoff event** from the context stage's own JSON output and
/// the length of the block it selected.
///
/// Kept a free function (like [`summary_event`]) so the shape is testable
/// without a live model: feed it the real `load…` payload and the same
/// constants [`Engine::stage_c`] injects with, and assert the notice adds up.
fn handoff_event(b_out: &Value, chars: u64) -> Event {
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
        // A bare scalar, not a list — read directly.
        recent_turns: b_out
            .get("loadRecentTurns")
            .and_then(Value::as_u64)
            .unwrap_or(0),
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
            history: Mutex::new(Vec::new()),
            system: String::new(),
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
        let rendered = fill_prompt(
            raw,
            &[
                ("brand", lingmiao_core::brand::NAME),
                ("memory_dir", lingmiao_core::brand::MEMORY_DIR),
                ("cache_dir", &lingmiao_core::brand::cache_rel()),
                ("env_prefix", lingmiao_core::brand::ENV_PREFIX),
                ("version", lingmiao_core::VERSION),
            ],
        );
        assert!(rendered.contains(&format!("你是 {} 编程助手", lingmiao_core::brand::NAME)));
        assert!(!rendered.contains("{brand}"));
        assert!(!rendered.contains("{memory_dir}"));
        assert!(!rendered.contains("{cache_dir}"));
        assert!(!rendered.contains("{env_prefix}"));
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
    fn engine_tracks_history_len() {
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
        assert_eq!(engine.history_len(), 0);
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
        let ev = handoff_event(&b_out, 1234);
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
                // The reported format is exactly what `stage_c` builds the
                // injected message with — `{prefix}\n{block}`.
                assert_eq!(format!("{prefix}\nBLOCK"), "[上下文检索]\nBLOCK");
            }
            other => panic!("expected a handoff event, got {other:?}"),
        }
        // A stage that returned prose instead of the JSON still yields a notice
        // (all zeros) rather than a panic — the pipeline degrades, never breaks.
        let ev = handoff_event(&Value::String("no json here".into()), 0);
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

//! Event protocol — ported from `core/events.py`.
//!
//! Q8 decision: the Python `@dataclass` events become **one [`Event`] enum**.
//! Q9 (2026-09-16) folded the four stage-result events of the now-combined
//! C_obs / D / F / G / H stages (对话观测/任务追踪/要点记录/上下文审计/内容质检)
//! into a single `summary_reported`, so the enum holds **14 variants** (the
//! eleventh, `context_usage`, was added for 需求①'s real context-composition
//! bar; the twelfth, `context_handoff`, for the 2026-09-30 A→B handoff notice;
//! the thirteenth, `wait_polled`, for the 2026-10-05 F 项
//! 「等待过程要进界面」轮询可见化; the fourteenth, `auto_notice`, for the
//! 2026-10-04 自动模式（`/auto`）进度提示).
//! The original transported events as loosely-typed dicts
//! (`{"event_type": "tool_called", ...}`) and its listener loop silently swallowed
//! exceptions (`logger.exception(...)` then continue). In Rust every consumer
//! matches exhaustively, so a forgotten event type is a *compile* error.
//!
//! The bus itself is intentionally simple (as in the original): no pub/sub
//! registry, just log + fan-out. The one addition is a `tokio::broadcast` channel
//! so async consumers (the TUI) can subscribe without a callback.

use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio::sync::broadcast;

/// Token + tool-call accounting for an LLM call sequence.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    /// Prompt tokens.
    #[serde(default)]
    pub input_tokens: u64,
    /// Completion tokens.
    #[serde(default)]
    pub output_tokens: u64,
    /// Total tokens.
    #[serde(default)]
    pub total_tokens: u64,
    /// Per-call tool-call records.
    #[serde(default)]
    pub tool_calls: Vec<Value>,
}

/// Every observable thing in the pipeline, in one exhaustive enum.
#[derive(Debug, Clone)]
pub enum Event {
    /// A stage begins execution.
    StageStarted { stage: String, ts: String },
    /// One LLM API round-trip.
    LlmResponse {
        stage: String,
        content: String,
        tool_calls: Vec<Value>,
        tool_count: u64,
        tools: Vec<Value>,
        reasoning_content: String,
        finish_reason: String,
        usage: Usage,
    },
    /// A tool is about to run (before execution).
    ToolStarted {
        stage: String,
        tool: String,
        args: Value,
        call_id: String,
    },
    /// A tool was invoked and returned a result.
    ToolCalled {
        stage: String,
        tool: String,
        args: Value,
        result_preview: String,
        ms: f64,
        error: bool,
        call_id: String,
        /// **Display-only** unified diff of a file mutation (CC's
        /// `structuredPatch`): `[{"kind":"add"|"remove"|"context"|"hunk",
        /// "text":"…"}]`, empty (`[]`) for every non-file / non-mutating call.
        ///
        /// cli 2026-09-28 (「参照 CC 实现代码改动时的红绿对比格式显示样式，包括写入
        /// 的时候也是」): the TUI paints the added lines green and the removed ones
        /// red — exactly like CC's `Edit` / `Write` tool cards — while the
        /// model-facing tool result stays the short sentence (`The file X has been
        /// updated successfully.`, CC's own wording). So the diff is a *view*, not
        /// part of the model's context.
        diff: Value,
    },
    /// A chunk of streaming output from a long-running tool.
    ToolOutput {
        stage: String,
        tool: String,
        chunk: String,
        stream: String,
        call_id: String,
    },
    /// A streaming token chunk from the LLM (`kind`: `content` | `reasoning`).
    LlmDelta {
        stage: String,
        kind: String,
        text: String,
    },
    /// A new turn in the C-stage tool loop.
    TurnStarted { chain_id: String, seq: u64 },
    /// A `StageAgent.work()` call completed.
    StageResultReported {
        stage: String,
        ok: bool,
        data: Value,
        fault_type: String,
        fault_detail: String,
        tokens: Value,
        tool_calls: u64,
        elapsed_ms: f64,
    },
    /// C-stage prompt composition — real per-section *character* counts of the
    /// first injection (需求① §8.5 上下文组成条). The UI derives per-section
    /// tokens as `chars × (provider_input_tokens ÷ total_chars)`, so the parts
    /// always sum to the provider-measured total.
    ContextUsage {
        stage: String,
        /// `[{ "name": "...", "chars": N }]`, in display order.
        sections: Value,
        /// Sum of `chars` across `sections`.
        total_chars: u64,
    },
    /// **`组织上下文` › `工作阶段` handoff** — *what* the context-selection stage
    /// picked, and *how* it reaches the conversation stage.
    ///
    /// cli 2026-09-30 (「增加一个机制，在A阶段结束后用一段简短的提示信息表达A阶段
    /// 的结果，并表明已经把这些给了B阶段，并且是按什么格式进去上下文的」): the TUI
    /// can only report the handoff truthfully if the side that owns the injection
    /// describes it. Every field is read off the real path — the counts come from
    /// the stage's parsed JSON (`loadObservations` / `loadNodes`) and the archive
    /// lookup that actually supplied the turns, `chars` from the injected block's
    /// own length, and `prefix` / `role` / `position` from the very constants and
    /// message order that `Engine::stage_c` uses to build the request — never from
    /// a UI-side copy of the contract.
    ContextHandoff {
        /// The stage that produced the context (`组织上下文`).
        from: String,
        /// The stage that receives it (`工作阶段`).
        to: String,
        /// Observation records it chose to load (`loadObservations` length).
        observations: u64,
        /// Knowledge-graph nodes it loaded (`loadNodes` length).
        nodes: u64,
        /// Archive turns actually injected — read off the assembled block, not
        /// off the model's `loadRecentTurns` request (an empty archive injects
        /// zero even when 3 were asked for).
        recent_turns: u64,
        /// Characters of the selected block that go on the wire. `0` means the
        /// receiving stage injects **nothing** this turn.
        chars: u64,
        /// The literal prefix the receiving stage uses (`## 上下文` — the section
        /// heading inside the **system** prompt; 2026-10-05 B 项 re-pointed this
        /// from the removed user-message copy).
        prefix: String,
        /// Wire role carrying the block (`system`).
        role: String,
        /// Where the block sits (`工作阶段 system 尾部`: it rides the system
        /// prompt — the engine is stateless, so there is no carried-over history
        /// for a user message to sit behind).
        position: String,
    },
    /// 沉淀阶段 result — the merged C_obs + D + F + G + H output (Q9).
    ///
    /// Replaces the four 1:1 Python events `task_tracked` / `observations_extracted`
    /// / `context_audited` / `turn_assessed`.
    SummaryReported {
        stage: String,
        prompt: String,
        response: String,
        turn_type: String,
        summary: String,
        observations: u64,
        task_status: String,
        next_steps: String,
        audit_grade: String,
        issues: u64,
        quality_grade: String,
        corrections: u64,
        error: String,
    },
    /// I stage result — knowledge graph nodes updated.
    MgUpdated {
        nodes_updated: u64,
        communities: u64,
    },
    /// **A polled wait's heartbeat** — one report per 5s sample of the F 项 wait
    /// poll, so the wait is *visible in the UI* instead of living only in the log.
    ///
    /// cli 2026-10-05（「要进界面的，这是核心体验」）: the poll was running and
    /// deciding correctly, but nothing about it reached the transcript — the only
    /// trace was a `wait judge ruling` tracing line in `.cache/lingmiao/logs/`.
    /// From the user's seat a 60-second `cargo fmt` therefore still looked like a
    /// silent hang. This event is the missing half: every heartbeat is pushed on
    /// the bus, and the TUI renders it as a live line under the running card.
    ///
    /// Emitted by `lingmiao_core::polling` — the only place that samples a wait —
    /// so the UI can never disagree with what the poll actually saw. Only armed
    /// waits report (no judge in force = no polling = no events, the pre-polling
    /// contract).
    WaitPolled {
        /// Class of the thing being waited on (`模型` / `外部命令` / `网络服务` /
        /// `数据库/阶段` — [`crate::polling::WaitClass::as_str`]).
        class: String,
        /// What is being waited on (e.g. `bash: cargo fmt --all --check`).
        what: String,
        /// How long the wait has run, in milliseconds.
        elapsed_ms: u64,
        /// How long since the last sign of progress, in milliseconds (the number
        /// the judge rules on: `0` means data is still arriving).
        silent_ms: u64,
        /// What this heartbeat is: `sampling` (collected, below the cost gate) /
        /// `asking` (handed to the judge) / `continue` / `interrupt` / `done`.
        phase: String,
        /// The judge's stated reason when `phase == "interrupt"`, else empty.
        detail: String,
    },
    /// **自动模式（`/auto`）进度提示** — 一条面向用户的一行话。
    ///
    /// cli 2026-10-04（「自动模式沿用 python 原版机制」）：原版靠
    /// `AutonomousLoop._emit()` 往主 TUI pipe 推 `ChatOut`（`▶ 启动` /
    /// `🤖 Main·第N轮 — 开始` / `🔍 Auditor·第N轮｜continue=…` / `✅ 完成`）。
    /// Rust 侧是**单一 `enum Event`**（Q8），所以自动模式的一行话走这个变体：
    /// TUI 按 `tag`（`message` / `status` / `error`）决定渲染成正文、灰色提示
    /// 还是错误行 —— 与引擎自身阶段事件走同一条总线、同一个事件循环。
    AutoNotice {
        /// `message` | `status` | `error`。
        tag: String,
        /// 提示正文（已是给人读的中文）。
        text: String,
        /// 该提示所处的**轮次**（1-based；`0` = 与某一轮无关，如「▶ 启动」）。
        ///
        /// cli 2026-10-04（「自动模式需要显示轮次，也就是到第几轮了」）：自动模式
        /// 是一条没有硬轮数上限的循环，屏幕上只有随对话流滚走的
        /// 「🤖 Main·第N轮 — 开始」。这一字段让 TUI 把**当前轮次**钉在一个持续可见
        /// 的位置（footer 左槽 + 活动行），不用回头翻找。引擎在 Main / Auditor 开始
        /// 处带上 `session.current_iteration`，其余提示留 `0`。
        iteration: u64,
    },
}

/// The variant → wire-name table, in declaration order (需求⑤ `events` view).
///
/// A single static mirror of [`Event`] for the meta platform to enumerate
/// without constructing every variant. The `tests` module asserts it stays in
/// lock-step with [`Event::event_type`].
pub const EVENT_VARIANTS: [(&str, &str); 14] = [
    ("StageStarted", "stage_started"),
    ("LlmResponse", "llm_response"),
    ("ToolStarted", "tool_started"),
    ("ToolCalled", "tool_called"),
    ("ToolOutput", "tool_output"),
    ("LlmDelta", "llm_delta"),
    ("TurnStarted", "turn_started"),
    ("StageResultReported", "stage_result_reported"),
    ("ContextUsage", "context_usage"),
    ("ContextHandoff", "context_handoff"),
    ("SummaryReported", "summary_reported"),
    ("MgUpdated", "mg_updated"),
    ("WaitPolled", "wait_polled"),
    ("AutoNotice", "auto_notice"),
];

impl Event {
    /// The stable wire name (`event_type`), identical to the Python values.
    pub fn event_type(&self) -> &'static str {
        match self {
            Event::StageStarted { .. } => "stage_started",
            Event::LlmResponse { .. } => "llm_response",
            Event::ToolStarted { .. } => "tool_started",
            Event::ToolCalled { .. } => "tool_called",
            Event::ToolOutput { .. } => "tool_output",
            Event::LlmDelta { .. } => "llm_delta",
            Event::TurnStarted { .. } => "turn_started",
            Event::StageResultReported { .. } => "stage_result_reported",
            Event::ContextUsage { .. } => "context_usage",
            Event::ContextHandoff { .. } => "context_handoff",
            Event::SummaryReported { .. } => "summary_reported",
            Event::MgUpdated { .. } => "mg_updated",
            Event::WaitPolled { .. } => "wait_polled",
            Event::AutoNotice { .. } => "auto_notice",
        }
    }

    /// Project the event into the flat dict the original `_to_dict()` produced
    /// (`event_type` plus one key per dataclass field).
    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        m.insert("event_type".into(), Value::String(self.event_type().into()));
        let s = |v: &str| Value::String(v.to_string());
        match self {
            Event::StageStarted { stage, ts } => {
                m.insert("stage".into(), s(stage));
                m.insert("ts".into(), s(ts));
            }
            Event::LlmResponse {
                stage,
                content,
                tool_calls,
                tool_count,
                tools,
                reasoning_content,
                finish_reason,
                usage,
            } => {
                m.insert("stage".into(), s(stage));
                m.insert("content".into(), s(content));
                m.insert(
                    "tool_calls".into(),
                    serde_json::to_value(tool_calls).unwrap_or(Value::Array(vec![])),
                );
                m.insert("tool_count".into(), Value::from(*tool_count));
                m.insert(
                    "tools".into(),
                    serde_json::to_value(tools).unwrap_or(Value::Array(vec![])),
                );
                m.insert("reasoning_content".into(), s(reasoning_content));
                m.insert("finish_reason".into(), s(finish_reason));
                m.insert(
                    "usage".into(),
                    serde_json::to_value(usage).unwrap_or(Value::Null),
                );
            }
            Event::ToolStarted {
                stage,
                tool,
                args,
                call_id,
            } => {
                m.insert("stage".into(), s(stage));
                m.insert("tool".into(), s(tool));
                m.insert("args".into(), args.clone());
                m.insert("call_id".into(), s(call_id));
            }
            Event::ToolCalled {
                stage,
                tool,
                args,
                result_preview,
                ms,
                error,
                call_id,
                diff,
            } => {
                m.insert("stage".into(), s(stage));
                m.insert("tool".into(), s(tool));
                m.insert("args".into(), args.clone());
                m.insert("result_preview".into(), s(result_preview));
                m.insert("ms".into(), Value::from(*ms));
                m.insert("error".into(), Value::Bool(*error));
                m.insert("call_id".into(), s(call_id));
                m.insert("diff".into(), diff.clone());
            }
            Event::ToolOutput {
                stage,
                tool,
                chunk,
                stream,
                call_id,
            } => {
                m.insert("stage".into(), s(stage));
                m.insert("tool".into(), s(tool));
                m.insert("chunk".into(), s(chunk));
                m.insert("stream".into(), s(stream));
                m.insert("call_id".into(), s(call_id));
            }
            Event::LlmDelta { stage, kind, text } => {
                m.insert("stage".into(), s(stage));
                m.insert("kind".into(), s(kind));
                m.insert("text".into(), s(text));
            }
            Event::TurnStarted { chain_id, seq } => {
                m.insert("chain_id".into(), s(chain_id));
                m.insert("seq".into(), Value::from(*seq));
            }
            Event::StageResultReported {
                stage,
                ok,
                data,
                fault_type,
                fault_detail,
                tokens,
                tool_calls,
                elapsed_ms,
            } => {
                m.insert("stage".into(), s(stage));
                m.insert("ok".into(), Value::Bool(*ok));
                m.insert("data".into(), data.clone());
                m.insert("fault_type".into(), s(fault_type));
                m.insert("fault_detail".into(), s(fault_detail));
                m.insert("tokens".into(), tokens.clone());
                m.insert("tool_calls".into(), Value::from(*tool_calls));
                m.insert("elapsed_ms".into(), Value::from(*elapsed_ms));
            }
            Event::ContextUsage {
                stage,
                sections,
                total_chars,
            } => {
                m.insert("stage".into(), s(stage));
                m.insert("sections".into(), sections.clone());
                m.insert("total_chars".into(), Value::from(*total_chars));
            }
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
                m.insert("from".into(), s(from));
                m.insert("to".into(), s(to));
                m.insert("observations".into(), Value::from(*observations));
                m.insert("nodes".into(), Value::from(*nodes));
                m.insert("recent_turns".into(), Value::from(*recent_turns));
                m.insert("chars".into(), Value::from(*chars));
                m.insert("prefix".into(), s(prefix));
                m.insert("role".into(), s(role));
                m.insert("position".into(), s(position));
            }
            Event::SummaryReported {
                stage,
                prompt,
                response,
                turn_type,
                summary,
                observations,
                task_status,
                next_steps,
                audit_grade,
                issues,
                quality_grade,
                corrections,
                error,
            } => {
                m.insert("stage".into(), s(stage));
                m.insert("prompt".into(), s(prompt));
                m.insert("response".into(), s(response));
                m.insert("type".into(), s(turn_type));
                m.insert("summary".into(), s(summary));
                m.insert("observations".into(), Value::from(*observations));
                m.insert("task_status".into(), s(task_status));
                m.insert("next_steps".into(), s(next_steps));
                m.insert("audit_grade".into(), s(audit_grade));
                m.insert("issues".into(), Value::from(*issues));
                m.insert("quality_grade".into(), s(quality_grade));
                m.insert("corrections".into(), Value::from(*corrections));
                m.insert("error".into(), s(error));
            }
            Event::MgUpdated {
                nodes_updated,
                communities,
            } => {
                m.insert("nodes_updated".into(), Value::from(*nodes_updated));
                m.insert("communities".into(), Value::from(*communities));
            }
            Event::WaitPolled {
                class,
                what,
                elapsed_ms,
                silent_ms,
                phase,
                detail,
            } => {
                m.insert("class".into(), s(class));
                m.insert("what".into(), s(what));
                m.insert("elapsed_ms".into(), Value::from(*elapsed_ms));
                m.insert("silent_ms".into(), Value::from(*silent_ms));
                m.insert("phase".into(), s(phase));
                m.insert("detail".into(), s(detail));
            }
            Event::AutoNotice {
                tag,
                text,
                iteration,
            } => {
                m.insert("tag".into(), s(tag));
                m.insert("text".into(), s(text));
                m.insert("iteration".into(), Value::from(*iteration));
            }
        }
        Value::Object(m)
    }
}

/// Callback listener — receives a borrowed event.
pub type Listener = Arc<dyn Fn(&Event) + Send + Sync>;

/// Synchronous event bus with a `tokio::broadcast` fan-out.
pub struct EventBus {
    tx: broadcast::Sender<Event>,
    listeners: RwLock<Vec<Listener>>,
}

impl Default for EventBus {
    fn default() -> Self {
        // 4096 (was 1024): a single C-stage turn pushes thousands of streamed
        // `LlmDelta` events (reasoning + content chunks). A 1024-slot broadcast
        // ring can lag behind the TUI's repaint loop and silently drop the
        // reasoning stream — bumping the buffer makes `Lagged` far rarer (the
        // TUI *also* drains the backlog each tick, see `lingmiao-tui::run_loop`).
        Self::new(4096)
    }
}

impl EventBus {
    /// Create a bus whose broadcast channel buffers `capacity` events.
    pub fn new(capacity: usize) -> Self {
        let (tx, _rx) = broadcast::channel(capacity.max(1));
        Self {
            tx,
            listeners: RwLock::new(Vec::new()),
        }
    }

    /// Subscribe to the broadcast stream (async consumers, e.g. the TUI).
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }

    /// Register a synchronous callback listener.
    pub fn listen<F>(&self, f: F)
    where
        F: Fn(&Event) + Send + Sync + 'static,
    {
        if let Ok(mut listeners) = self.listeners.write() {
            listeners.push(Arc::new(f));
        }
    }

    /// Number of registered callbacks.
    pub fn listener_count(&self) -> usize {
        self.listeners.read().map(|l| l.len()).unwrap_or(0)
    }

    /// Log the event, fan it out to callbacks, then broadcast it.
    ///
    /// Send failures (no receivers) are ignored, exactly like the original's
    /// silent fan-out.
    pub fn push(&self, event: Event) {
        let payload = event.to_json();
        tracing::info!(
            target: crate::brand::EVENTS_TARGET,
            event_type = event.event_type(),
            payload = %payload,
            "event"
        );
        if let Ok(listeners) = self.listeners.read() {
            for listener in listeners.iter() {
                listener(&event);
            }
        }
        let _ = self.tx.send(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_thirteen_event_types_are_wired() {
        let events = [
            Event::StageStarted {
                stage: "C".into(),
                ts: "t".into(),
            },
            Event::LlmResponse {
                stage: "C".into(),
                content: String::new(),
                tool_calls: vec![],
                tool_count: 0,
                tools: vec![],
                reasoning_content: String::new(),
                finish_reason: String::new(),
                usage: Usage::default(),
            },
            Event::ToolStarted {
                stage: "C".into(),
                tool: "bash".into(),
                args: Value::Null,
                call_id: String::new(),
            },
            Event::ToolCalled {
                stage: "C".into(),
                tool: "bash".into(),
                args: Value::Null,
                result_preview: String::new(),
                ms: 1.0,
                error: false,
                call_id: String::new(),
                diff: Value::Array(vec![]),
            },
            Event::ToolOutput {
                stage: "C".into(),
                tool: "bash".into(),
                chunk: "x".into(),
                stream: "stdout".into(),
                call_id: String::new(),
            },
            Event::LlmDelta {
                stage: "C".into(),
                kind: "content".into(),
                text: "hi".into(),
            },
            Event::TurnStarted {
                chain_id: "c".into(),
                seq: 1,
            },
            Event::StageResultReported {
                stage: "沉淀阶段".into(),
                ok: true,
                data: Value::Null,
                fault_type: String::new(),
                fault_detail: String::new(),
                tokens: Value::Null,
                tool_calls: 0,
                elapsed_ms: 0.0,
            },
            Event::ContextUsage {
                stage: "工作阶段".into(),
                sections: Value::Array(vec![]),
                total_chars: 0,
            },
            Event::ContextHandoff {
                from: "组织上下文".into(),
                to: "工作阶段".into(),
                observations: 0,
                nodes: 0,
                recent_turns: 0,
                chars: 0,
                prefix: "## 上下文".into(),
                role: "system".into(),
                position: "工作阶段 system 尾部".into(),
            },
            Event::SummaryReported {
                stage: "沉淀阶段".into(),
                prompt: String::new(),
                response: String::new(),
                turn_type: "qa".into(),
                summary: String::new(),
                observations: 0,
                task_status: "idle".into(),
                next_steps: String::new(),
                audit_grade: "good".into(),
                issues: 0,
                quality_grade: "good".into(),
                corrections: 0,
                error: String::new(),
            },
            Event::MgUpdated {
                nodes_updated: 0,
                communities: 0,
            },
            Event::WaitPolled {
                class: "外部命令".into(),
                what: "bash: cargo fmt".into(),
                elapsed_ms: 40_000,
                silent_ms: 35_000,
                phase: "asking".into(),
                detail: String::new(),
            },
            Event::AutoNotice {
                tag: "message".into(),
                text: "▶ 自动模式启动".into(),
                iteration: 0,
            },
        ];
        assert_eq!(events.len(), 14);
        for ev in &events {
            let json = ev.to_json();
            assert_eq!(json["event_type"], Value::from(ev.event_type()));
        }
        // `EVENT_VARIANTS` mirrors the enum 1:1, in order (需求⑤ `events` view).
        assert_eq!(EVENT_VARIANTS.len(), events.len());
        for (idx, ((_, wire), ev)) in EVENT_VARIANTS.iter().zip(events.iter()).enumerate() {
            assert_eq!(ev.event_type(), *wire, "variant #{idx} drifted");
        }
    }

    #[test]
    fn bus_fans_out_to_callbacks_and_broadcast() {
        let bus = EventBus::default();
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hits2 = hits.clone();
        bus.listen(move |ev| {
            assert_eq!(ev.event_type(), "stage_started");
            hits2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        });
        let mut rx = bus.subscribe();
        bus.push(Event::StageStarted {
            stage: "A".into(),
            ts: "t".into(),
        });
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(rx.try_recv().unwrap().event_type(), "stage_started");
    }
}

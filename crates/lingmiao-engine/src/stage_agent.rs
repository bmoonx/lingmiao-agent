//! The reusable stage agent (Q7) — a model-driven tool loop with JSON output.
//!
//! Ported from the Python original's 664-line `StageAgent`. Each pipeline
//! stage runs through [`StageAgent::work`], which drives the model until it
//! stops calling tools, then extracts (or salvages) the JSON it produced.
//!
//! Four behaviours are load-bearing and deliberately *not* simplified into a
//! single bare LLM call:
//!
//! 1. **Model-driven tool loop** — the model returns `tool_calls`, they are
//!    executed against the [`ToolRegistry`] and fed back, until it stops.
//! 2. **JSON extraction** — body JSON first, ```` ```json ```` fenced block as
//!    a fallback.
//! 3. **Salvage** — when parsing fails outright, `"key": value` pairs are
//!    scraped out of the text rather than discarding the whole reply.
//! 4. **Runaway guard** — an overall `timeout` (per-stage, from `stages.json`)
//!    plus no-progress detection (identical consecutive tool-call sets) instead
//!    of an arbitrary `max_turns` cap.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use lingmiao_core::events::{Event, Usage};
use lingmiao_core::polling::{
    CodeStall, Guarded, PHASE_ASKING, PHASE_CONTINUE, PHASE_INTERRUPT, PHASE_SAMPLING,
    POLL_INTERVAL, Progress, Verdict, WaitClass, WaitJudge, global_policy, guard_capped, report,
    sample,
};
use lingmiao_core::{EventBus, LingmiaoError};
use lingmiao_llm::{
    ChatRequest, Client, LlmResponse, Message, StreamAggregator, ToolCall, ToolDef,
};
use lingmiao_tools::ToolRegistry;
use serde_json::{Map, Value, json};

/// How many characters of a tool result are carried in [`Event::ToolCalled`]'s
/// `result_preview` (display-only; the model still receives the full content via
/// `Message::tool`).
///
/// Kept comfortably above the TUI's tool-output fold threshold (§12.10, ~8
/// wrapped lines) so a real `read_file`/`grep`/`bash` result actually *does*
/// fold instead of always fitting on screen — a 400-char preview never reached
/// the fold, which would have made the feature dead in practice.
const RESULT_PREVIEW_CHARS: usize = 2000;

/// Hard ceiling on how many characters of a **single tool result** are fed back
/// to the model inside the tool loop.
///
/// The tool-result text is re-sent on every subsequent round-trip of the stage,
/// so one unbounded result is multiplied into the request again and again. This
/// matches the `bash`/`grep` output cap tools already honour (`BASH_OUTPUT_CAP` /
/// `GREP_OUTPUT_CAP` = 64 KiB) and is the engine-level backstop for a tool that
/// forgets to bound itself — `list_archive` serialised whole archive rows
/// (megabytes of `full_messages` / `tool_calls`) and turned the 沉淀阶段 request
/// into 1,622,701 tokens → `HTTP 400 [上下文超限]` (cli 2026-09-28, screenshot
/// 055716). The display preview already had its own cap; the *model feed* had
/// none, which is what this closes.
const TOOL_FEED_MAX_CHARS: usize = 64 * 1024;

/// F 项（cli 2026-10-05 拍板）· ① 等模型吐字的停摆门槛。
///
/// 这一处的被等对象**就是模型自己**：卡住时再发一条「还等不等」的决策请求，
/// 走的是同一条卡住的流——同一根电话线问不出去。cli 拍板接受此处例外，改由
/// **代码**判停摆：`silent ≥ 90s`（沿用原 `read_timeout` 的 90s，语义从
/// 「单次读取超时」改成「距上一次收到任何新数据超过 90s」）即中断并如实报错。
const LLM_STREAM_STALL: Duration = Duration::from_secs(90);

/// 网络瞬断重试次数（cli 2026-10-04 拍板 A 项）。
///
/// 2026-10-04 实测：一次约 1 分钟的瞬时网络抖动让「沉淀阶段」与「组织上下文」
/// 各被判死一次（15.04s / 15.55s，均命中 `connect_timeout`），而主工作阶段
/// 侥幸成功——彼时这一轮只调用**一次** `chat_stream`，失败即 `break`，一次
/// 抖动就报死整个 stage。现在给可重试的失败留 1~2 次退避机会。
const STREAM_MAX_RETRIES: u32 = 2;

/// 第 `n` 次重试前的等待（0s / 1s / 3s，指数式）。纯函数，便于单测。
fn retry_backoff(attempt: u32) -> Duration {
    match attempt {
        0 => Duration::ZERO,
        1 => Duration::from_secs(1),
        _ => Duration::from_secs(3),
    }
}

/// 该失败是否值得重试。
///
/// 只重试**瞬时性**失败：传输层（`connect`/`read` timeout、connection reset、
/// dns）与上游 5xx（服务商临时故障）。4xx 一律不重试——认证失败、参数非法、
/// 上下文超限、限流配额都不是「再试一次」能变的（限流需要退避更长或人干预，
/// 不在这里猜）。判定优先看 `reqwest::Error` 的类型化探针（client 落的
/// `llm_is_timeout` / `llm_is_connect`），再看 `llm_category`，不靠文案猜测。
fn is_transient(e: &LingmiaoError) -> bool {
    let ctx = e.context();
    let flag = |k: &str| ctx.get(k).and_then(serde_json::Value::as_bool) == Some(true);
    if flag("llm_is_timeout") || flag("llm_is_connect") {
        return true;
    }
    matches!(
        ctx.get("llm_category").and_then(serde_json::Value::as_str),
        Some("network") | Some("upstream")
    )
}

/// Local ISO-8601 timestamp with milliseconds (event `ts` parity).
fn now_iso() -> String {
    chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, false)
}

/// Result of a [`StageAgent::work`] run.
#[derive(Debug, Clone)]
pub struct StageOutcome {
    /// The final assistant text.
    pub content: String,
    /// The JSON extracted (or salvaged) from the text; `Value::Null` when none.
    pub json: Value,
    /// Number of tool calls executed across the loop.
    pub tool_calls: u64,
    /// Summed token usage across every LLM round-trip.
    pub usage: Usage,
    /// Provider-measured `input_tokens` of the **last** LLM round-trip of the
    /// stage (not the sum).
    ///
    /// cli 2026-09-28: 右下角上下文行要显示「本轮次的上下文大小」，而 `usage`
    /// 是整段工作阶段工具循环里**每一次往返累加**的结果——图片 base64、全量
    /// msgs 重发会让它涨到 31.5M 这种荒谬量级。这里单独保留最后一次往返的
    /// `input_tokens`（真实的「当前上下文注入量」），供 UI 显示。
    pub last_input_tokens: u64,
    /// Wall-clock duration in milliseconds.
    pub elapsed_ms: f64,
    /// Non-empty when the stage faulted (timeout / no-progress / transport).
    pub fault: String,
    /// Total characters returned by each tool across the loop, keyed by tool
    /// name (`search_knowledge` → KG hit chars, `search_observations` → the
    /// observation hits, …). §8.5 uses this to attribute the context bar by
    /// *real* retrieval source instead of a magic coefficient.
    pub tool_result_chars: BTreeMap<String, u64>,
    /// Accumulated model reasoning (`reasoning_content`) across every LLM
    /// round-trip of the stage — the ③ E存档 `reasoning` column.
    pub reasoning: String,
    /// One record per executed tool call: `{"name", "arguments", "result"}`.
    /// The ③ E存档 `tool_calls` column (serialised to a JSON array), giving the
    /// archive the full tool transcript rather than a bare count.
    pub tool_call_records: Vec<Value>,
}

impl StageOutcome {
    /// Whether the stage completed without a fault.
    pub fn ok(&self) -> bool {
        self.fault.is_empty()
    }
}

/// A single stage runner: LLM client + event bus + tool registry + time budget.
#[derive(Clone)]
pub struct StageAgent {
    llm: Client,
    bus: Arc<EventBus>,
    registry: Arc<ToolRegistry>,
    timeout: Duration,
    /// Composed `base + stage` core lock (原版 `_build_lock`), re-injected
    /// after every tool turn so the model keeps the stage's core duty in view.
    /// Empty disables injection (`flash`/model-less stages, tests).
    core_lock: String,
    /// F 项: the **model** wait judge for this stage (② stage budget / ⑨
    /// round-trip). `None` (bare `StageAgent::new`, unit tests) keeps the old
    /// timeout semantics exactly; the engine arms one per stage.
    judge: Option<Arc<dyn WaitJudge>>,
}

impl StageAgent {
    /// Build an agent with an explicit time budget.
    pub fn new(
        llm: Client,
        bus: Arc<EventBus>,
        registry: Arc<ToolRegistry>,
        timeout: Duration,
    ) -> Self {
        Self {
            llm,
            bus,
            registry,
            timeout,
            core_lock: String::new(),
            judge: None,
        }
    }

    /// Set the core lock this stage re-injects after each tool turn.
    pub fn with_core_lock(mut self, lock: impl Into<String>) -> Self {
        self.core_lock = lock.into();
        self
    }

    /// Arm the **model** wait judge for this stage's own waits (F 项).
    ///
    /// ① 「等模型吐字」永远走代码判定（见 [`LLM_STREAM_STALL`]）；这里的裁判只
    /// 用于 ② 阶段总预算 / ⑨ 阶段内往返这类**不是等模型自己**的等待。
    pub fn with_wait_judge(mut self, judge: Option<Arc<dyn WaitJudge>>) -> Self {
        self.judge = judge;
        self
    }

    /// The configured time budget.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Run one stage to completion.
    ///
    /// Emits [`Event::StageStarted`] on entry and [`Event::StageResultReported`]
    /// on exit (both success and fault paths). `tools` is the canonical
    /// function-calling array to advertise (already narrowed to the stage
    /// whitelist); an empty array disables the `tools` field entirely.
    pub async fn work(
        &self,
        stage: &str,
        system: String,
        messages: Vec<Message>,
        tools: Value,
    ) -> Result<StageOutcome, LingmiaoError> {
        self.bus.push(Event::StageStarted {
            stage: stage.to_string(),
            ts: now_iso(),
        });
        let started = Instant::now();
        // `Duration::ZERO` disables the overall deadline (工作阶段 with no
        // timeout); the no-progress guard below still stops a stuck loop.
        let has_timeout = !self.timeout.is_zero();
        let deadline = tokio::time::Instant::now() + self.timeout;
        let tool_defs = tools_to_defs(&tools);
        // ⑥ vision gate: the active model's capability is read from its
        // `ModelSpec` (never hard-coded by name), so a `supports_vision` model
        // receives `read_file` images as multipart attachments while every other
        // model keeps the textual marker + an explicit note.
        let model = self.llm.model().to_string();
        let supports_vision = self.llm.spec().supports_vision;

        let mut msgs = messages;
        // 原版对齐: the core lock rides on the first user turn too, so
        // even a tool-less stage carries the reminder (`user_content = prompt +
        // lock`). The model's real question is preserved; the lock is appended.
        reinject_lock(&mut msgs, &self.core_lock);
        let mut total = Usage::default();
        // cli 2026-09-28: 另记「最后一次往返」的 provider 实测 input_tokens——
        // 这才是「当前上下文注入量」的真值（`total` 是所有往返的累加，会因工具
        // 循环/图片 base64 重发而暴涨到 31.5M 这种无意义的量级）。
        let mut last_input_tokens: u64 = 0;
        let mut tool_calls: u64 = 0;
        let mut last_sig: Option<String> = None;
        let mut repeats: u32 = 0;
        let mut fault = String::new();
        let mut content = String::new();
        let mut tool_result_chars: BTreeMap<String, u64> = BTreeMap::new();
        // ③ E存档: the full-turn extras — the model's reasoning and one record
        // per executed tool call — accumulated so the archive row is complete.
        let mut reasoning = String::new();
        let mut tool_call_records: Vec<Value> = Vec::new();
        // F 项 ②: one progress clock for the whole stage; touched on every
        // completed round-trip and every tool result, so a stage that keeps
        // working is never mistaken for a stalled one.
        let stage_progress = Progress::new();

        loop {
            let mut req = ChatRequest::chat(system.clone(), msgs.clone());
            req.tools = tool_defs.clone();
            req.tool_choice = "auto".to_string();

            // F 项 ①: the round-trip's progress clock. Every stream delta touches
            // it, so "silent" really means **nothing arrived**, not "we are
            // waiting" — a long but streaming answer is never mistaken for a hang.
            let turn_progress = Progress::new();
            let touch = turn_progress.clone();
            // A 项（cli 2026-10-04）: 瞬时网络抖动重试。`chat_stream` 的 `Err`
            // 只发生在**建连/发出请求**阶段（流中途失败走 `StreamDelta::Error`），
            // 正是实测 15s `connect_timeout` 的失败面。只对**瞬时**失败重试
            // （[`is_transient`]），4xx 一律直接上报——认证/参数/超限/配额不是
            // 「再试一次」能变的。每次重试重放同一个 `req`（无副作用，幂等）。
            let attempt = async move {
                let mut last_err: Option<LingmiaoError> = None;
                for retry in 0..=STREAM_MAX_RETRIES {
                    if retry > 0 {
                        tokio::time::sleep(retry_backoff(retry)).await;
                    }
                    match self.llm.chat_stream(req.clone()).await {
                        Ok(mut rx) => {
                            let mut agg = StreamAggregator::default();
                            while let Some(delta) = rx.recv().await {
                                touch.touch();
                                match &delta {
                                    lingmiao_llm::StreamDelta::Content(t) => {
                                        self.bus.push(Event::LlmDelta {
                                            stage: stage.to_string(),
                                            kind: "content".to_string(),
                                            text: t.clone(),
                                        })
                                    }
                                    lingmiao_llm::StreamDelta::Reasoning(t) => {
                                        self.bus.push(Event::LlmDelta {
                                            stage: stage.to_string(),
                                            kind: "reasoning".to_string(),
                                            text: t.clone(),
                                        })
                                    }
                                    _ => {}
                                }
                                agg.apply(&delta);
                            }
                            return Ok::<_, LingmiaoError>(agg.finish());
                        }
                        Err(e) if retry < STREAM_MAX_RETRIES && is_transient(&e) => {
                            tracing::warn!(
                                stage,
                                attempt = retry + 1,
                                max = STREAM_MAX_RETRIES,
                                error = %e.message(),
                                "chat_stream transient failure — retrying after backoff"
                            );
                            last_err = Some(e);
                        }
                        Err(e) => return Err(e),
                    }
                }
                Err(last_err.unwrap_or_else(|| {
                    LingmiaoError::llm("request failed: exhausted retries", 0, 1)
                }))
            };

            // 剩余阶段预算（② / ⑨ 共用的那条硬上界）。`None` = 无预算
            // （工作阶段 timeout=0），此时由各自的停摆判定兜底。
            let stage_remaining: Option<Duration> = if has_timeout {
                match deadline.checked_duration_since(tokio::time::Instant::now()) {
                    Some(rem) => Some(rem),
                    None => {
                        fault = format!(
                            "stage `{stage}` timed out after {}s",
                            self.timeout.as_secs()
                        );
                        break;
                    }
                }
            } else {
                None
            };

            // ① 等模型吐字 —— **代码**判定（cli 拍板接受的例外，理由见
            // [`LLM_STREAM_STALL`]）：被等的就是模型自己，卡住时问它「还等不等」
            // 等于同一根电话线问不出去。持续吐字节就持续 touch，长回答不会被误杀；
            // 连续 90s 一个字节都没有才中断。无裁判在范围里时，它同时就是旧的
            // 90s 读超时（`guard` 语义），行为不变。
            let streamed = match guard_capped(
                Some(CodeStall::handle(LLM_STREAM_STALL)),
                WaitClass::Model,
                format!("{stage}：模型响应"),
                LLM_STREAM_STALL,
                stage_remaining,
                turn_progress,
                attempt,
            )
            .await
            {
                Guarded::Done(Ok(resp)) => Some(resp),
                Guarded::Done(Err(e)) => {
                    fault = e.message().to_string();
                    break;
                }
                Guarded::Aborted(a) => {
                    fault = format!("stage `{stage}`: {}", a.message());
                    break;
                }
                Guarded::TimedOut => {
                    fault = format!(
                        "stage `{stage}` timed out after {}s",
                        self.timeout.as_secs()
                    );
                    break;
                }
            };
            let Some(mut resp) = streamed else { break };

            // ② 阶段总预算 / ⑨ 阶段内单次往返（F 项）。这一处的等待发生在**两次
            // 往返之间**：每次拿到回复、进入下一轮之前，检查「这个阶段安静了
            // 多久」——安静超过门槛就把状态交给**模型**决策（cli 口径）。持续有
            // 往返或工具进展时一次都不会问，长工具循环不会被误判成卡死。
            //
            // cli 2026-10-05 第二轮（「要进界面的」）: 这一处不经 `polled`，所以
            // 它自己把心跳发上总线，与其他 7 处走同一条 [`Event::WaitPolled`]
            // 通道 —— 界面因此不会因为等待发生在哪一处而表现不一致。
            if let Some(judge) = self.judge.clone() {
                let what = format!("{stage}：阶段内等待");
                let state = sample(WaitClass::Store, what.clone(), &stage_progress);
                if state.silent >= global_policy().ask_after {
                    report(WaitClass::Store, &what, &state, PHASE_ASKING, "");
                    match judge.judge(&state).await {
                        Verdict::Continue => {
                            report(WaitClass::Store, &what, &state, PHASE_CONTINUE, "");
                        }
                        Verdict::Interrupt(reason) => {
                            report(WaitClass::Store, &what, &state, PHASE_INTERRUPT, &reason);
                            fault = format!("stage `{stage}`: {reason}");
                            break;
                        }
                    }
                } else if state.silent >= POLL_INTERVAL {
                    // Quiet for a whole sampling period → report it (this is the
                    // 「卡住了还是在干活」 case the user cannot otherwise tell).
                    report(WaitClass::Store, &what, &state, PHASE_SAMPLING, "");
                }
            }

            stage_progress.touch();
            add_usage(&mut total, &resp.usage);
            last_input_tokens = resp.usage.input_tokens;
            // cli 2026-09-28（「上下文显示还是不对，你显示当前 llm 的输入 tokens 就行」）:
            // **每一次 LLM 往返**都上报它自己的 provider 实测 usage。原版
            // `stage_agent.py:453` 就是 `self.ql.events.push(resp)` —— 一个 `llm_response`
            // 事件；Rust 版此前只在**阶段结束**push 一次 `StageResultReported`，UI 因此
            // 拿不到「当前这次调用」的输入量：回合进行中 组织上下文 刚发出的调用、工作阶段 正在
            // 流式的那次往返，右下角都读不到，一直显示**上一轮**的数字。
            self.bus.push(round_trip_event(stage, &resp));
            if !resp.reasoning_content.is_empty() {
                if !reasoning.is_empty() {
                    reasoning.push('\n');
                }
                reasoning.push_str(&resp.reasoning_content);
            }
            if !resp.content.trim().is_empty() {
                content = resp.content.clone();
            }
            // 原版对齐 (`stage_agent.py:456`): deepseek-v4-flash sometimes
            // leaks tool calls as DSML markup in `content` instead of populating
            // the structured `tool_calls` field. Normalize those into real calls
            // so the loop can execute them rather than ending on a markup blob.
            if resp.tool_calls.is_empty() {
                let dsml_calls = extract_dsml_tool_calls(&resp.content);
                if !dsml_calls.is_empty() {
                    resp.tool_calls = dsml_calls;
                }
            }
            if resp.tool_calls.is_empty() {
                break;
            }

            tool_calls += resp.tool_calls.len() as u64;
            msgs.push(Message::assistant_tool_calls(resp.tool_calls.clone()));
            let mut sigs: Vec<String> = Vec::with_capacity(resp.tool_calls.len());
            // ⑥ vision: images read by this tool batch (path, data_url), attached
            // as one multimodal user turn after the batch completes.
            let mut batch_images: Vec<(String, String)> = Vec::new();
            // A 项（cli 2026-10-02）: **只读并行 / 写串行**。一次 assistant 回合里
            // 模型可能带回多个 `tool_calls`；此前严格逐个 `await`，于是两个
            // `search_memory` 的耗时是**相加**的（组织上下文实测 103.9 s + 51.8 s
            // = 155.7 s，2026-10-02）。现在同批里**连续的只读调用**并发执行，
            // 写调用（`is_mutating`）单独串行——它可能依赖/改变外部状态，必须保持
            // 模型给出的先后。结果仍按原始调用序返回，所以下面的事件、tool 消息、
            // 存档记录与串行版逐字节一致（唯一变化是耗时）。
            let execs =
                execute_tool_batch(&self.registry, &self.bus, stage, &resp.tool_calls).await;
            for (call, exec) in resp.tool_calls.iter().zip(execs) {
                sigs.push(format!("{}:{}", call.name, call.arguments));
                let (out_content, is_error, out_diff, ms) = exec;
                stage_progress.touch();
                // §8.5: record the real character volume each tool returned, so
                // the context bar can attribute retrieval by source (KG vs
                // observations) rather than guessing from coefficients.
                *tool_result_chars.entry(call.name.clone()).or_default() +=
                    out_content.chars().count() as u64;
                // Display-only preview for the TUI: an image marker's base64 is
                // replaced by a short note (cli 2026-09-28) — the raw `data_url`
                // otherwise floods the transcript tool card with megabytes of
                // base64 the user cannot read.
                let preview = truncate(
                    &lingmiao_core::image::strip_data_url(&out_content),
                    RESULT_PREVIEW_CHARS,
                );
                self.bus.push(Event::ToolCalled {
                    stage: stage.to_string(),
                    tool: call.name.clone(),
                    args: call.arguments.clone(),
                    result_preview: preview,
                    ms,
                    error: is_error,
                    call_id: call.id.clone(),
                    // cli 2026-09-28「代码改动的红绿对比」: the display-only diff of
                    // a file mutation (empty for every other tool) — the TUI paints
                    // `add` green / `remove` red exactly like CC's tool cards.
                    diff: Value::Array(
                        out_diff
                            .iter()
                            .map(|l| json!({"kind": l.kind.as_str(), "text": l.text}))
                            .collect(),
                    ),
                });
                let (tool_text, image) = resolve_tool_media(&out_content, &model, supports_vision);
                // Engine-level context backstop: the feed re-sends this text on
                // every later round-trip, so an unbounded tool result balloons
                // the request (and was the 沉淀阶段 400's root cause). Cap it
                // after the image marker is resolved, so the pixels still ride
                // the multimodal attachment while the text copy stays bounded.
                let feed_text = cap_tool_feed(tool_text);
                msgs.push(Message::tool(call.id.clone(), feed_text));
                if let Some(media) = image {
                    batch_images.push(media);
                }
                // ③ E存档: record this call's name / arguments / full result so
                // the completed turn's archive carries the whole tool transcript
                // (not just a count).
                tool_call_records.push(json!({
                    "name": call.name,
                    "arguments": call.arguments,
                    "result": out_content,
                }));
            }

            // ⑥ vision: the images this batch produced ride the very next user
            // message (OpenAI keeps `image_url` parts in user content, so tool
            // turns stay text-only). Non-vision models got a note instead and
            // contribute nothing here.
            if !batch_images.is_empty() {
                let hint = image_hint(&batch_images);
                msgs.push(Message::user_with_images(
                    hint,
                    batch_images.drain(..).map(|(_, url)| url).collect(),
                ));
            }

            // 原版 `_reinject_lock`: after every tool turn, append the core
            // lock to the last tool message so the stage's core duty survives
            // the growing tool transcript.
            reinject_lock(&mut msgs, &self.core_lock);

            // No-progress guard: identical consecutive tool-call sets mean the
            // model is stuck; abort rather than spin until the timeout.
            let sig = sigs.join("|");
            if Some(&sig) == last_sig.as_ref() {
                repeats += 1;
            } else {
                repeats = 0;
            }
            last_sig = Some(sig);
            if repeats >= 2 {
                fault = format!("stage `{stage}`: no progress (repeated identical tool calls)");
                break;
            }
        }

        let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
        let json = extract_json(&content);
        // cli 2026-09-28: `tokens` 里除累加值外，另带 `input_tokens_last`
        // （最后一次往返的实测输入），供 UI 显示「当前轮次的上下文大小」。
        let mut tokens_json = serde_json::to_value(&total).unwrap_or(Value::Null);
        if let Value::Object(m) = &mut tokens_json {
            m.insert("input_tokens_last".into(), Value::from(last_input_tokens));
        }
        self.bus.push(Event::StageResultReported {
            stage: stage.to_string(),
            ok: fault.is_empty(),
            data: json.clone(),
            fault_type: if fault.is_empty() {
                String::new()
            } else {
                "stage".to_string()
            },
            fault_detail: fault.clone(),
            tokens: tokens_json,
            tool_calls,
            elapsed_ms,
        });

        let outcome = StageOutcome {
            content,
            json,
            tool_calls,
            usage: total,
            last_input_tokens,
            elapsed_ms,
            fault: fault.clone(),
            tool_result_chars,
            reasoning,
            tool_call_records,
        };
        if fault.is_empty() {
            Ok(outcome)
        } else {
            Err(LingmiaoError::stage(stage, fault))
        }
    }
}

fn add_usage(acc: &mut Usage, add: &Usage) {
    acc.input_tokens += add.input_tokens;
    acc.output_tokens += add.output_tokens;
    acc.total_tokens += add.total_tokens;
}

/// One tool call's outcome: `(content, is_error, display_diff, elapsed_ms)`.
type ToolExec = (String, bool, Vec<lingmiao_tools::diff::DiffLine>, f64);

/// Execute one assistant turn's `tool_calls` — **只读并行 / 写串行**（A 项，
/// cli 2026-10-02）。
///
/// The model may return several calls in one turn. Running them strictly one
/// after another made the batch's latency the **sum** of its calls: two
/// `search_memory` calls cost 103.9 s + 51.8 s = 155.7 s of a single
/// 组织上下文 stage (measured 2026-10-02). Now each **maximal run of
/// consecutive read-only calls** runs concurrently, while a mutating call
/// ([`ToolRegistry::is_mutating`]) keeps to its own serial slot — it may observe
/// or change external state the model meant to sequence.
///
/// Results are returned in the **original call order** regardless of which
/// finished first, so every downstream consumer (the `ToolCalled` events, the
/// `Message::tool` feed, the archive transcript) sees exactly what the serial
/// version produced — the only difference is wall-clock time.
async fn execute_tool_batch(
    registry: &Arc<ToolRegistry>,
    bus: &Arc<EventBus>,
    stage: &str,
    calls: &[ToolCall],
) -> Vec<ToolExec> {
    let mut out: Vec<Option<ToolExec>> = (0..calls.len()).map(|_| None).collect();
    let mut i = 0;
    while i < calls.len() {
        // A mutating call forms a run of its own; a read-only one extends the
        // run until the next mutating call.
        let mut end = i + 1;
        if !registry.is_mutating(&calls[i].name) {
            while end < calls.len() && !registry.is_mutating(&calls[end].name) {
                end += 1;
            }
        }
        for call in &calls[i..end] {
            bus.push(Event::ToolStarted {
                stage: stage.to_string(),
                tool: call.name.clone(),
                args: call.arguments.clone(),
                call_id: call.id.clone(),
            });
        }
        if end - i == 1 {
            out[i] = Some(execute_one(registry, &calls[i]).await);
        } else {
            let results = futures_util::future::join_all(
                calls[i..end].iter().map(|c| execute_one(registry, c)),
            )
            .await;
            for (k, r) in results.into_iter().enumerate() {
                out[i + k] = Some(r);
            }
        }
        i = end;
    }
    out.into_iter()
        .map(|o| o.unwrap_or_else(|| (String::new(), true, Vec::new(), 0.0)))
        .collect()
}

/// Run a single tool call, timing it and never propagating an error (a failed
/// tool still returns its message so the model can recover).
async fn execute_one(registry: &Arc<ToolRegistry>, call: &ToolCall) -> ToolExec {
    let t0 = Instant::now();
    let (content, is_error, diff) = match registry.execute(&call.name, call.arguments.clone()).await
    {
        Ok(o) => (o.content, o.is_error, o.diff),
        Err(e) => (e.to_string(), true, Vec::new()),
    };
    (content, is_error, diff, t0.elapsed().as_secs_f64() * 1000.0)
}

/// The `llm_response` event for **one** LLM round-trip (原版对齐 —
/// `stage_agent.py:453` `self.ql.events.push(resp)`).
///
/// cli 2026-09-28（「上下文显示还是不对，你显示当前 llm 的输入 tokens 就行」）: the
/// inner loop told the UI nothing until the *stage* finished, so the bottom-right
/// context figure showed the previous turn's number while B was still calling
/// and during C's whole stream. Publishing every round-trip's provider-measured
/// `usage.input_tokens` lets the UI track **the LLM call happening now** (this
/// variant existed since M0 but was never pushed — it was dead).
///
/// Only the accounting rides the event: the text / tool-call payloads are left
/// empty on purpose. They already stream as [`Event::LlmDelta`] (per chunk) and
/// are archived in full by `record_turn`, so repeating them here would only
/// duplicate megabytes into the event log once per round-trip of a long
/// C-stage tool loop.
fn round_trip_event(stage: &str, resp: &LlmResponse) -> Event {
    Event::LlmResponse {
        stage: stage.to_string(),
        content: String::new(),
        tool_calls: Vec::new(),
        tool_count: resp.tool_calls.len() as u64,
        tools: Vec::new(),
        reasoning_content: String::new(),
        finish_reason: resp.finish_reason.clone(),
        usage: resp.usage.clone(),
    }
}

/// DSML (DeepSeek Markup Language) tool-call delimiter.
///
/// Some models — notably `deepseek-v4-flash` — leak tool calls into `content`
/// as this markup instead of populating the structured `tool_calls` field. The
/// tags use fullwidth vertical bars (U+FF5C), 1:1 with the Python original's
/// `_DSML` (`core/stage_agent.py:77`).
const DSML: &str = "\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C}";

/// Parse DSML tool-call markup leaked into `content` into structured calls.
///
/// Ported 1:1 from the Python original's `_extract_dsml_tool_calls`
/// (`core/stage_agent.py:86`). Each `<…DSML…invoke name="X">…</…DSML…invoke>`
/// block becomes one [`ToolCall`] with `id = "dsml_<i>"`; the inner
/// `<…DSML…parameter name="k">v</…DSML…parameter>` tags become its `arguments`.
/// Returns `[]` when no `DSML` markup is present. Parameter values stay strings
/// unless they parse as JSON (so arrays/objects survive the round-trip).
fn extract_dsml_tool_calls(content: &str) -> Vec<ToolCall> {
    if content.is_empty() || !content.contains("DSML") {
        return Vec::new();
    }
    let invoke_open = format!("<{DSML}invoke");
    let invoke_close = format!("</{DSML}invoke>");
    let param_open = format!("<{DSML}parameter");
    let param_close = format!("</{DSML}parameter>");

    let mut calls: Vec<ToolCall> = Vec::new();
    let mut search = 0usize;
    let mut idx = 0usize;
    // `<…DSML…invoke\s+name="…"[^>]*>(.*?)</…DSML…invoke>` (DOTALL, lazy body)
    while let Some(rel) = content[search..].find(&invoke_open) {
        let attr_start = search + rel + invoke_open.len();
        let Some((name, attr_end)) = dsml_attr_name(content, attr_start) else {
            search = attr_start;
            continue;
        };
        let Some(gt_rel) = content[attr_end..].find('>') else {
            break;
        };
        let body_start = attr_end + gt_rel + 1;
        let Some(close_rel) = content[body_start..].find(&invoke_close) else {
            break;
        };
        let body = &content[body_start..body_start + close_rel];

        // `<…DSML…parameter\s+name="…"[^>]*>(.*?)</…DSML…parameter>`
        let mut args = Map::new();
        let mut psearch = 0usize;
        while let Some(prel) = body[psearch..].find(&param_open) {
            let pattr_start = psearch + prel + param_open.len();
            let Some((key, pattr_end)) = dsml_attr_name(body, pattr_start) else {
                psearch = pattr_start;
                continue;
            };
            let Some(pgt_rel) = body[pattr_end..].find('>') else {
                break;
            };
            let pbody_start = pattr_end + pgt_rel + 1;
            let Some(pclose_rel) = body[pbody_start..].find(&param_close) else {
                break;
            };
            let val = body[pbody_start..pbody_start + pclose_rel].trim();
            args.insert(key, dsml_param_value(val));
            psearch = pbody_start + pclose_rel + param_close.len();
        }

        calls.push(ToolCall {
            id: format!("dsml_{idx}"),
            name,
            arguments: Value::Object(args),
        });
        idx += 1;
        search = body_start + close_rel + invoke_close.len();
    }
    calls
}

/// Read a `\s+name="…"` attribute starting at byte `start`; returns the (trimmed,
/// non-empty) name and the absolute index just past the closing quote.
///
/// Mirrors the `\s+name="([^"]+)"` prefix of the Python invoke/parameter regexes.
fn dsml_attr_name(s: &str, start: usize) -> Option<(String, usize)> {
    let rest = &s[start..];
    // `\s+`: require at least one leading whitespace char before `name=`.
    let ws = rest.len() - rest.trim_start().len();
    if ws == 0 {
        return None;
    }
    const PREFIX: &str = "name=\"";
    let val = rest[ws..].strip_prefix(PREFIX)?;
    let end = val.find('"')?;
    let name = val[..end].trim();
    if name.is_empty() {
        return None;
    }
    Some((name.to_string(), start + ws + PREFIX.len() + end + 1))
}

/// DSML parameter value: JSON when it parses, else the raw trimmed string
/// (Python: `json.loads(val)` with a string fallback).
fn dsml_param_value(val: &str) -> Value {
    serde_json::from_str::<Value>(val).unwrap_or_else(|_| json!(val))
}

/// Append the core lock to the last message's content (原版 `_reinject_lock`).
///
/// Used twice per stage: once on the initial user turn, then after every tool
/// result batch. A no-op when the lock is empty, so lock-less stages/tests are
/// byte-for-byte unchanged.
fn reinject_lock(msgs: &mut [Message], lock: &str) {
    if lock.is_empty() {
        return;
    }
    if let Some(last) = msgs.last_mut() {
        let content = last.content.get_or_insert_with(String::new);
        if !content.is_empty() {
            content.push('\n');
        }
        content.push_str(lock);
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect::<String>() + "…"
}

/// Resolve one tool result into how it feeds back to the model (⑥ vision gate).
///
/// Returns the text for the tool-result message and, only when the result is an
/// image the active model can actually *see*, the `(path, data_url)` to attach
/// as a follow-up multimodal user turn. A non-vision model keeps the textual
/// image marker (the image is never silently dropped) and gets an explicit note
/// naming the model, so the transcript records why no image arrived.
///
/// cli 2026-09-28「图片 base64 双份注入」: the tool-result **text** is the raw
/// marker JSON, whose `data_url` is the full base64 — a second copy of the very
/// pixels the attachment carries. Since the C-stage tool loop re-sends the whole
/// message list every round-trip, that base64 was re-counted into
/// `prompt_tokens` over and over (the 31.5M figure). Both branches therefore
/// feed the model a **compact** marker (base64 replaced by a short note).
fn resolve_tool_media(
    content: &str,
    model: &str,
    supports_vision: bool,
) -> (String, Option<(String, String)>) {
    match lingmiao_core::image::marker_parts(content) {
        Some((path, url)) if supports_vision => (
            lingmiao_core::image::strip_data_url(content),
            Some((path, url)),
        ),
        Some(_) => (
            format!(
                "{}\n\n[视觉不可用] 当前模型 `{model}` 不支持图片；本图未作为多模态附件发送。切换到支持视觉的模型（supports_vision）后可查看。",
                lingmiao_core::image::strip_data_url(content)
            ),
            None,
        ),
        None => (content.to_string(), None),
    }
}

/// Build the text hint that accompanies a batch's image attachments.
fn image_hint(images: &[(String, String)]) -> String {
    let mut s = String::from("以下是本轮工具读取的图片：");
    for (path, _) in images {
        s.push_str("\n- ");
        s.push_str(path);
    }
    s
}

/// Bound one tool result before it enters the model's message list.
///
/// Unlike [`RESULT_PREVIEW_CHARS`] (display-only), this text is *sent to the
/// model* and re-sent on every later round-trip of the stage — an unbounded
/// result is a context bomb. When cut, an explicit note tells the model the
/// result was truncated *and* how to get the rest, so it does not silently
/// reason over a half-read file as if it were complete (the `read_file` /
/// `grep` tools do the same, advising pagination).
fn cap_tool_feed(text: String) -> String {
    if text.chars().count() <= TOOL_FEED_MAX_CHARS {
        return text;
    }
    let head: String = text.chars().take(TOOL_FEED_MAX_CHARS).collect();
    format!(
        "{head}\n…[工具结果超长，已截断至 {TOOL_FEED_MAX_CHARS} 字符]\
         \n提示：结果仅保留了前部。请用更精确的参数（head_limit / offset / 更窄的关键词）\
         重新调用，或改用分页读取，不要依据这份截断结果下结论。"
    )
}

/// Convert the canonical `tools` array into [`ToolDef`]s (empty → `None`).
fn tools_to_defs(tools: &Value) -> Option<Vec<ToolDef>> {
    let arr = tools.as_array()?;
    if arr.is_empty() {
        return None;
    }
    let defs: Vec<ToolDef> = arr
        .iter()
        .filter_map(|t| {
            let f = &t["function"];
            let name = f["name"].as_str()?.to_string();
            Some(ToolDef {
                name,
                description: f["description"].as_str().unwrap_or("").to_string(),
                parameters: f["parameters"].clone(),
            })
        })
        .collect();
    if defs.is_empty() { None } else { Some(defs) }
}

/// Extract JSON from a model reply: body JSON first, then a fenced block, then
/// a field-level salvage. Returns [`Value::Null`] when nothing is recoverable.
pub fn extract_json(text: &str) -> Value {
    // 1. Body JSON (outermost object / array).
    if let Some(v) = parse_embedded(text) {
        return v;
    }
    // 2. ```json … ``` fenced block.
    if let Some(block) = fenced_block(text)
        && let Some(v) = parse_embedded(&block)
    {
        return v;
    }
    // 3. Salvage `"key": value` pairs rather than dropping the reply.
    salvage_fields(text)
}

/// Parse the outermost `{…}` (or `[…]`) substring as JSON.
fn parse_embedded(text: &str) -> Option<Value> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if end <= start {
        return None;
    }
    serde_json::from_str(&text[start..=end]).ok()
}

/// Return the contents of the first ```-fenced block (language tag stripped).
fn fenced_block(text: &str) -> Option<String> {
    let open = text.find("```")?;
    let after = &text[open + 3..];
    // Skip an optional language tag on the opening line.
    let body_start = after.find('\n').map(|i| i + 1).unwrap_or(0);
    let body = &after[body_start..];
    let close = body.find("```")?;
    Some(body[..close].to_string())
}

/// Scrape simple `"key": <string|number|bool|null>` pairs from broken JSON.
fn salvage_fields(text: &str) -> Value {
    let mut map = Map::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while let Some(rel) = text[i..].find('"') {
        let key_start = i + rel + 1;
        let Some(kend_rel) = text[key_start..].find('"') else {
            break;
        };
        let key = &text[key_start..key_start + kend_rel];
        let mut j = key_start + kend_rel + 1;
        // skip whitespace + colon
        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        if j < bytes.len() && bytes[j] == b':' {
            j += 1;
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if j < bytes.len() {
                let (value, next) = scrape_value(text, j);
                if let Some(v) = value {
                    map.insert(key.to_string(), v);
                }
                i = next.max(j + 1);
                continue;
            }
        }
        i = key_start + kend_rel + 1;
    }
    if map.is_empty() {
        Value::Null
    } else {
        Value::Object(map)
    }
}

/// Scrape one value starting at `start`; returns the value and the next index.
fn scrape_value(text: &str, start: usize) -> (Option<Value>, usize) {
    let rest = &text[start..];
    let Some(c) = rest.chars().next() else {
        return (None, start);
    };
    if c == '"' {
        if let Some(end_rel) = rest[1..].find('"') {
            let val = &rest[1..1 + end_rel];
            return (Some(json!(val)), start + 1 + end_rel + 1);
        }
        return (None, start + 1);
    }
    // Bare token: number / true / false / null / until delimiter.
    let end_rel = rest.find([',', '}', '\n']).unwrap_or(rest.len());
    let token = rest[..end_rel].trim();
    let value = match token {
        "true" => Some(json!(true)),
        "false" => Some(json!(false)),
        "null" => Some(Value::Null),
        _ => token.parse::<f64>().ok().map(|n| json!(n)),
    };
    (value, start + end_rel)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tool that sleeps a fixed time before echoing its own name; `mutating`
    /// selects whether the batch may parallelise it.
    struct SlowTool {
        name: &'static str,
        mutating: bool,
    }

    #[async_trait::async_trait]
    impl lingmiao_tools::Tool for SlowTool {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            "sleeps, then echoes its name"
        }
        fn parameters(&self) -> Value {
            json!({"type": "object"})
        }
        fn is_mutating(&self) -> bool {
            self.mutating
        }
        async fn execute(
            &self,
            _a: Value,
        ) -> Result<lingmiao_tools::ToolOutput, lingmiao_tools::ToolError> {
            tokio::time::sleep(Duration::from_millis(200)).await;
            Ok(lingmiao_tools::ToolOutput::ok(self.name))
        }
    }

    fn batch_calls(names: &[&str]) -> Vec<ToolCall> {
        names
            .iter()
            .enumerate()
            .map(|(i, n)| ToolCall {
                id: format!("call_{i}"),
                name: (*n).to_string(),
                arguments: json!({}),
            })
            .collect()
    }

    #[tokio::test]
    async fn read_only_calls_run_concurrently_but_keep_their_order() {
        // A 项 (cli 2026-10-02): two consecutive read-only calls must overlap —
        // the batch costs ~max(t), not ~sum(t) — while the returned vector stays
        // in the model's original call order.
        let reg = Arc::new(ToolRegistry::new());
        reg.register(SlowTool {
            name: "read_a",
            mutating: false,
        });
        reg.register(SlowTool {
            name: "read_b",
            mutating: false,
        });
        let bus = Arc::new(EventBus::new(64));
        let calls = batch_calls(&["read_a", "read_b"]);

        let t0 = Instant::now();
        let out = execute_tool_batch(&reg, &bus, "工作阶段", &calls).await;
        let elapsed = t0.elapsed();

        assert_eq!(out.len(), 2);
        assert_eq!(out[0].0, "read_a", "original order preserved");
        assert_eq!(out[1].0, "read_b", "original order preserved");
        assert!(
            elapsed < Duration::from_millis(380),
            "two 200ms reads must overlap, took {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn a_mutating_call_is_never_parallelised_with_a_read() {
        // A 项: a read-only *run* overlaps, but a mutating call gets its own
        // serial slot — read+write+read therefore costs the sum, not the max.
        let reg = Arc::new(ToolRegistry::new());
        reg.register(SlowTool {
            name: "read_a",
            mutating: false,
        });
        reg.register(SlowTool {
            name: "write_b",
            mutating: true,
        });
        reg.register(SlowTool {
            name: "read_c",
            mutating: false,
        });
        let bus = Arc::new(EventBus::new(64));
        let calls = batch_calls(&["read_a", "write_b", "read_c"]);

        let t0 = Instant::now();
        let out = execute_tool_batch(&reg, &bus, "工作阶段", &calls).await;
        let elapsed = t0.elapsed();

        assert_eq!(
            out.iter().map(|o| o.0.as_str()).collect::<Vec<_>>(),
            ["read_a", "write_b", "read_c"]
        );
        assert!(
            elapsed >= Duration::from_millis(560),
            "the write splits the batch into three serial slots, took {elapsed:?}"
        );
    }

    #[test]
    fn an_unknown_tool_counts_as_mutating() {
        // A tool the registry cannot classify is never parallelised: the
        // conservative answer can only cost speed, never correctness.
        assert!(ToolRegistry::new().is_mutating("never_registered"));
    }

    #[test]
    fn retry_backoff_grows_then_plateaus() {
        // A 项: the first attempt is immediate, then 1s / 3s — enough spacing
        // for a ~1-minute network blip to clear without hammering the endpoint.
        assert_eq!(retry_backoff(0), Duration::ZERO);
        assert_eq!(retry_backoff(1), Duration::from_secs(1));
        assert_eq!(retry_backoff(2), Duration::from_secs(3));
        assert!(retry_backoff(3) >= Duration::from_secs(3), "plateaus");
    }

    #[test]
    fn only_transient_failures_are_retried() {
        // A 项: a transport timeout / connection failure or an upstream 5xx is
        // worth another try — the 2026-10-04 沉淀阶段/组织上下文 15s 建连超时
        // 正是这一类。A 4xx (auth / bad request / context overflow / rate
        // limit) never is: retrying cannot change the answer.
        let timeout = LingmiaoError::llm("request failed: ...", 0, 1)
            .with_context("llm_category", "network")
            .with_context("llm_is_timeout", true);
        assert!(is_transient(&timeout), "connect/read timeout retries");

        let connect = LingmiaoError::llm("request failed: ...", 0, 1)
            .with_context("llm_category", "network")
            .with_context("llm_is_connect", true);
        assert!(is_transient(&connect));

        let upstream =
            LingmiaoError::llm("HTTP 503 ...", 0, 1).with_context("llm_category", "upstream");
        assert!(is_transient(&upstream), "5xx is a transient server fault");

        let auth = LingmiaoError::llm("HTTP 401 ...", 0, 1).with_context("llm_category", "auth");
        assert!(!is_transient(&auth), "auth failure must not be retried");

        let overflow = LingmiaoError::llm("HTTP 413 ...", 0, 1)
            .with_context("llm_category", "context_overflow");
        assert!(
            !is_transient(&overflow),
            "context overflow cannot be retried"
        );

        let rate =
            LingmiaoError::llm("HTTP 429 ...", 0, 1).with_context("llm_category", "rate_limit");
        assert!(!is_transient(&rate), "rate limit is not a blind retry");
    }

    #[test]
    fn round_trip_event_carries_the_usage_of_one_call() {
        // cli 2026-09-28: every LLM round-trip must be visible to the UI, so the
        // bottom-right context figure follows the call happening *now*. The event
        // carries the provider-measured usage (and the wire name the log/bus
        // expects) while deliberately leaving the text/tool payloads empty —
        // those already stream as `LlmDelta` and are archived by `record_turn`,
        // so repeating them here would duplicate megabytes per round-trip of a
        // long C-stage tool loop.
        let resp = LlmResponse {
            content: "ignored".into(),
            reasoning_content: "ignored".into(),
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                name: "read_file".into(),
                arguments: json!({"file_path": "a.rs"}),
            }],
            finish_reason: "tool_calls".into(),
            usage: Usage {
                input_tokens: 12_800,
                output_tokens: 40,
                total_tokens: 12_840,
                tool_calls: vec![],
            },
        };
        let ev = round_trip_event("工作阶段", &resp);
        assert_eq!(ev.event_type(), "llm_response");
        match ev {
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
                assert_eq!(stage, "工作阶段");
                assert_eq!(usage.input_tokens, 12_800, "the call's own input");
                assert_eq!(usage.output_tokens, 40);
                assert_eq!(finish_reason, "tool_calls");
                assert_eq!(tool_count, 1, "the count is kept (cheap, informative)");
                // Payloads stay out of the event — no duplication in the log.
                assert!(content.is_empty());
                assert!(reasoning_content.is_empty());
                assert!(tool_calls.is_empty());
                assert!(tools.is_empty());
            }
            other => panic!("expected llm_response, got {other:?}"),
        }
    }

    #[test]
    fn reinject_lock_appends_to_last_message_only() {
        // 原版 `_reinject_lock`: the lock rides the last message (initial
        // user turn, then the newest tool result) — earlier turns untouched.
        let mut msgs = vec![Message::user("hi"), Message::tool("c1", "result")];
        reinject_lock(&mut msgs, "LOCK");
        assert_eq!(msgs[0].content.as_deref(), Some("hi"));
        assert_eq!(msgs[1].content.as_deref(), Some("result\nLOCK"));
        // Empty lock is a no-op (lock-less stages/tests stay byte-identical).
        reinject_lock(&mut msgs, "");
        assert_eq!(msgs[1].content.as_deref(), Some("result\nLOCK"));
    }

    #[test]
    fn extract_body_json_first() {
        let v = extract_json("thinking… <输出>{\"a\": 1, \"b\": \"x\"}</输出>");
        assert_eq!(v["a"], 1);
        assert_eq!(v["b"], "x");
    }

    #[test]
    fn extract_fenced_json_fallback() {
        // Body parse fails (unbalanced brace in the prose) → fenced block wins.
        let text = "note { not json\n```json\n{\"grade\": \"good\"}\n```\n";
        let v = extract_json(text);
        assert_eq!(v["grade"], "good");
    }

    #[test]
    fn salvage_broken_json_fields() {
        // Truncated / malformed JSON: body parse fails, fenced fails, salvage runs.
        let text = "{\"stage\": \"沉淀阶段\", \"summary\": \"did stuff\", \"observations\": 3";
        let v = extract_json(text);
        assert_eq!(v["stage"], "沉淀阶段");
        assert_eq!(v["summary"], "did stuff");
        assert_eq!(v["observations"], 3.0);
    }

    #[test]
    fn no_json_yields_null() {
        assert_eq!(extract_json("just prose, nothing structured"), Value::Null);
    }

    #[test]
    fn vision_gate_attaches_images_only_for_vision_models() {
        // ⑥ vision: a marker + supports_vision → attach (path, data_url); the
        // tool-result **text** is the compact marker (base64 stripped, cli
        // 2026-09-28 — the pixels must not ride along twice). A plain tool
        // result never attaches anything.
        let marker = lingmiao_core::image::build_marker(
            "shots/a.png",
            "image/png",
            4,
            "data:image/png;base64,AAECAw==",
        )
        .to_string();
        let (text, media) = resolve_tool_media(&marker, "m-vis", true);
        assert!(
            !text.contains("AAECAw=="),
            "the base64 is stripped from the text copy: {text}"
        );
        assert!(text.contains("shots/a.png"), "path survives: {text}");
        let (path, url) = media.expect("vision model attaches the image");
        assert_eq!(path, "shots/a.png");
        assert_eq!(url, "data:image/png;base64,AAECAw==");

        // Non-marker tool text → nothing to attach, text untouched.
        let (text, media) = resolve_tool_media("just a grep hit", "m-vis", true);
        assert_eq!(text, "just a grep hit");
        assert!(media.is_none());
    }

    #[test]
    fn vision_gate_notes_when_the_model_cannot_see() {
        // !supports_vision → the image is never silently dropped: the compact
        // marker stays and an explicit note names the model.
        let marker = lingmiao_core::image::build_marker(
            "shots/b.png",
            "image/png",
            4,
            "data:image/png;base64,AAECAw==",
        )
        .to_string();
        let (text, media) = resolve_tool_media(&marker, "deepseek-v4-pro", false);
        assert!(media.is_none(), "no attachment for a non-vision model");
        assert!(
            text.contains("shots/b.png"),
            "the path is preserved: {text}"
        );
        assert!(
            !text.contains("AAECAw=="),
            "the base64 is stripped from the text copy: {text}"
        );
        assert!(text.contains("视觉不可用"));
        assert!(text.contains("deepseek-v4-pro"));
    }

    #[test]
    fn image_hint_lists_every_path() {
        let hint = image_hint(&[
            ("a.png".to_string(), "u1".to_string()),
            ("b.png".to_string(), "u2".to_string()),
        ]);
        assert!(hint.contains("a.png"));
        assert!(hint.contains("b.png"));
    }

    #[test]
    fn tools_to_defs_shapes_and_empties() {
        let arr = json!([
            {"type": "function", "function": {"name": "t", "description": "d", "parameters": {"type": "object"}}}
        ]);
        let defs = tools_to_defs(&arr).expect("some");
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "t");
        assert!(tools_to_defs(&json!([])).is_none());
        assert!(tools_to_defs(&Value::Null).is_none());
    }

    #[test]
    fn dsml_extracts_invoke_with_json_and_string_params() {
        // 1:1 with `_extract_dsml_tool_calls`: fullwidth-bar tags, `id = dsml_<i>`,
        // JSON-parsed values keep their type (array here), bare strings stay strings.
        let content = "\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C} 开头\n\
            <\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C}invoke name=\"read_file\">\
            <\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C}parameter name=\"path\">src/main.rs</\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C}parameter>\
            <\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C}parameter name=\"lines\">[1, 20]</\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C}parameter>\
            </\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C}invoke>";
        let calls = extract_dsml_tool_calls(content);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "dsml_0");
        assert_eq!(calls[0].name, "read_file");
        assert_eq!(calls[0].arguments["path"], json!("src/main.rs"));
        assert_eq!(calls[0].arguments["lines"], json!([1, 20]));
    }

    #[test]
    fn dsml_numbers_multiple_invokes_in_order() {
        let open = format!("<{DSML}invoke");
        let close = format!("</{DSML}invoke>");
        let content = format!("{open} name=\"a\">{close}middle{open} name=\"b\">{close}",);
        let calls = extract_dsml_tool_calls(&content);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].id, "dsml_0");
        assert_eq!(calls[0].name, "a");
        assert_eq!(calls[1].id, "dsml_1");
        assert_eq!(calls[1].name, "b");
        assert!(calls[0].arguments.as_object().unwrap().is_empty());
    }

    #[test]
    fn dsml_absent_or_malformed_yields_nothing() {
        assert!(extract_dsml_tool_calls("").is_empty());
        assert!(extract_dsml_tool_calls("just prose, no markup").is_empty());
        // Mentions DSML but no well-formed invoke tag → still nothing.
        assert!(extract_dsml_tool_calls("the DSML marker leaked here").is_empty());
        // An invoke open tag with no name attribute is not a call.
        assert!(extract_dsml_tool_calls(&format!("<{DSML}invoke>")).is_empty());
    }

    #[test]
    fn tool_feed_cap_bounds_one_result_and_tells_the_model() {
        // cli 2026-09-28 (screenshot 055716): the *model feed* had no cap (only
        // the display preview did), so one megabyte-scale tool result — whole
        // archive rows from `list_archive` — ballooned the next request to
        // 1.6M tokens. A short result must pass through untouched.
        assert_eq!(cap_tool_feed("short result".to_string()), "short result");

        let big = "a".repeat(TOOL_FEED_MAX_CHARS * 2);
        let capped = cap_tool_feed(big);
        // Head is kept plus a bounded note, never the whole body.
        assert!(
            capped.chars().count() < TOOL_FEED_MAX_CHARS + 400,
            "cap is not enforced: {} chars",
            capped.chars().count()
        );
        assert!(capped.contains("已截断"));
        assert!(
            capped.contains("重新调用"),
            "the model is told how to refetch"
        );
    }

    #[test]
    fn result_preview_cap_exceeds_the_fold_threshold() {
        // §12.10: `result_preview` is display-only, but it must be long enough
        // that a real `read_file`/`grep` result can exceed the TUI's tool-output
        // fold (~8 wrapped lines) — a 400-char cap never reached it, which would
        // have made the fold dead in practice.
        let big = "abcdefghij".repeat(RESULT_PREVIEW_CHARS / 5 + 10);
        let cut = truncate(&big, RESULT_PREVIEW_CHARS);
        assert!(cut.ends_with('…'), "over-long previews are marked");
        assert_eq!(cut.chars().count(), RESULT_PREVIEW_CHARS + 1);
        assert!(
            cut.chars().count() >= 1600,
            "preview cap {} is too small to ever fold",
            RESULT_PREVIEW_CHARS
        );
    }
}

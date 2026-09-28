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
        }
    }

    /// Set the core lock this stage re-injects after each tool turn.
    pub fn with_core_lock(mut self, lock: impl Into<String>) -> Self {
        self.core_lock = lock.into();
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

        loop {
            let mut req = ChatRequest::chat(system.clone(), msgs.clone());
            req.tools = tool_defs.clone();
            req.tool_choice = "auto".to_string();

            let attempt = async {
                let mut rx = self.llm.chat_stream(req).await?;
                let mut agg = StreamAggregator::default();
                while let Some(delta) = rx.recv().await {
                    match &delta {
                        lingmiao_llm::StreamDelta::Content(t) => self.bus.push(Event::LlmDelta {
                            stage: stage.to_string(),
                            kind: "content".to_string(),
                            text: t.clone(),
                        }),
                        lingmiao_llm::StreamDelta::Reasoning(t) => self.bus.push(Event::LlmDelta {
                            stage: stage.to_string(),
                            kind: "reasoning".to_string(),
                            text: t.clone(),
                        }),
                        _ => {}
                    }
                    agg.apply(&delta);
                }
                Ok::<_, LingmiaoError>(agg.finish())
            };

            let mut resp = if has_timeout {
                match tokio::time::timeout_at(deadline, attempt).await {
                    Ok(Ok(resp)) => resp,
                    Ok(Err(e)) => {
                        fault = e.message().to_string();
                        break;
                    }
                    Err(_) => {
                        fault = format!(
                            "stage `{stage}` timed out after {}s",
                            self.timeout.as_secs()
                        );
                        break;
                    }
                }
            } else {
                match attempt.await {
                    Ok(resp) => resp,
                    Err(e) => {
                        fault = e.message().to_string();
                        break;
                    }
                }
            };

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
            for call in &resp.tool_calls {
                sigs.push(format!("{}:{}", call.name, call.arguments));
                self.bus.push(Event::ToolStarted {
                    stage: stage.to_string(),
                    tool: call.name.clone(),
                    args: call.arguments.clone(),
                    call_id: call.id.clone(),
                });
                let t0 = Instant::now();
                // `out_diff` is the display-only unified diff a file mutation
                // returns (CC's `structuredPatch`) — empty for every other tool.
                let (out_content, is_error, out_diff) = match self
                    .registry
                    .execute(&call.name, call.arguments.clone())
                    .await
                {
                    Ok(o) => (o.content, o.is_error, o.diff),
                    Err(e) => (e.to_string(), true, Vec::new()),
                };
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
                    ms: t0.elapsed().as_secs_f64() * 1000.0,
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

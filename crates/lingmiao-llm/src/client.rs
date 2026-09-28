//! Streaming chat client — ported from `llm/deepseek.py` (`_chat_stream`).
//!
//! Q5 decision: `reqwest` + the hand-rolled [`SseDecoder`], with the streaming
//! response surfaced as an `mpsc` channel of [`StreamDelta`]. The Python version
//! drove a synchronous SDK iterator and fired an `on_delta(kind, text)` callback;
//! here the caller drains a channel, which lets the TUI render on the same
//! `tokio` loop without a callback ping-pong.
//!
//! `trust_env` parity: the Python client deliberately ignored shell proxy vars
//! (`socks://` proxies from Clash would crash httpx at construction). reqwest
//! does not read proxy env vars unless asked, so that hazard is inherently gone.

use std::collections::BTreeMap;
use std::time::Duration;

use futures_util::StreamExt;
use lingmiao_core::LingmiaoError;
use lingmiao_core::events::Usage;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::message::{Message, ToolCall, ToolDef};
use crate::provider::{Dialect, ModelSpec};
use crate::sse::SseDecoder;

/// Channel buffer between the streaming task and the consumer.
const STREAM_BUFFER: usize = 256;

/// One request to the chat API.
#[derive(Debug, Clone)]
pub struct ChatRequest {
    /// System prompt.
    pub system: String,
    /// Conversation messages (excluding the system prompt).
    pub messages: Vec<Message>,
    /// Tool declarations; `None` disables the tools field entirely (plain chat).
    pub tools: Option<Vec<ToolDef>>,
    /// `auto` | `none` | `required` — only sent when `tools` is set.
    pub tool_choice: String,
    /// Request reasoning output (`thinking` body field, DeepSeek).
    pub thinking: bool,
    /// Sampling temperature.
    pub temperature: f32,
    /// Output-token cap. `None` omits the field (provider default). The Python
    /// original passes `max_tokens` on a few *internal* (non-turn) calls such as
    /// the memory re-ranker; it flows through both wire builders here.
    pub max_tokens: Option<u32>,
}

impl Default for ChatRequest {
    fn default() -> Self {
        Self {
            system: String::new(),
            messages: Vec::new(),
            tools: None,
            tool_choice: "auto".to_string(),
            thinking: false,
            temperature: 1.0,
            max_tokens: None,
        }
    }
}

impl ChatRequest {
    /// A minimal plain-chat request (no tools) for the given system + messages.
    pub fn chat(system: impl Into<String>, messages: Vec<Message>) -> Self {
        Self {
            system: system.into(),
            messages,
            ..Default::default()
        }
    }
}

/// One incremental item off the streaming channel.
#[derive(Debug, Clone)]
pub enum StreamDelta {
    /// A content token chunk.
    Content(String),
    /// A reasoning (`reasoning_content`) token chunk.
    Reasoning(String),
    /// A fragment of a tool call (arguments arrive split across chunks).
    ToolCallArgs {
        /// Slot index (parallel tool calls).
        index: usize,
        /// Call id, when the provider sends it.
        id: Option<String>,
        /// Function name, when the provider sends it.
        name: Option<String>,
        /// Raw argument-string fragment to append.
        args: String,
    },
    /// Final token accounting.
    Usage(Usage),
    /// The provider's `finish_reason`.
    Finish(String),
    /// A transport / protocol failure mid-stream.
    Error(String),
}

/// Accumulates a [`StreamDelta`] sequence back into a full [`LlmResponse`].
#[derive(Debug, Default)]
pub struct StreamAggregator {
    content: String,
    reasoning: String,
    calls: BTreeMap<usize, (String, String, String)>,
    finish_reason: String,
    usage: Usage,
}

impl StreamAggregator {
    /// Fold one delta into the accumulator.
    pub fn apply(&mut self, delta: &StreamDelta) {
        match delta {
            StreamDelta::Content(t) => self.content.push_str(t),
            StreamDelta::Reasoning(t) => self.reasoning.push_str(t),
            StreamDelta::ToolCallArgs {
                index,
                id,
                name,
                args,
            } => {
                let slot = self.calls.entry(*index).or_default();
                if let Some(id) = id {
                    slot.0 = id.clone();
                }
                if let Some(name) = name {
                    slot.1 = name.clone();
                }
                slot.2.push_str(args);
            }
            StreamDelta::Usage(u) => self.usage = u.clone(),
            StreamDelta::Finish(reason) => self.finish_reason = reason.clone(),
            StreamDelta::Error(_) => {}
        }
    }

    /// Consume the accumulator, producing the final response.
    pub fn finish(self) -> LlmResponse {
        let mut tool_calls: Vec<ToolCall> = self
            .calls
            .into_values()
            .map(|(id, name, args)| ToolCall::from_raw(id, name, &args))
            .collect();
        let mut usage = self.usage;
        usage.tool_calls = tool_calls.iter().map(|c| json!(c.name)).collect();
        LlmResponse {
            content: self.content,
            reasoning_content: self.reasoning,
            tool_calls: std::mem::take(&mut tool_calls),
            finish_reason: self.finish_reason,
            usage,
        }
    }
}

/// A completed (non-streamed) chat response.
#[derive(Debug, Clone, Default)]
pub struct LlmResponse {
    /// Assistant text.
    pub content: String,
    /// Reasoning text (`reasoning_content`).
    pub reasoning_content: String,
    /// Tool calls requested by the model.
    pub tool_calls: Vec<ToolCall>,
    /// Provider finish reason.
    pub finish_reason: String,
    /// Token usage.
    pub usage: Usage,
}

/// HTTP chat client bound to one [`ModelSpec`].
#[derive(Debug, Clone)]
pub struct Client {
    http: reqwest::Client,
    cfg: ModelSpec,
}

impl Client {
    /// Build a client for `cfg`.
    pub fn new(cfg: ModelSpec) -> Result<Self, LingmiaoError> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .read_timeout(Duration::from_secs(90))
            .build()
            .map_err(|e| LingmiaoError::llm(format!("failed to build HTTP client: {e}"), 0, 1))?;
        Ok(Self { http, cfg })
    }

    /// Build from the resolved default spec (fatal if the API key is missing).
    pub fn from_env() -> Result<Self, LingmiaoError> {
        Self::new(ModelSpec::from_env()?)
    }

    /// The active model id.
    pub fn model(&self) -> &str {
        &self.cfg.model
    }

    /// The active protocol dialect.
    pub fn dialect(&self) -> Dialect {
        self.cfg.dialect
    }

    /// The active model group id (the API source), e.g. `deepseek`.
    pub fn group_id(&self) -> &str {
        &self.cfg.group_id
    }

    /// The full resolved spec (model / base_url / extra …).
    pub fn spec(&self) -> &ModelSpec {
        &self.cfg
    }

    /// Build a classified [`LingmiaoError`] for an HTTP failure — the message carries
    /// the human category + advice (`error_classify`), and the raw detail /
    /// category / status ride along as structured context for logs + the TUI
    /// diagnostic panel.
    fn http_error(&self, status: u16, body: &str) -> LingmiaoError {
        let info = crate::error_classify::classify_llm_error(Some(status), body);
        // 落盘**原始** provider 响应体 —— cli 2026-09-28: 此前只把分类后的文案
        // 存进 `llm_detail`（截断 400 字符），真实 400 body 全程丢失，无法定位
        // 「量级不可能超窗口却报上下文超限」的真因。这里把 status + 原始 body
        // 写进结构化日志（截断 2000 字符防爆），供事后对齐。
        tracing::error!(
            status,
            category = info.category,
            group = %self.cfg.group_id,
            model = %self.cfg.model,
            body = %truncate_chars(body, 2000),
            "llm http error (raw provider body)"
        );
        LingmiaoError::llm(
            format!("HTTP {status} [{}] —— {}", info.label, info.advice),
            0,
            1,
        )
        .with_context("llm_category", info.category)
        .with_context("llm_status", u64::from(status))
        .with_context("llm_detail", info.detail)
        .with_context("llm_advice", info.advice)
    }

    /// Rough token estimate (Python parity: `max(1, len//3)`).
    pub fn estimate_tokens(text: &str) -> usize {
        (text.chars().count() / 3).max(1)
    }
    fn build_body(&self, req: &ChatRequest, stream: bool) -> Value {
        let mut messages: Vec<Value> = Vec::with_capacity(req.messages.len() + 1);
        if !req.system.is_empty() {
            messages.push(json!({"role": "system", "content": req.system}));
        }
        messages.extend(req.messages.iter().map(Message::to_wire));

        let mut body = json!({
            "model": self.cfg.model,
            "messages": messages,
            "stream": stream,
            "temperature": req.temperature,
        });
        if let Some(mt) = req.max_tokens {
            body["max_tokens"] = json!(mt);
        }
        if let Some(tools) = &req.tools {
            let defs: Vec<Value> = tools.iter().map(ToolDef::to_wire).collect();
            body["tools"] = Value::Array(defs);
            body["tool_choice"] = json!(req.tool_choice);
        }
        // Pass the model's `extra` keys straight into the body (需求② §3.5):
        // a DeepSeek-style `thinking: true` becomes the wire toggle, every
        // other key is forwarded verbatim. No per-provider `match` remains —
        // provider quirks are now data, not code.
        let mut thinking_set = false;
        if let Value::Object(extra) = &self.cfg.extra {
            for (k, v) in extra {
                if k == "thinking"
                    && let Some(b) = v.as_bool()
                {
                    body["thinking"] = json!({"type": if b { "enabled" } else { "disabled" }});
                    thinking_set = true;
                    continue;
                }
                body[k] = v.clone();
            }
        }
        // Honour an explicit request-level thinking toggle when the model spec
        // does not already dictate one.
        if !thinking_set && req.thinking {
            body["thinking"] = json!({"type": "enabled"});
        }
        if stream {
            body["stream_options"] = json!({"include_usage": true});
        }
        body
    }

    /// Non-streaming completion (used by pipeline stages).
    ///
    /// Dispatches on the spec's protocol dialect: the OpenAI wire shape goes
    /// out as-is, the Anthropic shape goes through the
    /// [`crate::anthropic`] translation layer.
    pub async fn chat(&self, req: ChatRequest) -> Result<LlmResponse, LingmiaoError> {
        match self.cfg.dialect {
            Dialect::OpenAi => self.chat_openai(req).await,
            Dialect::Anthropic => self.chat_anthropic(req).await,
        }
    }

    /// OpenAI-compatible non-streaming completion.
    async fn chat_openai(&self, req: ChatRequest) -> Result<LlmResponse, LingmiaoError> {
        let body = self.build_body(&req, false);
        let resp = self
            .http
            .post(self.cfg.chat_url())
            .bearer_auth(&self.cfg.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| LingmiaoError::llm(format!("request failed: {e}"), 0, 1))?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(self.http_error(status.as_u16(), &text));
        }
        let v: Value = resp
            .json()
            .await
            .map_err(|e| LingmiaoError::llm(format!("bad JSON response: {e}"), 0, 1))?;
        Ok(response_from_body(&v))
    }

    /// Anthropic Messages API non-streaming completion (translation layer).
    ///
    /// Uses the `x-api-key` + `anthropic-version` headers (not a bearer token)
    /// and parses the Messages response shape back into an [`LlmResponse`].
    async fn chat_anthropic(&self, req: ChatRequest) -> Result<LlmResponse, LingmiaoError> {
        let body = crate::anthropic::build_body(&self.cfg, &req, false);
        let resp = self
            .http
            .post(self.cfg.chat_url())
            .header("x-api-key", &self.cfg.api_key)
            .header("anthropic-version", crate::anthropic::ANTHROPIC_VERSION)
            .json(&body)
            .send()
            .await
            .map_err(|e| LingmiaoError::llm(format!("request failed: {e}"), 0, 1))?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(self.http_error(status.as_u16(), &text));
        }
        let v: Value = resp
            .json()
            .await
            .map_err(|e| LingmiaoError::llm(format!("bad JSON response: {e}"), 0, 1))?;
        Ok(crate::anthropic::response_to_llm_response(&v))
    }

    /// Streaming completion; the caller drains the returned channel.
    ///
    /// The HTTP request is issued eagerly so an immediate failure (auth, 4xx)
    /// is reported as `Err` here; transient errors *during* streaming arrive as
    /// [`StreamDelta::Error`].
    pub async fn chat_stream(
        &self,
        req: ChatRequest,
    ) -> Result<mpsc::Receiver<StreamDelta>, LingmiaoError> {
        match self.cfg.dialect {
            Dialect::OpenAi => self.chat_stream_openai(req).await,
            Dialect::Anthropic => self.chat_stream_anthropic(req).await,
        }
    }

    /// OpenAI-compatible streaming completion (SSE, `[DONE]`-terminated).
    async fn chat_stream_openai(
        &self,
        req: ChatRequest,
    ) -> Result<mpsc::Receiver<StreamDelta>, LingmiaoError> {
        let body = self.build_body(&req, true);
        let resp = self
            .http
            .post(self.cfg.chat_url())
            .bearer_auth(&self.cfg.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| LingmiaoError::llm(format!("request failed: {e}"), 0, 1))?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(self.http_error(status.as_u16(), &text));
        }

        let (tx, rx) = mpsc::channel(STREAM_BUFFER);
        tokio::spawn(async move {
            let mut stream = resp.bytes_stream();
            let mut decoder = SseDecoder::default();
            while let Some(chunk) = stream.next().await {
                match chunk {
                    Ok(bytes) => {
                        for data in decoder.push(&bytes) {
                            if data == "[DONE]" {
                                return;
                            }
                            for delta in deltas_from_json(&data) {
                                if tx.send(delta).await.is_err() {
                                    return; // consumer dropped
                                }
                            }
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(StreamDelta::Error(e.to_string())).await;
                        return;
                    }
                }
            }
        });
        Ok(rx)
    }

    /// Anthropic Messages streaming completion — the translation layer's SSE
    /// path (no `[DONE]`; the stream ends with `message_stop`).
    async fn chat_stream_anthropic(
        &self,
        req: ChatRequest,
    ) -> Result<mpsc::Receiver<StreamDelta>, LingmiaoError> {
        let body = crate::anthropic::build_body(&self.cfg, &req, true);
        let resp = self
            .http
            .post(self.cfg.chat_url())
            .header("x-api-key", &self.cfg.api_key)
            .header("anthropic-version", crate::anthropic::ANTHROPIC_VERSION)
            .json(&body)
            .send()
            .await
            .map_err(|e| LingmiaoError::llm(format!("request failed: {e}"), 0, 1))?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(self.http_error(status.as_u16(), &text));
        }

        let (tx, rx) = mpsc::channel(STREAM_BUFFER);
        tokio::spawn(async move {
            let mut stream = resp.bytes_stream();
            let mut decoder = SseDecoder::default();
            let mut parser = crate::anthropic::AnthropicStreamParser::default();
            while let Some(chunk) = stream.next().await {
                match chunk {
                    Ok(bytes) => {
                        for data in decoder.push(&bytes) {
                            for delta in parser.push(&data) {
                                if tx.send(delta).await.is_err() {
                                    return; // consumer dropped
                                }
                            }
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(StreamDelta::Error(e.to_string())).await;
                        return;
                    }
                }
            }
        });
        Ok(rx)
    }
}

/// Truncate `s` to `max` **characters** (not bytes), appending a cut note —
/// used for logging a raw provider error body without blowing up the log.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    format!("{head}…(截断)")
}

/// Parse one SSE `data:` JSON payload into zero or more deltas.
pub fn deltas_from_json(data: &str) -> Vec<StreamDelta> {
    let Ok(v) = serde_json::from_str::<Value>(data) else {
        return Vec::new();
    };
    let mut out = Vec::new();

    if let Some(u) = v.get("usage").filter(|u| !u.is_null()) {
        out.push(StreamDelta::Usage(Usage {
            input_tokens: u.get("prompt_tokens").and_then(Value::as_u64).unwrap_or(0),
            output_tokens: u
                .get("completion_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            total_tokens: u.get("total_tokens").and_then(Value::as_u64).unwrap_or(0),
            tool_calls: Vec::new(),
        }));
    }

    let Some(choice) = v
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
    else {
        return out;
    };

    if let Some(delta) = choice.get("delta") {
        if let Some(c) = delta.get("content").and_then(Value::as_str)
            && !c.is_empty()
        {
            out.push(StreamDelta::Content(c.to_string()));
        }
        if let Some(r) = delta.get("reasoning_content").and_then(Value::as_str)
            && !r.is_empty()
        {
            out.push(StreamDelta::Reasoning(r.to_string()));
        }
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for tc in calls {
                let index = tc.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                let id = tc.get("id").and_then(Value::as_str).map(str::to_string);
                let func = tc.get("function");
                let name = func
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let args = func
                    .and_then(|f| f.get("arguments"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                out.push(StreamDelta::ToolCallArgs {
                    index,
                    id,
                    name,
                    args,
                });
            }
        }
    }
    if let Some(fr) = choice.get("finish_reason").and_then(Value::as_str)
        && !fr.is_empty()
    {
        out.push(StreamDelta::Finish(fr.to_string()));
    }
    out
}

/// Parse a full non-streaming response body.
pub fn response_from_body(v: &Value) -> LlmResponse {
    let Some(choice) = v
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
    else {
        return LlmResponse::default();
    };
    let msg = choice.get("message").cloned().unwrap_or(Value::Null);
    let tool_calls: Vec<ToolCall> = msg
        .get("tool_calls")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .map(|tc| {
                    let id = tc
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let func = tc.get("function");
                    let name = func
                        .and_then(|f| f.get("name"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let raw = func
                        .and_then(|f| f.get("arguments"))
                        .and_then(Value::as_str)
                        .unwrap_or("{}");
                    ToolCall::from_raw(id, name, raw)
                })
                .collect()
        })
        .unwrap_or_default();
    let usage = v.get("usage").filter(|u| !u.is_null());
    LlmResponse {
        content: msg
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        reasoning_content: msg
            .get("reasoning_content")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        tool_calls,
        finish_reason: choice
            .get("finish_reason")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        usage: Usage {
            input_tokens: usage
                .and_then(|u| u.get("prompt_tokens"))
                .and_then(Value::as_u64)
                .unwrap_or(0),
            output_tokens: usage
                .and_then(|u| u.get("completion_tokens"))
                .and_then(Value::as_u64)
                .unwrap_or(0),
            total_tokens: usage
                .and_then(|u| u.get("total_tokens"))
                .and_then(Value::as_u64)
                .unwrap_or(0),
            tool_calls: Vec::new(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_estimate_matches_python() {
        assert_eq!(Client::estimate_tokens(""), 1);
        assert_eq!(Client::estimate_tokens("abcdef"), 2);
    }

    #[test]
    fn parses_content_and_finish_deltas() {
        let data = r#"{"choices":[{"index":0,"delta":{"content":"你好"},"finish_reason":null}]}"#;
        let deltas = deltas_from_json(data);
        assert_eq!(deltas.len(), 1);
        assert!(matches!(&deltas[0], StreamDelta::Content(t) if t == "你好"));

        let done = r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#;
        assert!(matches!(deltas_from_json(done)[0], StreamDelta::Finish(_)));
    }

    #[test]
    fn parses_usage_only_chunk() {
        let data = r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}"#;
        let deltas = deltas_from_json(data);
        assert_eq!(deltas.len(), 1);
        match &deltas[0] {
            StreamDelta::Usage(u) => {
                assert_eq!(u.input_tokens, 10);
                assert_eq!(u.total_tokens, 15);
            }
            other => panic!("expected usage, got {other:?}"),
        }
    }

    #[test]
    fn aggregator_reassembles_split_tool_call() {
        let mut agg = StreamAggregator::default();
        agg.apply(&StreamDelta::Content("hi ".into()));
        agg.apply(&StreamDelta::Content("there".into()));
        agg.apply(&StreamDelta::ToolCallArgs {
            index: 0,
            id: Some("c1".into()),
            name: Some("bash".into()),
            args: "{\"comm".into(),
        });
        agg.apply(&StreamDelta::ToolCallArgs {
            index: 0,
            id: None,
            name: None,
            args: "and\":\"ls\"}".into(),
        });
        agg.apply(&StreamDelta::Finish("tool_calls".into()));
        let resp = agg.finish();
        assert_eq!(resp.content, "hi there");
        assert_eq!(resp.tool_calls.len(), 1);
        assert_eq!(resp.tool_calls[0].name, "bash");
        assert_eq!(resp.tool_calls[0].arguments, json!({"command": "ls"}));
        assert_eq!(resp.finish_reason, "tool_calls");
    }

    #[test]
    fn build_body_forwards_extra_and_translates_thinking() {
        let mut spec = ModelSpec::openai("k", "https://api.deepseek.com", "deepseek-v4-pro");
        spec.extra = json!({"thinking": true, "top_p": 0.9});
        let client = Client::new(spec).unwrap();
        let body = client.build_body(&ChatRequest::chat("sys", vec![]), false);
        assert_eq!(body["thinking"], json!({"type": "enabled"}));
        assert_eq!(body["top_p"], json!(0.9));
        assert_eq!(body["model"], "deepseek-v4-pro");
    }

    #[test]
    fn build_body_emits_max_tokens_only_when_set() {
        // Internal callers (the memory re-ranker) pass `max_tokens`; a plain
        // turn leaves it unset so the provider default applies.
        let spec = ModelSpec::openai("k", "https://api.deepseek.com", "deepseek-v4-pro");
        let client = Client::new(spec).unwrap();
        let mut req = ChatRequest::chat("sys", vec![]);
        assert!(client.build_body(&req, false).get("max_tokens").is_none());
        req.max_tokens = Some(500);
        assert_eq!(client.build_body(&req, false)["max_tokens"], json!(500));
    }

    #[test]
    fn anthropic_dialect_is_routed_to_translation_layer() {
        // The Anthropic protocol is no longer rejected — it is served by the
        // translation layer (crate::anthropic). We assert the routing metadata
        // here without touching the network; the translation itself is covered
        // by the anthropic module's tests.
        let spec = ModelSpec::new(
            Dialect::Anthropic,
            "k",
            "https://api.anthropic.com",
            "claude-fable-5",
        );
        let client = Client::new(spec).unwrap();
        assert_eq!(client.dialect(), Dialect::Anthropic);
        assert_eq!(client.group_id(), "anthropic");
        assert_eq!(
            client.spec().chat_url(),
            "https://api.anthropic.com/v1/messages"
        );
        assert!(!client.spec().is_openai_compatible());
    }

    #[test]
    fn non_stream_body_parses() {
        let body = r#"{
            "choices":[{"message":{"content":"ok","tool_calls":[
                {"id":"c1","type":"function","function":{"name":"bash","arguments":"{\"command\":\"pwd\"}"}}
            ]},"finish_reason":"tool_calls"}],
            "usage":{"prompt_tokens":3,"completion_tokens":4,"total_tokens":7}
        }"#;
        let resp = response_from_body(&serde_json::from_str(body).unwrap());
        assert_eq!(resp.content, "ok");
        assert_eq!(resp.tool_calls[0].name, "bash");
        assert_eq!(resp.tool_calls[0].arguments, json!({"command": "pwd"}));
        assert_eq!(resp.usage.total_tokens, 7);
    }

    #[test]
    fn http_error_is_classified_with_context() {
        let spec = ModelSpec::openai("k", "https://api.deepseek.com", "deepseek-v4-pro");
        let client = Client::new(spec).unwrap();
        let err = client.http_error(429, "rate limit exceeded");
        assert!(err.message().contains("限流"), "{}", err.message());
        assert_eq!(err.context().get("llm_category").unwrap(), "rate_limit");
        assert_eq!(err.context().get("llm_status").unwrap(), 429);

        let auth = client.http_error(401, "invalid api key");
        assert!(auth.message().contains("认证"), "{}", auth.message());
        assert_eq!(auth.context().get("llm_category").unwrap(), "auth");
    }
}

//! Anthropic Messages API translation layer — ported from `llm/anthropic.py`.
//!
//! Q5 decision: the internal message / tool format stays OpenAI-canonical; the
//! Anthropic protocol is the one dialect that needs a real code adapter
//! ([`crate::provider::Dialect::Anthropic`]). This module is that adapter — it
//! performs the same bidirectional translation the Python original did, so a
//! `claude` group in `models.json` (`protocol: "anthropic"`) works without the
//! engine core knowing anything about the wire format.
//!
//! Compared to routing Claude through an OpenAI proxy, native format preserves
//! **thinking** blocks → surfaced as `reasoning_content`, giving the same
//! reasoning visibility the DeepSeek path has.
//!
//! Translation rules (1:1 with the Python original)
//! ─────────────────────────────────────────────────
//!   message content
//!     str                     → str (pass-through)
//!     [{type:text}, {type:image_url}]
//!                             → [{type:text}, {type:image,source:{…}}]
//!   tools (OpenAI function)
//!     {type:function,function:{name,description,parameters}}
//!                             → {name,description,input_schema}
//!   assistant tool_calls
//!     [{id,name,arguments}]   → [{type:tool_use,id,name,input}]
//!   tool messages
//!     {role:tool,tool_call_id,content}
//!                             → {role:user,content:[{type:tool_result,…}]}
//!   reasoning_content         → thinking block in the assistant array

use lingmiao_core::events::Usage;
use serde_json::{Map, Value, json};

use crate::client::{ChatRequest, LlmResponse, StreamDelta};
use crate::message::{ToolCall, ToolDef};
use crate::provider::ModelSpec;

/// `anthropic-version` header value (matches the Python SDK default).
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Default `max_tokens` when the caller does not set one (Python parity:
/// `max_tokens or 8192`).
pub const DEFAULT_MAX_TOKENS: u64 = 8192;

/// Thinking budget used when `thinking` is enabled (Python parity).
pub const THINKING_BUDGET_TOKENS: u64 = 4096;

/// Map an Anthropic `stop_reason` to the internal (OpenAI-flavoured) reason.
///
/// `end_turn → stop`, `tool_use → tool_calls`, `max_tokens → length`; anything
/// else passes through unchanged (Python parity).
pub fn map_stop_reason(stop: &str) -> String {
    match stop {
        "end_turn" => "stop",
        "tool_use" => "tool_calls",
        "max_tokens" => "length",
        other => other,
    }
    .to_string()
}

/// Convert an OpenAI-format message `content` value to Anthropic blocks.
///
/// Strings pass through; a multipart array has its `text` parts re-emitted and
/// its `image_url` data URLs turned into `{type:image,source:{type:base64,…}}`.
pub fn content_to_anthropic(content: &Value) -> Value {
    let Value::Array(parts) = content else {
        return content.clone();
    };
    let mut blocks: Vec<Value> = Vec::with_capacity(parts.len());
    for part in parts {
        match part.get("type").and_then(Value::as_str) {
            Some("text") => {
                blocks.push(json!({
                    "type": "text",
                    "text": part.get("text").cloned().unwrap_or(Value::String(String::new())),
                }));
            }
            Some("image_url") => {
                let url = part
                    .get("image_url")
                    .and_then(|i| i.get("url"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if let Some(rest) = url.strip_prefix("data:")
                    && let Some((header, b64)) = rest.split_once(',')
                {
                    let mime = header.strip_suffix(";base64").unwrap_or(header);
                    blocks.push(json!({
                        "type": "image",
                        "source": {"type": "base64", "media_type": mime, "data": b64},
                    }));
                }
            }
            _ => blocks.push(part.clone()),
        }
    }
    Value::Array(blocks)
}

/// Convert OpenAI tool declarations to Anthropic `tools[]` entries.
pub fn tools_to_anthropic(tools: &[ToolDef]) -> Value {
    let out: Vec<Value> = tools
        .iter()
        .map(|t| {
            let mut item = Map::new();
            item.insert("name".into(), json!(t.name));
            if !t.description.is_empty() {
                item.insert("description".into(), json!(t.description));
            }
            item.insert("input_schema".into(), t.parameters.clone());
            Value::Object(item)
        })
        .collect();
    Value::Array(out)
}

/// Convert a full OpenAI-format message list (each via [`Message::to_wire`]) to
/// the Anthropic Messages shape.
pub fn messages_to_anthropic(wire: &[Value]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::with_capacity(wire.len());
    for msg in wire {
        let role = msg.get("role").and_then(Value::as_str).unwrap_or("");
        let content = msg
            .get("content")
            .cloned()
            .unwrap_or(Value::String(String::new()));
        let tcs = msg.get("tool_calls").and_then(Value::as_array);
        let reasoning = msg
            .get("reasoning_content")
            .and_then(Value::as_str)
            .unwrap_or("");

        // tool result → user turn carrying a `tool_result` block.
        if role == "tool" {
            let tool_content = match &content {
                Value::String(_) => content.clone(),
                Value::Array(_) => content_to_anthropic(&content),
                other => Value::String(other.to_string()),
            };
            out.push(json!({
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": msg.get("tool_call_id").and_then(Value::as_str).unwrap_or(""),
                    "content": tool_content,
                }],
            }));
            continue;
        }

        // assistant turn with tool calls and/or reasoning → content blocks.
        let has_calls = tcs.map(|a| !a.is_empty()).unwrap_or(false);
        if role == "assistant" && (has_calls || !reasoning.is_empty()) {
            let mut blocks: Vec<Value> = Vec::new();
            if !reasoning.is_empty() {
                blocks.push(json!({"type": "thinking", "thinking": reasoning}));
            }
            if let Value::String(s) = &content
                && !s.trim().is_empty()
            {
                blocks.push(json!({"type": "text", "text": s}));
            }
            if let Some(tcs) = tcs {
                for tc in tcs {
                    // tolerate both nested {type:function,function:{…}} and flat.
                    let func = tc.get("function").unwrap_or(tc);
                    let name = func
                        .get("name")
                        .and_then(Value::as_str)
                        .or_else(|| tc.get("name").and_then(Value::as_str))
                        .unwrap_or("");
                    let args_raw = func
                        .get("arguments")
                        .cloned()
                        .or_else(|| tc.get("arguments").cloned())
                        .unwrap_or_else(|| json!({}));
                    let input = match args_raw {
                        Value::String(s) => {
                            serde_json::from_str::<Value>(&s).unwrap_or_else(|_| json!({}))
                        }
                        v => v,
                    };
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": tc.get("id").and_then(Value::as_str).unwrap_or(""),
                        "name": name,
                        "input": input,
                    }));
                }
            }
            out.push(json!({"role": "assistant", "content": blocks}));
            continue;
        }

        out.push(json!({"role": role, "content": content_to_anthropic(&content)}));
    }
    out
}

/// The Anthropic `tool_choice` object for a request, or `None` to omit it.
///
/// Only emitted when tools are present — Claude rejects `tool_choice` without
/// `tools`. `auto → {type:auto}`, `any`/`required → {type:any}`, otherwise
/// `{type:tool,name}`.
fn tool_choice(req: &ChatRequest) -> Option<Value> {
    if req.tools.as_ref().map(|t| t.is_empty()).unwrap_or(true) {
        return None;
    }
    Some(match req.tool_choice.as_str() {
        "auto" | "" => json!({"type": "auto"}),
        "any" | "required" => json!({"type": "any"}),
        other => json!({"type": "tool", "name": other}),
    })
}

/// The Anthropic `thinking` parameter (Python parity: always emitted when a
/// model `thinking` extra or a request-level toggle is set, otherwise disabled).
fn thinking_param(req: &ChatRequest, spec: &ModelSpec) -> Value {
    let requested = req.thinking
        || spec
            .extra
            .get("thinking")
            .and_then(Value::as_bool)
            .unwrap_or(false);
    if requested {
        json!({"type": "enabled", "budget_tokens": THINKING_BUDGET_TOKENS})
    } else {
        json!({"type": "disabled"})
    }
}

/// Build the full Anthropic Messages request body.
pub fn build_body(spec: &ModelSpec, req: &ChatRequest, stream: bool) -> Value {
    let wire: Vec<Value> = req.messages.iter().map(|m| m.to_wire()).collect();
    let mut body = Map::new();
    body.insert("model".into(), json!(spec.model));
    // Python parity: `max_tokens or 8192` — an explicit request-level cap wins.
    let max_tokens = req.max_tokens.map(u64::from).unwrap_or(DEFAULT_MAX_TOKENS);
    body.insert("max_tokens".into(), json!(max_tokens));
    body.insert(
        "messages".into(),
        Value::Array(messages_to_anthropic(&wire)),
    );
    body.insert("temperature".into(), json!(req.temperature));
    if !req.system.is_empty() {
        body.insert("system".into(), json!(req.system));
    }
    if let Some(tools) = &req.tools
        && !tools.is_empty()
    {
        body.insert("tools".into(), tools_to_anthropic(tools));
    }
    if let Some(tc) = tool_choice(req) {
        body.insert("tool_choice".into(), tc);
    }
    body.insert("thinking".into(), thinking_param(req, spec));
    if stream {
        body.insert("stream".into(), json!(true));
    }
    Value::Object(body)
}

/// Convert a non-streaming Anthropic Messages response into the internal
/// [`LlmResponse`].
pub fn response_to_llm_response(v: &Value) -> LlmResponse {
    let blocks = v
        .get("content")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut text_parts: Vec<String> = Vec::new();
    let mut reasoning_parts: Vec<String> = Vec::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    for blk in &blocks {
        match blk.get("type").and_then(Value::as_str) {
            Some("text") => text_parts.push(
                blk.get("text")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            ),
            Some("tool_use") => tool_calls.push(ToolCall {
                id: blk
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                name: blk
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                arguments: blk.get("input").cloned().unwrap_or_else(|| json!({})),
            }),
            Some("thinking") => reasoning_parts.push(
                blk.get("thinking")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            ),
            _ => {}
        }
    }
    let finish_reason = map_stop_reason(v.get("stop_reason").and_then(Value::as_str).unwrap_or(""));
    let usage = v.get("usage");
    let input_tokens = usage
        .and_then(|u| u.get("input_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output_tokens = usage
        .and_then(|u| u.get("output_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let mut u = Usage {
        input_tokens,
        output_tokens,
        total_tokens: input_tokens + output_tokens,
        tool_calls: Vec::new(),
    };
    u.tool_calls = tool_calls.iter().map(|c| json!(c.name)).collect();

    LlmResponse {
        content: text_parts.join("\n"),
        reasoning_content: reasoning_parts.join("\n"),
        tool_calls,
        finish_reason,
        usage: u,
    }
}

/// Stateful parser for the Anthropic SSE stream.
///
/// Anthropic does not use `[DONE]`; the stream ends with `message_stop`. The
/// `input_tokens` count arrives once on `message_start` and the `output_tokens`
/// on `message_delta`, so the parser holds the input count to emit a single
/// combined [`StreamDelta::Usage`] (the [`crate::client::StreamAggregator`]
/// replaces usage wholesale, so a lone output-only delta would zero the input).
#[derive(Debug, Default)]
pub struct AnthropicStreamParser {
    input_tokens: u64,
}

impl AnthropicStreamParser {
    /// Fold one SSE `data:` JSON payload into zero or more deltas.
    pub fn push(&mut self, data: &str) -> Vec<StreamDelta> {
        let Ok(v) = serde_json::from_str::<Value>(data) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        match v.get("type").and_then(Value::as_str).unwrap_or("") {
            "message_start" => {
                if let Some(n) = v
                    .get("message")
                    .and_then(|m| m.get("usage"))
                    .and_then(|u| u.get("input_tokens"))
                    .and_then(Value::as_u64)
                {
                    self.input_tokens = n;
                }
            }
            "content_block_start" => {
                let index = v.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                if let Some(blk) = v.get("content_block")
                    && blk.get("type").and_then(Value::as_str) == Some("tool_use")
                {
                    out.push(StreamDelta::ToolCallArgs {
                        index,
                        id: blk.get("id").and_then(Value::as_str).map(str::to_string),
                        name: blk.get("name").and_then(Value::as_str).map(str::to_string),
                        args: String::new(),
                    });
                }
            }
            "content_block_delta" => {
                let index = v.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                if let Some(delta) = v.get("delta") {
                    match delta.get("type").and_then(Value::as_str).unwrap_or("") {
                        "text_delta" => {
                            if let Some(t) = delta.get("text").and_then(Value::as_str)
                                && !t.is_empty()
                            {
                                out.push(StreamDelta::Content(t.to_string()));
                            }
                        }
                        "thinking_delta" => {
                            if let Some(t) = delta.get("thinking").and_then(Value::as_str)
                                && !t.is_empty()
                            {
                                out.push(StreamDelta::Reasoning(t.to_string()));
                            }
                        }
                        "input_json_delta" => {
                            if let Some(p) = delta.get("partial_json").and_then(Value::as_str)
                                && !p.is_empty()
                            {
                                out.push(StreamDelta::ToolCallArgs {
                                    index,
                                    id: None,
                                    name: None,
                                    args: p.to_string(),
                                });
                            }
                        }
                        _ => {}
                    }
                }
            }
            "message_delta" => {
                if let Some(sr) = v
                    .get("delta")
                    .and_then(|d| d.get("stop_reason"))
                    .and_then(Value::as_str)
                    && !sr.is_empty()
                {
                    out.push(StreamDelta::Finish(map_stop_reason(sr)));
                }
                let output_tokens = v
                    .get("usage")
                    .and_then(|u| u.get("output_tokens"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                out.push(StreamDelta::Usage(Usage {
                    input_tokens: self.input_tokens,
                    output_tokens,
                    total_tokens: self.input_tokens + output_tokens,
                    tool_calls: Vec::new(),
                }));
            }
            // ping / content_block_stop / message_stop carry nothing we need.
            _ => {}
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::StreamAggregator;
    use crate::message::{Message, ToolCall};
    use crate::provider::Dialect;

    fn spec() -> ModelSpec {
        ModelSpec::new(
            Dialect::Anthropic,
            "k",
            "https://api.anthropic.com",
            "claude-fable-5",
        )
    }

    #[test]
    fn string_content_passes_through() {
        assert_eq!(content_to_anthropic(&json!("hello")), json!("hello"));
    }

    #[test]
    fn multipart_content_maps_image_url_to_base64_source() {
        let content = json!([
            {"type": "text", "text": "看图"},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAECAw=="}}
        ]);
        let out = content_to_anthropic(&content);
        assert_eq!(out[0], json!({"type": "text", "text": "看图"}));
        assert_eq!(out[1]["type"], json!("image"));
        assert_eq!(out[1]["source"]["type"], json!("base64"));
        assert_eq!(out[1]["source"]["media_type"], json!("image/png"));
        assert_eq!(out[1]["source"]["data"], json!("AAECAw=="));
    }

    #[test]
    fn tools_use_input_schema() {
        let tools = vec![ToolDef {
            name: "bash".into(),
            description: "run".into(),
            parameters: json!({"type": "object"}),
        }];
        let out = tools_to_anthropic(&tools);
        assert_eq!(out[0]["name"], json!("bash"));
        assert_eq!(out[0]["description"], json!("run"));
        assert_eq!(out[0]["input_schema"], json!({"type": "object"}));
        assert!(out[0].get("function").is_none());
    }

    #[test]
    fn tool_result_becomes_user_tool_result_block() {
        let wire = vec![Message::tool("c1", "ok").to_wire()];
        let out = messages_to_anthropic(&wire);
        assert_eq!(out[0]["role"], json!("user"));
        assert_eq!(out[0]["content"][0]["type"], json!("tool_result"));
        assert_eq!(out[0]["content"][0]["tool_use_id"], json!("c1"));
        assert_eq!(out[0]["content"][0]["content"], json!("ok"));
    }

    #[test]
    fn assistant_tool_call_becomes_tool_use_block() {
        let wire = vec![
            Message::assistant_tool_calls(vec![ToolCall {
                id: "c1".into(),
                name: "bash".into(),
                arguments: json!({"command": "ls"}),
            }])
            .to_wire(),
        ];
        let out = messages_to_anthropic(&wire);
        assert_eq!(out[0]["role"], json!("assistant"));
        assert_eq!(out[0]["content"][0]["type"], json!("tool_use"));
        assert_eq!(out[0]["content"][0]["name"], json!("bash"));
        assert_eq!(out[0]["content"][0]["input"], json!({"command": "ls"}));
    }

    #[test]
    fn build_body_shape() {
        let mut req = ChatRequest::chat("sys", vec![Message::user("hi")]);
        req.tools = Some(vec![ToolDef {
            name: "bash".into(),
            description: "run".into(),
            parameters: json!({"type": "object"}),
        }]);
        let body = build_body(&spec(), &req, true);
        assert_eq!(body["model"], json!("claude-fable-5"));
        assert_eq!(body["max_tokens"], json!(8192));
        assert_eq!(body["system"], json!("sys"));
        assert_eq!(body["stream"], json!(true));
        assert_eq!(body["messages"][0]["role"], json!("user"));
        assert_eq!(body["tools"][0]["name"], json!("bash"));
        assert_eq!(body["tool_choice"], json!({"type": "auto"}));
        assert_eq!(body["thinking"], json!({"type": "disabled"}));
    }

    #[test]
    fn build_body_honours_explicit_max_tokens() {
        let mut req = ChatRequest::chat("s", vec![]);
        req.max_tokens = Some(500);
        let body = build_body(&spec(), &req, false);
        assert_eq!(body["max_tokens"], json!(500));
    }

    #[test]
    fn thinking_enabled_when_requested() {
        let mut req = ChatRequest::chat("s", vec![]);
        req.thinking = true;
        let body = build_body(&spec(), &req, false);
        assert_eq!(
            body["thinking"],
            json!({"type": "enabled", "budget_tokens": 4096})
        );
        assert!(body.get("stream").is_none());
    }

    #[test]
    fn response_parses_text_thinking_and_tool_use() {
        let v = json!({
            "stop_reason": "tool_use",
            "content": [
                {"type": "thinking", "thinking": "let me think"},
                {"type": "text", "text": "ok"},
                {"type": "tool_use", "id": "c1", "name": "bash", "input": {"command": "pwd"}}
            ],
            "usage": {"input_tokens": 10, "output_tokens": 5}
        });
        let resp = response_to_llm_response(&v);
        assert_eq!(resp.content, "ok");
        assert_eq!(resp.reasoning_content, "let me think");
        assert_eq!(resp.tool_calls[0].name, "bash");
        assert_eq!(resp.tool_calls[0].arguments, json!({"command": "pwd"}));
        assert_eq!(resp.finish_reason, "tool_calls");
        assert_eq!(resp.usage.input_tokens, 10);
        assert_eq!(resp.usage.total_tokens, 15);
    }

    #[test]
    fn stream_parser_reassembles_text_and_tool_call() {
        let mut p = AnthropicStreamParser::default();
        let mut agg = StreamAggregator::default();
        let feed = |p: &mut AnthropicStreamParser, agg: &mut StreamAggregator, s: &str| {
            for d in p.push(s) {
                agg.apply(&d);
            }
        };
        feed(
            &mut p,
            &mut agg,
            r#"{"type":"message_start","message":{"usage":{"input_tokens":7,"output_tokens":1}}}"#,
        );
        feed(
            &mut p,
            &mut agg,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hmm"}}"#,
        );
        feed(
            &mut p,
            &mut agg,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi "}}"#,
        );
        feed(
            &mut p,
            &mut agg,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"there"}}"#,
        );
        feed(
            &mut p,
            &mut agg,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"c1","name":"bash"}}"#,
        );
        feed(
            &mut p,
            &mut agg,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"command\":"}}"#,
        );
        feed(
            &mut p,
            &mut agg,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"ls\"}"}}"#,
        );
        feed(
            &mut p,
            &mut agg,
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":12}}"#,
        );
        feed(&mut p, &mut agg, r#"{"type":"message_stop"}"#);

        let resp = agg.finish();
        assert_eq!(resp.content, "hi there");
        assert_eq!(resp.reasoning_content, "hmm");
        assert_eq!(resp.tool_calls[0].name, "bash");
        assert_eq!(resp.tool_calls[0].arguments, json!({"command": "ls"}));
        assert_eq!(resp.finish_reason, "tool_calls");
        assert_eq!(resp.usage.input_tokens, 7);
        assert_eq!(resp.usage.output_tokens, 12);
        assert_eq!(resp.usage.total_tokens, 19);
    }

    #[test]
    fn stop_reason_mapping() {
        assert_eq!(map_stop_reason("end_turn"), "stop");
        assert_eq!(map_stop_reason("tool_use"), "tool_calls");
        assert_eq!(map_stop_reason("max_tokens"), "length");
        assert_eq!(map_stop_reason("stop_sequence"), "stop_sequence");
    }
}

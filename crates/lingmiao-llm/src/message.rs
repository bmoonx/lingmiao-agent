//! OpenAI-canonical message types — ported from `llm/base.py`.
//!
//! Q5 decision: the internal message format stays OpenAI-canonical; other
//! providers translate at the boundary. The Python original passed plain dicts
//! (`{"role": "user", "content": "..."}`) around with `Any` typing; here they
//! become real structs, and [`Message::to_wire`] reproduces the exact JSON the
//! OpenAI SDK emitted (tool-call `arguments` normalized to a JSON *string*).

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

/// Conversation role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// System prompt (usually passed separately, but allowed inline).
    System,
    /// The user / human turn.
    User,
    /// The model's turn.
    Assistant,
    /// A tool result turn.
    Tool,
}

impl Role {
    /// The lowercase wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        }
    }
}

/// A tool invocation requested by the model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    /// Provider call id (echoed back on the tool result message).
    pub id: String,
    /// Tool / function name.
    pub name: String,
    /// Parsed arguments object (empty object when the model emitted malformed JSON).
    pub arguments: Value,
}

impl ToolCall {
    /// Build a call from an id/name and raw argument string, parsing safely.
    pub fn from_raw(id: impl Into<String>, name: impl Into<String>, raw: &str) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            arguments: safe_json_object(raw),
        }
    }
}

/// Parse a JSON object, falling back to `{}` on malformed input.
///
/// Mirrors `_safe_json_loads`: broken tool-call arguments must not kill the turn.
pub fn safe_json_object(raw: &str) -> Value {
    match serde_json::from_str::<Value>(raw) {
        Ok(v) if v.is_object() => v,
        _ => Value::Object(Map::new()),
    }
}

/// One conversation message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    /// Who produced the message.
    pub role: Role,
    /// Text content (`None` on a pure tool-call assistant turn).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Image attachments as base64 data URLs (⑥ vision). When non-empty,
    /// [`Message::to_wire`] emits the OpenAI multipart `content` array
    /// (`text` + `image_url` entries) instead of a bare string.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<String>,
    /// Tool calls (assistant turns only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// Which call this message answers (tool turns only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl Message {
    /// A user message.
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: Some(text.into()),
            images: Vec::new(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    /// A user message carrying image attachments (base64 data URLs) alongside
    /// the text — the ⑥ vision multimodal turn.
    pub fn user_with_images(text: impl Into<String>, images: Vec<String>) -> Self {
        Self {
            role: Role::User,
            content: Some(text.into()),
            images,
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    /// An assistant text message.
    pub fn assistant(text: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: Some(text.into()),
            images: Vec::new(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    /// An assistant message that only requests tool calls.
    pub fn assistant_tool_calls(calls: Vec<ToolCall>) -> Self {
        Self {
            role: Role::Assistant,
            content: None,
            images: Vec::new(),
            tool_calls: calls,
            tool_call_id: None,
        }
    }

    /// A tool-result message.
    pub fn tool(call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: Some(content.into()),
            images: Vec::new(),
            tool_calls: Vec::new(),
            tool_call_id: Some(call_id.into()),
        }
    }

    /// Serialize into the exact OpenAI wire shape (Python `_norm` parity).
    pub fn to_wire(&self) -> Value {
        let mut m = Map::new();
        m.insert("role".into(), json!(self.role.as_str()));
        match self.role {
            Role::Tool => {
                m.insert(
                    "content".into(),
                    json!(self.content.clone().unwrap_or_default()),
                );
                m.insert(
                    "tool_call_id".into(),
                    json!(self.tool_call_id.clone().unwrap_or_default()),
                );
            }
            _ => {
                if self.images.is_empty() {
                    if let Some(c) = &self.content {
                        m.insert("content".into(), json!(c));
                    }
                } else {
                    // ⑥ vision: OpenAI multipart content array. Text part first
                    // (when present), then one `image_url` part per data URL.
                    let mut parts: Vec<Value> = Vec::new();
                    if let Some(c) = &self.content
                        && !c.is_empty()
                    {
                        parts.push(json!({"type": "text", "text": c}));
                    }
                    for url in &self.images {
                        parts.push(json!({
                            "type": "image_url",
                            "image_url": {"url": url},
                        }));
                    }
                    m.insert("content".into(), Value::Array(parts));
                }
                if !self.tool_calls.is_empty() {
                    let calls: Vec<Value> = self
                        .tool_calls
                        .iter()
                        .map(|tc| {
                            let args = serde_json::to_string(&tc.arguments)
                                .unwrap_or_else(|_| "{}".to_string());
                            json!({
                                "id": tc.id,
                                "type": "function",
                                "function": {"name": tc.name, "arguments": args},
                            })
                        })
                        .collect();
                    m.insert("tool_calls".into(), Value::Array(calls));
                }
            }
        }
        Value::Object(m)
    }
}

/// A tool declaration advertised to the model (OpenAI `tools[]` entry).
#[derive(Debug, Clone, PartialEq)]
pub struct ToolDef {
    /// Tool name.
    pub name: String,
    /// Natural-language description.
    pub description: String,
    /// JSON Schema for the arguments.
    pub parameters: Value,
}

impl ToolDef {
    /// Serialize into the OpenAI `{"type":"function","function":{...}}` shape.
    pub fn to_wire(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": self.name,
                "description": self.description,
                "parameters": self.parameters,
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_message_wire_shape() {
        assert_eq!(
            Message::user("hi").to_wire(),
            json!({"role": "user", "content": "hi"})
        );
    }

    #[test]
    fn assistant_tool_call_arguments_become_json_string() {
        let msg = Message::assistant_tool_calls(vec![ToolCall {
            id: "c1".into(),
            name: "bash".into(),
            arguments: json!({"command": "ls"}),
        }]);
        let wire = msg.to_wire();
        assert_eq!(wire["tool_calls"][0]["function"]["name"], json!("bash"));
        // arguments must be a *string* containing JSON, per the OpenAI API.
        let args = wire["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .expect("arguments is a string");
        assert_eq!(
            serde_json::from_str::<Value>(args).unwrap(),
            json!({"command": "ls"})
        );
    }

    #[test]
    fn tool_result_wire_shape() {
        assert_eq!(
            Message::tool("c1", "ok").to_wire(),
            json!({"role": "tool", "content": "ok", "tool_call_id": "c1"})
        );
    }

    #[test]
    fn user_with_images_emits_multipart_content() {
        let msg = Message::user_with_images(
            "看这张图",
            vec!["data:image/png;base64,AAECAw==".to_string()],
        );
        let wire = msg.to_wire();
        assert_eq!(wire["role"], json!("user"));
        let arr = wire["content"].as_array().expect("multipart array");
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["type"], json!("text"));
        assert_eq!(arr[0]["text"], json!("看这张图"));
        assert_eq!(arr[1]["type"], json!("image_url"));
        assert_eq!(
            arr[1]["image_url"]["url"],
            json!("data:image/png;base64,AAECAw==")
        );
        // No images → plain string content (byte-for-byte parity preserved).
        assert_eq!(Message::user("hi").to_wire()["content"], json!("hi"));
    }

    #[test]
    fn malformed_arguments_degrade_to_empty_object() {
        assert_eq!(safe_json_object("{not json"), json!({}));
        assert_eq!(safe_json_object("[1,2]"), json!({}));
    }
}

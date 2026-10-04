//! Model selection + credential resolution — 需求② (方言 × 模型组).
//!
//! The M1 code welded two very different things into one compile-time enum
//! `Provider { DeepSeek, Kimi, Claude }`: the *protocol dialect* (real code — a
//! `match` over request bodies) and the *endpoint data* (base_url / api_key /
//! model id / context window). Because the data was welded into code, adding a
//! new API source (GLM / Qwen / OpenRouter / local vLLM …) meant editing the
//! enum and recompiling.
//!
//! 需求② splits them apart:
//! * [`Dialect`] — the protocol dialect that truly needs an adapter
//!   (`openai` / `anthropic`); the only dimension that is code.
//! * [`ModelSpec`] — pure data (`base_url` / `api_key` / `model` / `extra`),
//!   loaded from `models.json` by [`crate::models::ModelRegistry`].
//!
//! Adding an OpenAI-compatible source is now zero code — one JSON group.

use lingmiao_core::LingmiaoError;

/// Wire protocol dialect — the one dimension that needs a code adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Dialect {
    /// `/chat/completions` + `Authorization: Bearer` (OpenAI wire format).
    /// Covers DeepSeek, Kimi, GLM, Qwen, OpenRouter, SiliconFlow, vLLM, Ollama…
    OpenAi,
    /// `/v1/messages` + `x-api-key` (the Anthropic Messages API, served by the
    /// [`crate::anthropic`] translation layer).
    Anthropic,
}

impl Dialect {
    /// Stable lowercase name (matches the `protocol` field in `models.json`).
    pub fn as_str(self) -> &'static str {
        match self {
            Dialect::OpenAi => "openai",
            Dialect::Anthropic => "anthropic",
        }
    }

    /// Parse the `protocol` field. Returns `None` for an unknown dialect so the
    /// caller can decide (warn + fall back) rather than silently mis-talking.
    pub fn parse(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "openai" | "openai-compatible" | "openai_compatible" | "chat-completions" => {
                Some(Dialect::OpenAi)
            }
            "anthropic" | "messages" | "claude" => Some(Dialect::Anthropic),
            _ => None,
        }
    }

    /// Whether this dialect speaks the OpenAI `/chat/completions` shape.
    pub fn is_openai_compatible(self) -> bool {
        matches!(self, Dialect::OpenAi)
    }

    /// The URL path appended to `base_url` for a chat request.
    pub fn chat_path(self) -> &'static str {
        match self {
            Dialect::OpenAi => "chat/completions",
            Dialect::Anthropic => "v1/messages",
        }
    }
}

/// A fully resolved model connection — one `(group, model)` pair from
/// `models.json`, with the API key already resolved (inline or from env).
#[derive(Debug, Clone)]
pub struct ModelSpec {
    /// Owning group id (one API source), e.g. `deepseek`.
    pub group_id: String,
    /// Group display label (falls back to `group_id`).
    pub group_label: String,
    /// Model display label (falls back to `model`).
    pub model_label: String,
    /// Protocol dialect (the only code dimension).
    pub dialect: Dialect,
    /// Endpoint base (no trailing slash).
    pub base_url: String,
    /// Resolved bearer / x-api-key value (may be empty when listing).
    pub api_key: String,
    /// Model id sent to the provider.
    pub model: String,
    /// Whether this model accepts image (multimodal) input — the ⑥ vision gate.
    /// Only a `supports_vision` model receives `read_file` images as multipart
    /// attachments; others keep the textual marker + an explicit note.
    pub supports_vision: bool,
    /// Extra keys passed through into the request body (e.g. DeepSeek
    /// `thinking`). An empty object when none.
    pub extra: serde_json::Value,
}

impl ModelSpec {
    /// Build a spec from explicit values (used by tests + programmatic callers).
    pub fn new(
        dialect: Dialect,
        api_key: impl Into<String>,
        base_url: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        let mut base_url = base_url.into();
        while base_url.ends_with('/') {
            base_url.pop();
        }
        let model = model.into();
        Self {
            group_id: dialect.as_str().to_string(),
            group_label: dialect.as_str().to_string(),
            model_label: model.clone(),
            dialect,
            base_url,
            api_key: api_key.into(),
            model,
            supports_vision: false,
            extra: serde_json::json!({}),
        }
    }

    /// Convenience constructor for an OpenAI-compatible spec.
    pub fn openai(
        api_key: impl Into<String>,
        base_url: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self::new(Dialect::OpenAi, api_key, base_url, model)
    }

    /// The chat endpoint URL for this spec.
    pub fn chat_url(&self) -> String {
        format!("{}/{}", self.base_url, self.dialect.chat_path())
    }

    /// Whether this spec speaks the OpenAI wire format.
    pub fn is_openai_compatible(&self) -> bool {
        self.dialect.is_openai_compatible()
    }

    /// `group/model` — the string the TUI shows for the active model.
    pub fn display(&self) -> String {
        format!("{}/{}", self.group_id, self.model)
    }

    /// Resolve the default spec from `models.json` (loading priority per
    /// 需求② §3.3). Fatal when the selected group has no API key.
    pub fn from_env() -> Result<Self, LingmiaoError> {
        crate::models::ModelRegistry::load()?.default_spec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dialect_parses_canonical_and_aliases() {
        assert_eq!(Dialect::parse("openai"), Some(Dialect::OpenAi));
        assert_eq!(Dialect::parse("OpenAI"), Some(Dialect::OpenAi));
        assert_eq!(Dialect::parse("anthropic"), Some(Dialect::Anthropic));
        assert_eq!(Dialect::parse("claude"), Some(Dialect::Anthropic));
        assert_eq!(Dialect::parse("nonsense"), None);
        assert!(Dialect::OpenAi.is_openai_compatible());
        assert!(!Dialect::Anthropic.is_openai_compatible());
    }

    #[test]
    fn chat_url_appends_protocol_path() {
        let o = ModelSpec::openai("k", "https://api.deepseek.com/", "m");
        assert_eq!(o.base_url, "https://api.deepseek.com");
        assert_eq!(o.chat_url(), "https://api.deepseek.com/chat/completions");
        let a = ModelSpec::new(Dialect::Anthropic, "k", "https://api.anthropic.com", "m");
        assert_eq!(a.chat_url(), "https://api.anthropic.com/v1/messages");
        assert_eq!(o.display(), "openai/m");
    }
}

//! # lingmiao-llm — LLM provider layer
//!
//! Ported from `llm/` (base / factory / deepseek). Decision Q5: `reqwest` +
//! hand-rolled SSE, streaming chunks delivered over an `mpsc` channel of
//! [`StreamDelta`], internal message format kept OpenAI-canonical.
//!
//! M1 scope: OpenAI-compatible providers (DeepSeek + Kimi). The Anthropic
//! translation layer (Claude) is implemented in [`anthropic`] — the OpenAI↔
//! Anthropic message / tool / thinking / SSE translation the Python original
//! did — so a `protocol: "anthropic"` group in `models.json` works natively.
//!
//! [`UserConfig`] (`config.json`, cli 2026-09-27) is the single user-facing
//! configuration file: it may carry the model catalogue itself *and* route each
//! pipeline stage to its own `group/model` — i.e. its own API source.

#![forbid(unsafe_code)]

pub mod anthropic;
pub mod client;
pub mod decide;
pub mod error_classify;
pub mod message;
pub mod models;
pub mod provider;
pub mod sse;
pub mod userconfig;

pub use anthropic::AnthropicStreamParser;
pub use client::{ChatRequest, Client, LlmResponse, StreamAggregator, StreamDelta};
pub use decide::{DECIDE_TIMEOUT, ModelJudge, judges_by_stage};
pub use error_classify::{
    CATEGORIES, LlmErrorInfo, category_advice, category_label, classify_llm_error,
    collect_llm_params, format_error_panel, format_error_summary,
};
pub use message::{Message, Role, ToolCall, ToolDef};
pub use models::ModelRegistry;
pub use provider::{Dialect, ModelSpec};
pub use sse::SseDecoder;
pub use userconfig::{StageSel, UserConfig};

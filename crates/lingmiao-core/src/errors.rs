//! Layered error hierarchy — ported from `core/errors.py`.
//!
//! The original design has a single base error class carrying a
//! `recoverable: bool` flag that drives graceful degradation (a single stage
//! failing should not crash the pipeline). Rust keeps that information as the
//! first-class [`LingmiaoError`] struct: a [`kind`](LingmiaoErrorKind) discriminant plus a
//! [`recoverable`](LingmiaoError::recoverable) flag and a machine-readable context map.
//!
//! Crate-specific enums (e.g. [`ConfigError`]) convert into [`LingmiaoError`] at the
//! boundary so downstream code only handles one error type.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use serde_json::Value;

/// The seven error classes from `core/errors.py`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LingmiaoErrorKind {
    /// Non-fatal, can be safely degraded (single tool call failed).
    Safe,
    /// Unrecoverable, must terminate the session (missing API key, corrupt store).
    Fatal,
    /// A tool execution failed. Recoverable by default.
    Tool,
    /// An LLM API call failed. Recoverable while retries remain.
    Llm,
    /// A memory layer (observations / knowledge / archive) operation failed.
    Memory,
    /// Configuration is invalid or missing. Usually fatal at startup.
    Config,
    /// A pipeline stage (B / C / 沉淀阶段) failed. Recoverable by default.
    Stage,
}

impl LingmiaoErrorKind {
    /// Stable lowercase name, handy for logs and event payloads.
    pub fn as_str(self) -> &'static str {
        match self {
            LingmiaoErrorKind::Safe => "safe",
            LingmiaoErrorKind::Fatal => "fatal",
            LingmiaoErrorKind::Tool => "tool",
            LingmiaoErrorKind::Llm => "llm",
            LingmiaoErrorKind::Memory => "memory",
            LingmiaoErrorKind::Config => "config",
            LingmiaoErrorKind::Stage => "stage",
        }
    }
}

impl fmt::Display for LingmiaoErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Unified error — the Rust counterpart of `原始错误基类`.
#[derive(Debug)]
pub struct LingmiaoError {
    kind: LingmiaoErrorKind,
    recoverable: bool,
    message: String,
    context: BTreeMap<String, Value>,
}

impl LingmiaoError {
    /// The error class.
    pub fn kind(&self) -> LingmiaoErrorKind {
        self.kind
    }

    /// Whether the engine may degrade gracefully on this error.
    pub fn recoverable(&self) -> bool {
        self.recoverable
    }

    /// The human-readable message.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Machine-readable context for logging / TUI display.
    pub fn context(&self) -> &BTreeMap<String, Value> {
        &self.context
    }

    fn base(kind: LingmiaoErrorKind, message: impl Into<String>, recoverable: bool) -> Self {
        Self {
            kind,
            recoverable,
            message: message.into(),
            context: BTreeMap::new(),
        }
    }

    /// Error that the engine may safely degrade on.
    pub fn safe(message: impl Into<String>) -> Self {
        Self::base(LingmiaoErrorKind::Safe, message, true)
    }

    /// Unrecoverable error — terminate the session.
    pub fn fatal(message: impl Into<String>) -> Self {
        Self::base(LingmiaoErrorKind::Fatal, message, false)
    }

    /// Tool execution failure (recoverable, carries the tool name + args).
    pub fn tool(
        tool_name: impl Into<String>,
        tool_args: Value,
        message: impl Into<String>,
    ) -> Self {
        let mut err = Self::base(LingmiaoErrorKind::Tool, message, true);
        err.context
            .insert("tool_name".into(), Value::String(tool_name.into()));
        err.context.insert("tool_args".into(), tool_args);
        err
    }

    /// LLM call failure. Recoverable while `attempt < max_retries`.
    pub fn llm(message: impl Into<String>, attempt: u32, max_retries: u32) -> Self {
        let mut err = Self::base(LingmiaoErrorKind::Llm, message, attempt < max_retries);
        err.context.insert("attempt".into(), Value::from(attempt));
        err.context
            .insert("max_retries".into(), Value::from(max_retries));
        err
    }

    /// Memory store failure (recoverable — falls back to degraded mode).
    pub fn memory(store: impl Into<String>, message: impl Into<String>) -> Self {
        let mut err = Self::base(LingmiaoErrorKind::Memory, message, true);
        err.context
            .insert("store".into(), Value::String(store.into()));
        err
    }

    /// Invalid / missing configuration (fatal).
    pub fn config(message: impl Into<String>) -> Self {
        Self::base(LingmiaoErrorKind::Config, message, false)
    }

    /// Pipeline stage failure (recoverable — one stage must not kill the pipeline).
    pub fn stage(stage: impl Into<String>, message: impl Into<String>) -> Self {
        let mut err = Self::base(LingmiaoErrorKind::Stage, message, true);
        err.context
            .insert("stage".into(), Value::String(stage.into()));
        err
    }

    /// Attach an arbitrary context entry (builder style).
    pub fn with_context(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.context.insert(key.into(), value.into());
        self
    }
}

impl fmt::Display for LingmiaoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}", self.kind, self.message)
    }
}

impl std::error::Error for LingmiaoError {}

/// Configuration-loading error — the crate-specific typed error (Q8).
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// A config file could not be read from an override directory.
    #[error("failed to read config file {path}: {source}")]
    Io {
        /// Path that failed.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// The config payload could not be parsed.
    #[error("invalid config in {file}: {detail}")]
    Invalid {
        /// Config file name (e.g. `stages.json`).
        file: String,
        /// Why parsing failed.
        detail: String,
    },
    /// An override directory was requested but does not exist.
    #[error("config directory not found: {0}")]
    DirNotFound(PathBuf),
}

impl From<ConfigError> for LingmiaoError {
    fn from(e: ConfigError) -> Self {
        LingmiaoError::config(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn llm_error_recoverability_tracks_retries() {
        assert!(LingmiaoError::llm("boom", 1, 3).recoverable());
        assert!(!LingmiaoError::llm("boom", 3, 3).recoverable());
    }

    #[test]
    fn stage_and_tool_errors_are_recoverable() {
        assert!(LingmiaoError::stage("沉淀阶段", "x").recoverable());
        assert!(LingmiaoError::tool("bash", Value::Null, "x").recoverable());
        assert!(!LingmiaoError::config("bad").recoverable());
        assert!(!LingmiaoError::fatal("dead").recoverable());
    }

    #[test]
    fn tool_error_carries_name_and_args() {
        let err = LingmiaoError::tool("bash", Value::from("ls"), "exploded");
        assert_eq!(err.context().get("tool_name"), Some(&Value::from("bash")));
    }
}

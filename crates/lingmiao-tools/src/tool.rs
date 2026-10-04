//! Core tool abstraction (decision Q6).
//!
//! Everything in `lingmiao-tools` is a [`Tool`]: a name, a natural-language
//! description, a JSON Schema for its arguments (derived with
//! [`schemars`](schemars::JsonSchema)), and an async [`execute`](Tool::execute).
//!
//! The engine does not talk to bare schemas: [`ToolRegistry::schemas`] emits the
//! **OpenAI canonical function-calling tools array** (Q5/Q8) directly, so the
//! LLM layer can hand it to a request without re-shaping anything:
//!
//! ```json
//! [ { "type": "function",
//!     "function": { "name": "...", "description": "...", "parameters": { ... } } } ]
//! ```

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use lingmiao_core::LingmiaoError;
use serde_json::{Value, json};

/// A single tool result fed back to the model.
///
/// `is_error` mirrors the canonical tool-result flag: a failing tool still
/// returns content (the error text) so the model can recover, but flags it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutput {
    /// The textual payload returned to the model.
    pub content: String,
    /// Whether this output represents a tool failure.
    pub is_error: bool,
    /// A **display-only** unified diff of what this call changed (CC's
    /// `structuredPatch`), for the TUI's red/green tool card.
    ///
    /// Empty for virtually every tool: only the file mutations (`edit` /
    /// `write_file`) fill it. It never reaches the model — the engine copies it
    /// into [`lingmiao_core::events::Event::ToolCalled`]'s `diff`, while the model
    /// keeps reading [`Self::content`].
    pub diff: Vec<crate::diff::DiffLine>,
}

impl ToolOutput {
    /// A successful result.
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            diff: Vec::new(),
        }
    }

    /// A failed result (content carries the error message).
    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            diff: Vec::new(),
        }
    }

    /// Attach a display-only diff (file mutations, CC's `structuredPatch`).
    pub fn with_diff(mut self, diff: Vec<crate::diff::DiffLine>) -> Self {
        self.diff = diff;
        self
    }
}

/// Layered tool error (Q8 keeps the `recoverable` semantics upstream — every
/// tool error converts into a recoverable [`LingmiaoError`] at the boundary).
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    /// No tool with that name is registered.
    #[error("tool `{0}` is not registered")]
    NotFound(String),
    /// The arguments did not match the tool's schema.
    #[error("invalid arguments for `{tool}`: {detail}")]
    InvalidArgs {
        /// Tool name.
        tool: String,
        /// Why parsing failed.
        detail: String,
    },
    /// The requested path is outside the allowed sandbox.
    #[error("path not allowed: {path} ({reason})")]
    PathNotAllowed {
        /// The offending path as supplied.
        path: String,
        /// Human-readable reason.
        reason: String,
    },
    /// A file must be read before it may be written (rules.md work style).
    #[error("read-before-write required: `{0}` must be read first")]
    ReadRequired(String),
    /// The on-disk content changed since the last read (stale-write guard).
    #[error("stale content: `{0}` changed on disk since it was read")]
    Stale(String),
    /// Underlying I/O failure.
    #[error("i/o error on `{path}`: {source}")]
    Io {
        /// Path the operation targeted.
        path: String,
        /// Wrapped I/O error.
        #[source]
        source: std::io::Error,
    },
    /// Anything else (used by the MCP adapter for remote failures).
    #[error("{0}")]
    Other(String),
}

impl ToolError {
    /// Invalid-arguments helper.
    pub fn invalid(tool: &str, detail: impl Into<String>) -> Self {
        Self::InvalidArgs {
            tool: tool.to_string(),
            detail: detail.into(),
        }
    }

    /// I/O helper carrying the path.
    pub fn io(path: impl Into<String>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}

/// Tool errors are recoverable: a single failed call must not kill the loop.
impl From<ToolError> for LingmiaoError {
    fn from(e: ToolError) -> Self {
        LingmiaoError::safe(e.to_string())
    }
}

/// A tool the agent can call.
#[async_trait]
pub trait Tool: Send + Sync {
    /// Unique tool name (goes into the canonical function name field).
    fn name(&self) -> &str;

    /// One-sentence description shown to the model.
    fn description(&self) -> &str;

    /// JSON Schema for the argument object (`{"type":"object",...}`).
    fn parameters(&self) -> Value;

    /// Run the tool with already-parsed JSON arguments.
    async fn execute(&self, arguments: Value) -> Result<ToolOutput, ToolError>;

    /// Whether the tool mutates external state (files, whiteboard, desktop).
    fn is_mutating(&self) -> bool {
        false
    }

    /// The canonical OpenAI function-calling entry for this tool.
    fn canonical(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": self.name(),
                "description": self.description(),
                "parameters": self.parameters(),
            }
        })
    }
}

/// Convert a [`schemars`] type into a `serde_json::Value` JSON Schema.
pub fn json_schema<T: schemars::JsonSchema>() -> Value {
    serde_json::to_value(schemars::schema_for!(T)).unwrap_or(Value::Null)
}

/// Name → tool registry. Insertion keeps a deterministic (sorted) order so the
/// canonical array is stable across runs (good for golden-file diffs, Q6).
///
/// The maps sit behind an `RwLock` so the registry can be **shared (`Arc`) and
/// still accept late registrations** — ⑤ MCP attach runs in the background
/// after startup (CC's non-blocking connect), so the tools it discovers must
/// land in an already-shared registry. Reads (`schemas_for`, `execute`) take a
/// read lock; only the (rare) registrations take the write lock.
#[derive(Default)]
pub struct ToolRegistry {
    tools: std::sync::RwLock<BTreeMap<String, Arc<dyn Tool>>>,
    /// Names of tools discovered from remote MCP servers (⑤). Tracked separately
    /// because their names are dynamic — `stages.json` cannot enumerate them, so
    /// a stage whitelist spells them as the `mcp:*` pattern instead.
    remote: std::sync::RwLock<std::collections::BTreeSet<String>>,
}

impl ToolRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a tool by value.
    pub fn register<T: Tool + 'static>(&self, tool: T) {
        self.register_arc(Arc::new(tool));
    }

    /// Register a tool behind an existing `Arc` (shared handles).
    pub fn register_arc(&self, tool: Arc<dyn Tool>) {
        let name = tool.name().to_string();
        if let Ok(mut tools) = self.tools.write() {
            tools.insert(name, tool);
        }
    }

    /// Register a **remote (MCP)** tool behind an `Arc`, tagging it so the
    /// `mcp:*` whitelist pattern matches it (⑤ MCP 接上).
    ///
    /// Remote tools keep their original (server-supplied) names — no prefix —
    /// so a static stage whitelist cannot name them; tagging them here is what
    /// lets [`schemas_for`](Self::schemas_for) resolve `mcp:*`.
    pub fn register_arc_remote(&self, tool: Arc<dyn Tool>) {
        let name = tool.name().to_string();
        if let Ok(mut remote) = self.remote.write() {
            remote.insert(name.clone());
        }
        if let Ok(mut tools) = self.tools.write() {
            tools.insert(name, tool);
        }
    }

    /// Look a tool up by name.
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.read().ok()?.get(name).cloned()
    }

    /// Registered tool names, sorted.
    pub fn names(&self) -> Vec<String> {
        self.tools
            .read()
            .map(|t| t.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// Whether `name` is registered.
    pub fn contains(&self, name: &str) -> bool {
        self.tools
            .read()
            .map(|t| t.contains_key(name))
            .unwrap_or(false)
    }

    /// Number of registered tools.
    pub fn len(&self) -> usize {
        self.tools.read().map(|t| t.len()).unwrap_or(0)
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The OpenAI canonical function-calling **tools array** (Q5/Q8).
    pub fn schemas(&self) -> Value {
        let Ok(tools) = self.tools.read() else {
            return Value::Array(Vec::new());
        };
        Value::Array(tools.values().map(|t| t.canonical()).collect())
    }

    /// The canonical tools array restricted to `names` (stage whitelists read
    /// from `stages.json`). Unknown names are skipped; callers can pre-validate
    /// with [`contains`](Self::contains).
    ///
    /// ⑤ MCP 接上: the pattern `mcp:*` is special — it matches **every**
    /// registered remote tool (whose names are dynamic), in addition to any
    /// exact names listed. A whitelist with no remote tools registered simply
    /// yields the exact-name matches.
    pub fn schemas_for(&self, names: &[String]) -> Value {
        let set: std::collections::BTreeSet<&str> = names.iter().map(String::as_str).collect();
        let all_remote = set.contains("mcp:*");
        let Ok(tools) = self.tools.read() else {
            return Value::Array(Vec::new());
        };
        let remote_guard = self.remote.read().ok();
        let remote_set: Option<&std::collections::BTreeSet<String>> = remote_guard.as_deref();
        Value::Array(
            tools
                .iter()
                .filter(|(k, _)| {
                    set.contains(k.as_str())
                        || (all_remote && remote_set.map(|r| r.contains(*k)).unwrap_or(false))
                })
                .map(|(_, t)| t.canonical())
                .collect(),
        )
    }

    /// The full request-body fragment: `{"tools": [...]}`.
    pub fn canonical_body(&self) -> Value {
        json!({ "tools": self.schemas() })
    }

    /// Execute a tool by name.
    pub async fn execute(&self, name: &str, arguments: Value) -> Result<ToolOutput, ToolError> {
        match self.get(name) {
            Some(tool) => tool.execute(arguments).await,
            None => Err(ToolError::NotFound(name.to_string())),
        }
    }

    /// Whether the named tool mutates external state ([`Tool::is_mutating`]).
    ///
    /// The stage agent consults this to decide **只读并行 / 写串行** (cli
    /// 2026-10-02): a run of consecutive read-only calls may execute
    /// concurrently, while a mutating call keeps the serial slot the model
    /// implied.
    ///
    /// An **unknown** name is reported as mutating on purpose — a tool the
    /// registry cannot classify is never parallelised, so the conservative
    /// answer can only cost speed, never correctness.
    pub fn is_mutating(&self, name: &str) -> bool {
        match self.get(name) {
            Some(tool) => tool.is_mutating(),
            None => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Deserialize, schemars::JsonSchema)]
    struct EchoArgs {
        /// Text to echo back.
        text: String,
    }

    struct EchoTool;

    #[async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "Echo the provided text."
        }
        fn parameters(&self) -> Value {
            json_schema::<EchoArgs>()
        }
        async fn execute(&self, arguments: Value) -> Result<ToolOutput, ToolError> {
            let a: EchoArgs = serde_json::from_value(arguments)
                .map_err(|e| ToolError::invalid("echo", e.to_string()))?;
            Ok(ToolOutput::ok(a.text))
        }
    }

    #[test]
    fn register_and_retrieve() {
        let reg = ToolRegistry::new();
        reg.register(EchoTool);
        assert!(reg.contains("echo"));
        assert_eq!(reg.len(), 1);
        let tool = reg.get("echo").expect("registered");
        assert_eq!(tool.name(), "echo");
        assert!(reg.get("missing").is_none());
    }

    #[tokio::test]
    async fn execute_dispatches_and_reports_not_found() {
        let reg = ToolRegistry::new();
        reg.register(EchoTool);
        let out = reg.execute("echo", json!({"text": "hi"})).await.unwrap();
        assert_eq!(out.content, "hi");
        assert!(!out.is_error);
        assert!(matches!(
            reg.execute("nope", Value::Null).await,
            Err(ToolError::NotFound(_))
        ));
    }

    #[test]
    fn canonical_array_has_openai_shape() {
        let reg = ToolRegistry::new();
        reg.register(EchoTool);
        let schemas = reg.schemas();
        let arr = schemas.as_array().expect("array");
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["type"], "function");
        assert_eq!(arr[0]["function"]["name"], "echo");
        assert!(arr[0]["function"]["parameters"]["properties"]["text"].is_object());
        let body = reg.canonical_body();
        assert!(body["tools"].is_array());
    }

    #[test]
    fn schemas_for_filters_to_whitelist() {
        let reg = ToolRegistry::new();
        reg.register(EchoTool);
        let only = reg.schemas_for(&["echo".to_string()]);
        assert_eq!(only.as_array().unwrap().len(), 1);
        let none = reg.schemas_for(&["missing".to_string()]);
        assert_eq!(none.as_array().unwrap().len(), 0);
    }

    /// A stand-in remote tool (distinct name) for the `mcp:*` wildcard test.
    struct RemoteEchoTool;

    #[async_trait]
    impl Tool for RemoteEchoTool {
        fn name(&self) -> &str {
            "mcp_echo"
        }
        fn description(&self) -> &str {
            "A remote echo."
        }
        fn parameters(&self) -> Value {
            json!({"type": "object", "properties": {}})
        }
        async fn execute(&self, _arguments: Value) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::ok(""))
        }
    }

    #[test]
    fn schemas_for_mcp_wildcard_matches_remote_tools_only() {
        // ⑤ MCP 接上: `mcp:*` expands to every remote-registered tool (dynamic
        // names stages.json cannot spell out), never the builtins.
        let reg = ToolRegistry::new();
        reg.register(EchoTool);
        reg.register_arc_remote(Arc::new(RemoteEchoTool));

        let only = reg.schemas_for(&["mcp:*".to_string()]);
        let names: Vec<&str> = only
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["mcp_echo"]);

        // Wildcard and exact names compose.
        let mixed = reg.schemas_for(&["echo".to_string(), "mcp:*".to_string()]);
        let mut got: Vec<&str> = mixed
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect();
        got.sort_unstable();
        assert_eq!(got, ["echo", "mcp_echo"]);

        // No remote tools registered → `mcp:*` resolves to nothing (not an error).
        let none = ToolRegistry::new().schemas_for(&["mcp:*".to_string()]);
        assert!(none.as_array().unwrap().is_empty());
    }
}

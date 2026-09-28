//! MCP integration (Q6: official `rmcp` SDK) — **M3**.
//!
//! Design: remote MCP servers are adapted to the local [`Tool`] abstraction via
//! a transport-agnostic [`McpSource`] trait. [`RemoteTool`] wraps one server
//! tool and implements [`Tool`], so MCP tools register into the same
//! [`ToolRegistry`] and appear in the same canonical schema array as the
//! builtins — the engine cannot tell them apart.
//!
//! The concrete `rmcp` client lives behind the `mcp` feature (see
//! [`client`]); the abstraction below compiles without it so the registry,
//! builtins and their tests never depend on the MCP dependency tree.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use crate::tool::{Tool, ToolError, ToolOutput, ToolRegistry};

/// A remote tool descriptor discovered from an MCP server.
#[derive(Debug, Clone)]
pub struct McpToolSpec {
    /// Remote tool name.
    pub name: String,
    /// Remote tool description.
    pub description: String,
    /// JSON Schema for the tool's arguments.
    pub input_schema: Value,
}

impl McpToolSpec {
    /// Build a spec with an object schema.
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            input_schema,
        }
    }
}

/// Transport-agnostic handle to an MCP server.
///
/// Implemented by the `rmcp` client (feature `mcp`) and by test fakes.
#[async_trait]
pub trait McpSource: Send + Sync {
    /// Discover the server's tools.
    async fn list_tools(&self) -> Result<Vec<McpToolSpec>, String>;

    /// Invoke a remote tool by name and return its textual result.
    async fn call_tool(&self, name: &str, arguments: Value) -> Result<String, String>;
}

/// A single remote tool adapted into the local [`Tool`] trait.
pub struct RemoteTool {
    source: Arc<dyn McpSource>,
    spec: McpToolSpec,
}

impl RemoteTool {
    /// Wrap a remote spec as a local tool.
    pub fn new(source: Arc<dyn McpSource>, spec: McpToolSpec) -> Self {
        Self { source, spec }
    }
}

#[async_trait]
impl Tool for RemoteTool {
    fn name(&self) -> &str {
        &self.spec.name
    }
    fn description(&self) -> &str {
        &self.spec.description
    }
    fn parameters(&self) -> Value {
        self.spec.input_schema.clone()
    }
    fn is_mutating(&self) -> bool {
        true
    }
    async fn execute(&self, arguments: Value) -> Result<ToolOutput, ToolError> {
        match self.source.call_tool(&self.spec.name, arguments).await {
            Ok(text) => Ok(ToolOutput::ok(text)),
            Err(e) => Ok(ToolOutput::error(e)),
        }
    }
}

/// Discover a source's tools and adapt them (without registering).
pub async fn discover(source: Arc<dyn McpSource>) -> Result<Vec<RemoteTool>, String> {
    let specs = source.list_tools().await?;
    Ok(specs
        .into_iter()
        .map(|spec| RemoteTool::new(source.clone(), spec))
        .collect())
}

/// Discover a source's tools and register them; returns the count added.
///
/// ⑤ MCP 接上: tools go in via [`ToolRegistry::register_arc_remote`], which
/// tags them so a stage whitelist's `mcp:*` pattern includes them (remote names
/// are dynamic and cannot be listed in `stages.json`).
pub async fn register_remote(
    registry: &mut ToolRegistry,
    source: Arc<dyn McpSource>,
) -> Result<usize, String> {
    let tools = discover(source).await?;
    let count = tools.len();
    for tool in tools {
        registry.register_arc_remote(Arc::new(tool));
    }
    Ok(count)
}

#[cfg(feature = "mcp")]
pub mod client;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct FakeSource;

    #[async_trait]
    impl McpSource for FakeSource {
        async fn list_tools(&self) -> Result<Vec<McpToolSpec>, String> {
            Ok(vec![McpToolSpec::new(
                "remote_echo",
                "Echo a value back.",
                json!({"type": "object", "properties": {"v": {"type": "string"}}}),
            )])
        }
        async fn call_tool(&self, name: &str, arguments: Value) -> Result<String, String> {
            Ok(format!("{name}:{}", arguments["v"].as_str().unwrap_or("")))
        }
    }

    #[tokio::test]
    async fn remote_tool_registers_and_executes() {
        let mut reg = ToolRegistry::new();
        let n = register_remote(&mut reg, Arc::new(FakeSource))
            .await
            .unwrap();
        assert_eq!(n, 1);
        assert!(reg.contains("remote_echo"));
        let out = reg
            .execute("remote_echo", json!({"v": "hi"}))
            .await
            .unwrap();
        assert_eq!(out.content, "remote_echo:hi");
        // The canonical array includes the remote tool like any builtin.
        let names: Vec<String> = reg
            .schemas()
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap().to_string())
            .collect();
        assert!(names.contains(&"remote_echo".to_string()));
    }

    /// ⑤ MCP 接上: a stage whitelist can pull in every discovered remote tool
    /// with the `mcp:*` pattern — even though the remote name is dynamic.
    #[tokio::test]
    async fn discovered_remote_tools_resolve_via_mcp_wildcard() {
        let mut reg = ToolRegistry::new();
        register_remote(&mut reg, Arc::new(FakeSource))
            .await
            .unwrap();
        let only = reg.schemas_for(&["mcp:*".to_string()]);
        let names: Vec<&str> = only
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["remote_echo"]);
    }
}

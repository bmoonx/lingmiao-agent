//! Concrete MCP client on the official `rmcp` SDK (feature `mcp`, Q6).
//!
//! [`RmcpSource`] connects to a server over a **stdio child process** (the
//! common transport) and implements the crate's [`McpSource`] trait, so the
//! remote tools register into [`ToolRegistry`] exactly like builtins.

use async_trait::async_trait;
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, CallToolResult, ContentBlock};
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::TokioChildProcess;
use serde_json::{Map, Value};

use super::{McpSource, McpToolSpec};

/// An MCP client backed by `rmcp`, talking to a child process over stdio.
pub struct RmcpSource {
    client: RunningService<RoleClient, ()>,
}

impl RmcpSource {
    /// Spawn `program args...` and perform the MCP initialize handshake.
    pub async fn connect(
        program: impl AsRef<std::ffi::OsStr>,
        args: &[impl AsRef<std::ffi::OsStr>],
    ) -> Result<Self, String> {
        Self::connect_with_env(program, args, &std::collections::HashMap::new()).await
    }

    /// Spawn `program args...` with extra environment variables and perform the
    /// MCP initialize handshake. The `mcp.json` declarations carry per-server
    /// env (e.g. a `PYTHONPATH` for a Python server), so this is the path the
    /// engine uses.
    pub async fn connect_with_env(
        program: impl AsRef<std::ffi::OsStr>,
        args: &[impl AsRef<std::ffi::OsStr>],
        env: &std::collections::HashMap<String, String>,
    ) -> Result<Self, String> {
        let program = program.as_ref().to_os_string();
        let owned: Vec<std::ffi::OsString> =
            args.iter().map(|a| a.as_ref().to_os_string()).collect();
        let env_owned: Vec<(std::ffi::OsString, std::ffi::OsString)> =
            env.iter().map(|(k, v)| (k.into(), v.into())).collect();
        // Spawn with a guard on **all three** stdio ends.
        //
        // cli 2026-09-28 (「刚启动的时候 UI 是乱的，拖动一下窗口大小会变好」):
        // `TokioChildProcess::new` leaves rmcp's default `stderr: Stdio::inherit()`
        // (rmcp 3.4 `transport/child_process.rs`), so a server that dies at launch
        // (e.g. `tui-mcp`'s `node .../server.js` when that module is not installed)
        // writes its node stack trace **straight into the TUI's alternate screen**.
        // ratatui repaints only its own dirty cells, so the foreign text lingered
        // until a resize forced a full repaint — the exact "garbled first frame,
        // fine after a resize" symptom. Pipe it and forward it to `tracing`
        // instead: the diagnostic survives (in the log), the screen stays clean.
        let mut cmd = tokio::process::Command::new(&program);
        cmd.args(&owned);
        cmd.envs(env_owned);
        let (transport, stderr) = TokioChildProcess::builder(cmd)
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("spawn {program:?}: {e}"))?;
        if let Some(stderr) = stderr {
            tokio::spawn(async move {
                use tokio::io::AsyncBufReadExt;
                let mut lines = tokio::io::BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::warn!("mcp server stderr: {line}");
                }
            });
        }
        let client = ().serve(transport).await.map_err(|e| format!("mcp initialize failed: {e}"))?;
        Ok(Self { client })
    }
}

fn content_to_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|b| b.as_text().map(|t| t.text.clone()))
        .collect::<Vec<_>>()
        .join("\n")
}

#[async_trait]
impl McpSource for RmcpSource {
    async fn list_tools(&self) -> Result<Vec<McpToolSpec>, String> {
        let tools = self
            .client
            .list_all_tools()
            .await
            .map_err(|e| format!("tools/list failed: {e}"))?;
        Ok(tools
            .into_iter()
            .map(|t| {
                let schema = Value::Object((*t.input_schema).clone());
                let description = t
                    .description
                    .as_ref()
                    .map(|d| d.to_string())
                    .unwrap_or_default();
                McpToolSpec::new(t.name.to_string(), description, schema)
            })
            .collect())
    }

    async fn call_tool(&self, name: &str, arguments: Value) -> Result<String, String> {
        let map: Map<String, Value> = match arguments {
            Value::Object(m) => m,
            Value::Null => Map::new(),
            other => {
                let mut m = Map::new();
                m.insert("value".to_string(), other);
                m
            }
        };
        let params = CallToolRequestParams::new(name.to_string()).with_arguments(map);
        let result: CallToolResult = self
            .client
            .call_tool(params)
            .await
            .map_err(|e| format!("tools/call `{name}` failed: {e}"))?;
        let text = content_to_text(&result.content);
        if result.is_error.unwrap_or(false) {
            Err(text)
        } else {
            Ok(text)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A non-existent server binary must surface a spawn/initialize error, not
    /// panic or hang — the failure path the registry relies on.
    #[tokio::test]
    async fn connect_to_missing_binary_errors() {
        let err = match RmcpSource::connect("lingmiao-not-a-real-mcp-server-xyz", &["--help"]).await
        {
            Ok(_) => panic!("connect to a missing binary must fail"),
            Err(e) => e,
        };
        assert!(!err.is_empty());
    }
}

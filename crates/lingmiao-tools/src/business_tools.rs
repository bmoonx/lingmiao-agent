//! Business-database tools (Q6 / M4 binding): the free-form #4 store exposed as
//! [`Tool`]s so the model can create and query project-domain tables.
//!
//! `工作阶段` whitelists `business_db_query` / `business_db_execute` /
//! `business_db_schema`; this module closes that part of the binding gap.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;

use lingmiao_memory::Memory;

use crate::tool::{Tool, ToolError, ToolOutput, ToolRegistry, json_schema};

fn parse_args<T: for<'de> Deserialize<'de>>(tool: &str, arguments: Value) -> Result<T, ToolError> {
    serde_json::from_value(arguments).map_err(|e| ToolError::invalid(tool, e.to_string()))
}

fn mem_err(tool: &str, e: lingmiao_core::LingmiaoError) -> ToolError {
    ToolError::Other(format!("{tool}: {e}"))
}

#[derive(Deserialize, schemars::JsonSchema)]
struct SqlArgs {
    /// A single SQL statement.
    sql: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct SchemaArgs {
    /// Table to describe. When empty, lists all tables.
    #[serde(default)]
    table: String,
}

/// `business_db_query` — read-only `SELECT`.
pub struct BusinessQueryTool {
    memory: Arc<Memory>,
}

#[async_trait]
impl Tool for BusinessQueryTool {
    fn name(&self) -> &str {
        "business_db_query"
    }
    fn description(&self) -> &str {
        "Run a read-only SELECT on the business database (#4). Returns rows keyed by column name."
    }
    fn parameters(&self) -> Value {
        json_schema::<SqlArgs>()
    }
    async fn execute(&self, arguments: Value) -> Result<ToolOutput, ToolError> {
        let a: SqlArgs = parse_args("business_db_query", arguments)?;
        if a.sql.trim().is_empty() {
            return Err(ToolError::invalid(
                "business_db_query",
                "`sql` must not be empty",
            ));
        }
        let rows = self
            .memory
            .business
            .query(&a.sql)
            .map_err(|e| mem_err("business_db_query", e))?;
        match serde_json::to_string_pretty(&rows) {
            Ok(s) => Ok(ToolOutput::ok(s)),
            Err(e) => Ok(ToolOutput::error(format!("serialize failed: {e}"))),
        }
    }
}

/// `business_db_execute` — DDL / DML.
pub struct BusinessExecuteTool {
    memory: Arc<Memory>,
}

#[async_trait]
impl Tool for BusinessExecuteTool {
    fn name(&self) -> &str {
        "business_db_execute"
    }
    fn description(&self) -> &str {
        "Execute DDL/DML (CREATE / ALTER / INSERT / UPDATE / DELETE) on the business database (#4)."
    }
    fn parameters(&self) -> Value {
        json_schema::<SqlArgs>()
    }
    fn is_mutating(&self) -> bool {
        true
    }
    async fn execute(&self, arguments: Value) -> Result<ToolOutput, ToolError> {
        let a: SqlArgs = parse_args("business_db_execute", arguments)?;
        if a.sql.trim().is_empty() {
            return Err(ToolError::invalid(
                "business_db_execute",
                "`sql` must not be empty",
            ));
        }
        let n = self
            .memory
            .business
            .execute(&a.sql)
            .map_err(|e| mem_err("business_db_execute", e))?;
        Ok(ToolOutput::ok(format!("{n} row(s) affected")))
    }
}

/// `business_db_schema` — list tables or describe one.
pub struct BusinessSchemaTool {
    memory: Arc<Memory>,
}

#[async_trait]
impl Tool for BusinessSchemaTool {
    fn name(&self) -> &str {
        "business_db_schema"
    }
    fn description(&self) -> &str {
        "List business-database tables, or describe one table's columns when `table` is given."
    }
    fn parameters(&self) -> Value {
        json_schema::<SchemaArgs>()
    }
    async fn execute(&self, arguments: Value) -> Result<ToolOutput, ToolError> {
        let a: SchemaArgs = parse_args("business_db_schema", arguments)?;
        if a.table.is_empty() {
            let tables = self
                .memory
                .business
                .tables()
                .map_err(|e| mem_err("business_db_schema", e))?;
            return match serde_json::to_string_pretty(&tables) {
                Ok(s) => Ok(ToolOutput::ok(s)),
                Err(e) => Ok(ToolOutput::error(format!("serialize failed: {e}"))),
            };
        }
        // Only describe known tables so a table name can never inject SQL.
        let known = self
            .memory
            .business
            .tables()
            .map_err(|e| mem_err("business_db_schema", e))?;
        if !known.iter().any(|t| t == &a.table) {
            return Ok(ToolOutput::error(format!(
                "unknown table `{}` (known: {})",
                a.table,
                known.join(", ")
            )));
        }
        let sql = format!("PRAGMA table_info('{}')", a.table.replace('\'', "''"));
        let rows = self
            .memory
            .business
            .query(&sql)
            .map_err(|e| mem_err("business_db_schema", e))?;
        match serde_json::to_string_pretty(&rows) {
            Ok(s) => Ok(ToolOutput::ok(s)),
            Err(e) => Ok(ToolOutput::error(format!("serialize failed: {e}"))),
        }
    }
}

/// Names of the tools in this group.
pub const TOOL_NAMES: [&str; 3] = [
    "business_db_query",
    "business_db_execute",
    "business_db_schema",
];

/// Register every business-database tool against one shared [`Memory`] handle.
pub fn register(registry: &mut ToolRegistry, memory: Arc<Memory>) {
    registry.register(BusinessQueryTool {
        memory: memory.clone(),
    });
    registry.register(BusinessExecuteTool {
        memory: memory.clone(),
    });
    registry.register(BusinessSchemaTool { memory });
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingmiao_memory::Zone;
    use serde_json::json;

    fn temp_memory(tag: &str) -> (Arc<Memory>, std::path::PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("lingmiao-biztools-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mem = Memory::open_in_dir(&dir, Zone::Chat, None).expect("open memory");
        (Arc::new(mem), dir)
    }

    #[tokio::test]
    async fn create_insert_query_schema() {
        let (mem, dir) = temp_memory("crud");
        let mut reg = ToolRegistry::new();
        register(&mut reg, mem);
        reg.execute(
            "business_db_execute",
            json!({"sql": "CREATE TABLE t (id INTEGER, name TEXT)"}),
        )
        .await
        .unwrap();
        reg.execute(
            "business_db_execute",
            json!({"sql": "INSERT INTO t VALUES (1, 'a')"}),
        )
        .await
        .unwrap();
        let out = reg
            .execute("business_db_query", json!({"sql": "SELECT name FROM t"}))
            .await
            .unwrap();
        assert!(out.content.contains("\"a\""));
        let schema = reg
            .execute("business_db_schema", json!({"table": "t"}))
            .await
            .unwrap();
        assert!(schema.content.contains("name"));
        let tables = reg.execute("business_db_schema", json!({})).await.unwrap();
        assert!(tables.content.contains("\"t\""));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn schema_rejects_unknown_table() {
        let (mem, dir) = temp_memory("unknown");
        let mut reg = ToolRegistry::new();
        register(&mut reg, mem);
        let out = reg
            .execute("business_db_schema", json!({"table": "nope; DROP TABLE x"}))
            .await
            .unwrap();
        assert!(out.is_error);
        std::fs::remove_dir_all(&dir).ok();
    }
}

//! Business database (#4 memory) — ported from `memory/business_store.py`.
//!
//! Unlike the other three stores, this one has **no fixed schema**: the original
//! exposes raw `CREATE TABLE` / `INSERT` / `UPDATE` / `DELETE` / `SELECT` to the
//! agent so it can record project-domain data (clients, invoices, tickets…).
//! The Rust port therefore offers a thin, typed wrapper over raw SQL rather than
//! modelling tables — the schema is whatever the agent created.

use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use lingmiao_core::LingmiaoError;
use rusqlite::types::ValueRef;
use rusqlite::{Connection, params};
use serde_json::{Map, Value};

use crate::id::now_iso;
use crate::store::{STORE, lock, open_db};

const DB_NAME: &str = "business.db";

/// A single result row, keyed by column name.
pub type Row = Map<String, Value>;

/// The business database handle.
pub struct Business {
    conn: Mutex<Connection>,
    rows_written: AtomicU64,
}

impl Business {
    /// Open (creating if needed) the business DB at `path`.
    pub fn open(path: &Path) -> Result<Self, LingmiaoError> {
        let conn = open_db(path)?;
        Ok(Self {
            conn: Mutex::new(conn),
            rows_written: AtomicU64::new(0),
        })
    }

    /// Open the business DB inside a zone directory.
    pub fn open_in_dir(dir: &Path) -> Result<Self, LingmiaoError> {
        Self::open(&dir.join(DB_NAME))
    }

    /// Execute a statement that does not return rows (DDL / INSERT / UPDATE /
    /// DELETE). Returns the number of rows affected.
    pub fn execute(&self, sql: &str) -> Result<usize, LingmiaoError> {
        let conn = lock(&self.conn);
        // `execute_batch` handles multi-statement DDL; `execute` cannot.
        if is_multi_statement(sql) {
            conn.execute_batch(sql)
                .map_err(|e| LingmiaoError::memory(STORE, format!("business execute: {e}")))?;
            Ok(0)
        } else {
            let n = conn
                .execute(sql, [])
                .map_err(|e| LingmiaoError::memory(STORE, format!("business execute: {e}")))?;
            self.rows_written.fetch_add(n as u64, Ordering::Relaxed);
            Ok(n)
        }
    }

    /// Run a read-only `SELECT`, returning rows keyed by column name.
    pub fn query(&self, sql: &str) -> Result<Vec<Row>, LingmiaoError> {
        let conn = lock(&self.conn);
        let mut stmt = conn
            .prepare(sql)
            .map_err(|e| LingmiaoError::memory(STORE, format!("business prepare: {e}")))?;
        let column_names: Vec<String> = stmt
            .column_names()
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        let mut rows = stmt
            .query([])
            .map_err(|e| LingmiaoError::memory(STORE, format!("business query: {e}")))?;
        let mut out = Vec::new();
        while let Some(row) = rows
            .next()
            .map_err(|e| LingmiaoError::memory(STORE, e.to_string()))?
        {
            let mut map = Map::new();
            for (idx, name) in column_names.iter().enumerate() {
                let vref = row
                    .get_ref(idx)
                    .map_err(|e| LingmiaoError::memory(STORE, e.to_string()))?;
                map.insert(name.clone(), value_ref_to_json(vref));
            }
            out.push(map);
        }
        Ok(out)
    }

    /// List user tables (excluding SQLite internals), sorted by name.
    pub fn tables(&self) -> Result<Vec<String>, LingmiaoError> {
        let conn = lock(&self.conn);
        let mut stmt = conn
            .prepare(
                "SELECT name FROM sqlite_master
                 WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
            )
            .map_err(|e| LingmiaoError::memory(STORE, e.to_string()))?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(|e| LingmiaoError::memory(STORE, e.to_string()))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| LingmiaoError::memory(STORE, e.to_string()))?);
        }
        Ok(out)
    }

    /// Row count of a specific table.
    pub fn count(&self, table: &str) -> Result<u64, LingmiaoError> {
        // Guard the identifier against injection: only [A-Za-z0-9_] allowed.
        if !table.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(LingmiaoError::memory(
                STORE,
                format!("invalid table name: {table}"),
            ));
        }
        let conn = lock(&self.conn);
        let sql = format!("SELECT COUNT(*) FROM {table}");
        conn.query_row(&sql, params![], |r| r.get::<_, i64>(0))
            .map(|n| n.max(0) as u64)
            .map_err(|e| LingmiaoError::memory(STORE, e.to_string()))
    }

    /// Rows written across all `execute` calls so far.
    pub fn rows_written(&self) -> u64 {
        self.rows_written.load(Ordering::Relaxed)
    }

    /// Current UTC timestamp (exposed so callers can stamp rows consistently).
    pub fn now(&self) -> String {
        now_iso()
    }
}

fn is_multi_statement(sql: &str) -> bool {
    // A trailing semicolon plus another semicolon in the body, or any CREATE.
    let trimmed = sql.trim();
    let semi = trimmed.matches(';').count();
    semi > 1
        || (semi == 1
            && !trimmed.trim_end_matches(';').contains(';')
            && trimmed.to_uppercase().starts_with("CREATE"))
}

fn value_ref_to_json(v: ValueRef<'_>) -> Value {
    match v {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => Value::from(i),
        ValueRef::Real(f) => Value::from(f),
        ValueRef::Text(t) => Value::from(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => Value::Array(b.iter().map(|x| Value::from(*x)).collect()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_business() -> (Business, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("lingmiao-biz-{}", crate::id::short_id("t")));
        std::fs::create_dir_all(&dir).unwrap();
        let b = Business::open_in_dir(&dir).unwrap();
        (b, dir)
    }

    #[test]
    fn ddl_insert_and_query() {
        let (b, dir) = temp_business();
        b.execute(
            "CREATE TABLE projects (code TEXT PRIMARY KEY, name TEXT NOT NULL, size INTEGER)",
        )
        .unwrap();
        let n = b
            .execute("INSERT INTO projects (code, name, size) VALUES ('SRV-001', 'server', 42)")
            .unwrap();
        assert_eq!(n, 1);
        let rows = b.query("SELECT code, name, size FROM projects").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["code"], Value::from("SRV-001"));
        assert_eq!(rows[0]["size"], Value::from(42));
        assert_eq!(b.count("projects").unwrap(), 1);
        assert_eq!(b.tables().unwrap(), vec!["projects".to_string()]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rejects_unsafe_table_names() {
        let (b, dir) = temp_business();
        assert!(b.count("projects; DROP TABLE x").is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}

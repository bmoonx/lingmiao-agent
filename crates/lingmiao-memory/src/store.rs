//! Store plumbing shared by every memory table: zone layout and DB opening.
//!
//! ## Three-zone isolation (ADR A1)
//!
//! The original keeps one set of DBs for the root conversation and one set per
//! *role loop*:
//!
//! ```text
//! .memory/observations.db          ← chat zone (root)
//! .memory/main/observations.db     ← Main role zone
//! .memory/auditor/observations.db  ← Auditor role zone
//! ```
//!
//! [`Zone`] models those three locations; all other modules are zone-agnostic.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use lingmiao_core::LingmiaoError;
use rusqlite::{Connection, OptionalExtension};

/// Store name used in [`LingmiaoError::memory`] context.
pub const STORE: &str = "memory";

/// A memory zone — the root chat store or one of the two role-loop stores.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Zone {
    /// Root conversation (`.memory/`).
    Chat,
    /// Main agent loop (`.memory/main/`).
    Main,
    /// Auditor loop (`.memory/auditor/`).
    Auditor,
}

impl Zone {
    /// Every zone, in a stable order.
    pub const ALL: [Zone; 3] = [Zone::Chat, Zone::Main, Zone::Auditor];

    /// The sub-directory under `memory/`, or `None` for the chat (root) zone.
    pub const fn subdir(self) -> Option<&'static str> {
        match self {
            Zone::Chat => None,
            Zone::Main => Some("main"),
            Zone::Auditor => Some("auditor"),
        }
    }

    /// Stable lowercase name (`chat` / `main` / `auditor`).
    pub const fn name(self) -> &'static str {
        match self {
            Zone::Chat => "chat",
            Zone::Main => "main",
            Zone::Auditor => "auditor",
        }
    }

    /// Directory holding this zone's DBs, given the `memory/` directory.
    pub fn dir(self, memory_dir: &Path) -> PathBuf {
        match self.subdir() {
            Some(sub) => memory_dir.join(sub),
            None => memory_dir.to_path_buf(),
        }
    }

    /// Full path to a named DB inside this zone.
    pub fn db_path(self, memory_dir: &Path, db_name: &str) -> PathBuf {
        self.dir(memory_dir).join(db_name)
    }
}

/// Open a SQLite database in WAL mode with a 10-second busy timeout.
///
/// The busy timeout mirrors the Python `sqlite3.connect(..., timeout=10)` fix
/// for cross-process `database is locked` errors; WAL mirrors the original's
/// `PRAGMA journal_mode=WAL`. Parent directories are created as needed.
pub fn open_db(path: &Path) -> Result<Connection, LingmiaoError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            LingmiaoError::memory(
                STORE,
                format!("create memory dir {}: {e}", parent.display()),
            )
        })?;
    }
    let conn = Connection::open(path)
        .map_err(|e| LingmiaoError::memory(STORE, format!("open {}: {e}", path.display())))?;
    conn.busy_timeout(std::time::Duration::from_secs(10))
        .map_err(|e| LingmiaoError::memory(STORE, format!("set busy_timeout: {e}")))?;
    conn.execute_batch("PRAGMA journal_mode=WAL;")
        .map_err(|e| LingmiaoError::memory(STORE, format!("enable WAL: {e}")))?;
    Ok(conn)
}

/// Open an existing SQLite database **read-only**.
///
/// Used to query *another* project's memory (需求⑤ / cross-project search)
/// without mutating it: no directory creation, no `journal_mode` switch, no
/// `CREATE TABLE` / `ALTER TABLE`. The caller must ensure the file exists;
/// opening a missing file with `SQLITE_OPEN_READ_ONLY` errors rather than
/// silently creating an empty database.
pub fn open_db_readonly(path: &Path) -> Result<Connection, LingmiaoError> {
    let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| {
            LingmiaoError::memory(STORE, format!("open read-only {}: {e}", path.display()))
        })?;
    conn.busy_timeout(std::time::Duration::from_secs(10))
        .map_err(|e| LingmiaoError::memory(STORE, format!("set busy_timeout: {e}")))?;
    Ok(conn)
}

/// Acquire the connection guard, transparently recovering from poisoning.
pub(crate) fn lock(conn: &Mutex<Connection>) -> MutexGuard<'_, Connection> {
    conn.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ── per-store metadata (embedder-space provenance) ─────────────

/// Key under which an embedding store records the [`Embedder::backend`] that
/// wrote its vectors. Read back by the cross-project search to detect a vector
/// -space mismatch: a sibling written by a *different* backend has vectors in a
/// different space, so a cosine against the current query is meaningless.
pub const META_EMBEDDER_BACKEND: &str = "embedder_backend";

/// DDL creating the per-store key/value metadata table. Each embedding store
/// includes it in `init_schema`, so the provenance record lives beside the
/// vectors it describes.
pub const META_TABLE_DDL: &str =
    "CREATE TABLE IF NOT EXISTS store_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);";

/// Record `key=value` in a store's metadata table (writable stores only).
pub fn write_meta(conn: &Connection, key: &str, value: &str) -> Result<(), LingmiaoError> {
    conn.execute(
        "INSERT INTO store_meta (key, value) VALUES (?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        rusqlite::params![key, value],
    )
    .map_err(|e| LingmiaoError::memory(STORE, format!("write store_meta {key}: {e}")))?;
    Ok(())
}

/// Record `key=value` only when `key` is not already present (**first write
/// wins**). Used for vector-space provenance so the value reflects the embedder
/// that *created* the vectors, not the one that merely opened the store last.
pub fn write_meta_if_absent(
    conn: &Connection,
    key: &str,
    value: &str,
) -> Result<(), LingmiaoError> {
    conn.execute(
        "INSERT OR IGNORE INTO store_meta (key, value) VALUES (?1, ?2)",
        rusqlite::params![key, value],
    )
    .map_err(|e| LingmiaoError::memory(STORE, format!("write store_meta {key}: {e}")))?;
    Ok(())
}

/// Read `key` from a store's metadata table, or `None` when the key — or the
/// whole table (a store written before the metadata table existed) — is absent.
/// Safe on a **read-only** connection: it never creates the table, and a missing
/// table degrades to `None` rather than an error.
pub fn read_meta(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row(
        "SELECT value FROM store_meta WHERE key=?1",
        rusqlite::params![key],
        |r| r.get::<_, String>(0),
    )
    .optional()
    .ok()
    .flatten()
}

/// Record an embedder's backend into a store's metadata table, ignoring any
/// failure (provenance is advisory, never a reason to fail a store open).
///
/// **First write wins** ([`write_meta_if_absent`]): the recorded backend stays
/// the one that created the vectors, so a later open with a *different* backend
/// can detect a stale vector space and trigger a one-time rebuild (④真语义).
pub(crate) fn record_embedder_backend(conn: &Connection, backend: Option<&str>) {
    if let Some(backend) = backend {
        let _ = write_meta_if_absent(conn, META_EMBEDDER_BACKEND, backend);
    }
}

/// Row counts across the four stores of a single zone (需求⑤ `zones` view).
///
/// Produced by [`read_zone_counts`], a **read-only** probe that never creates
/// or migrates anything — introspecting a sibling role-loop zone must not have
/// side effects.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ZoneCounts {
    /// `observations` rows.
    pub observations: u64,
    /// `nodes` rows.
    pub knowledge_nodes: u64,
    /// `edges` rows.
    pub knowledge_edges: u64,
    /// `turns` rows.
    pub archive_turns: u64,
    /// User tables in the business DB.
    pub business_tables: u64,
}

/// Read-only row counts for one zone, or `None` when the zone directory does
/// not exist yet (e.g. a role-loop zone that has never run).
///
/// Opens each existing DB with `SQLITE_OPEN_READ_ONLY`; a missing file (or a
/// 0-byte placeholder created by [`crate::Memory`] startup) counts as zero.
pub fn read_zone_counts(memory_dir: &Path, zone: Zone) -> Option<ZoneCounts> {
    let dir = zone.dir(memory_dir);
    if !dir.is_dir() {
        return None;
    }
    let obs = dir.join("observations.db");
    let kg = dir.join("knowledge.db");
    let arc = dir.join("context_record.db");
    let biz = dir.join("business.db");
    Some(ZoneCounts {
        observations: count_rows_if_present(&obs, "observations"),
        knowledge_nodes: count_rows_if_present(&kg, "nodes"),
        knowledge_edges: count_rows_if_present(&kg, "edges"),
        archive_turns: count_rows_if_present(&arc, "turns"),
        business_tables: count_tables_if_present(&biz),
    })
}

/// `SELECT COUNT(*)` against `table`, or 0 when the DB/table is absent.
fn count_rows_if_present(path: &Path, table: &str) -> u64 {
    if !path.is_file() {
        return 0;
    }
    let Ok(conn) = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
    else {
        return 0;
    };
    conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| {
        r.get::<_, i64>(0)
    })
    .map(|n| n.max(0) as u64)
    .unwrap_or(0)
}

/// Count user tables in the business DB, or 0 when it is absent.
fn count_tables_if_present(path: &Path) -> u64 {
    if !path.is_file() {
        return 0;
    }
    let Ok(conn) = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
    else {
        return 0;
    };
    conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
        [],
        |r| r.get::<_, i64>(0),
    )
    .map(|n| n.max(0) as u64)
    .unwrap_or(0)
}

/// One column's runtime definition (需求⑤ `store`/`schema` view).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnInfo {
    /// Column name.
    pub name: String,
    /// Declared SQL type (`TEXT` / `INTEGER` / `BLOB` / …); empty when untyped.
    pub ty: String,
    /// `NOT NULL` constraint present.
    pub not_null: bool,
    /// Part of the primary key.
    pub pk: bool,
}

/// One table's runtime schema, read from the live SQLite catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableSchema {
    /// Table name.
    pub name: String,
    /// Columns in declaration order.
    pub columns: Vec<ColumnInfo>,
    /// Non-implicit index names.
    pub indexes: Vec<String>,
}

/// Read one table's schema from the live connection (`PRAGMA table_info` +
/// `sqlite_master`), or `None` when the table does not exist.
///
/// This is the **runtime source of truth** for the 需求⑤ `store`/`schema` view:
/// it introspects the actual DB, so the platform can never report a hand-copied
/// column list that has drifted from `init_schema`.
pub fn read_table_schema(
    conn: &Connection,
    table: &str,
) -> Result<Option<TableSchema>, LingmiaoError> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(|e| LingmiaoError::memory(STORE, format!("table_info {table}: {e}")))?;
    let rows = stmt
        .query_map([], |r| {
            Ok(ColumnInfo {
                name: r.get::<_, String>(1)?,
                ty: r.get::<_, String>(2)?,
                not_null: r.get::<_, i64>(3)? != 0,
                pk: r.get::<_, i64>(5)? != 0,
            })
        })
        .map_err(|e| LingmiaoError::memory(STORE, format!("table_info {table}: {e}")))?;
    let mut columns = Vec::new();
    for r in rows {
        columns.push(r.map_err(|e| LingmiaoError::memory(STORE, e.to_string()))?);
    }
    if columns.is_empty() {
        return Ok(None);
    }
    let mut stmt = conn
        .prepare(
            "SELECT name FROM sqlite_master \
             WHERE type='index' AND tbl_name=?1 AND name NOT LIKE 'sqlite_%' ORDER BY name",
        )
        .map_err(|e| LingmiaoError::memory(STORE, format!("index_list {table}: {e}")))?;
    let rows = stmt
        .query_map([table], |r| r.get::<_, String>(0))
        .map_err(|e| LingmiaoError::memory(STORE, format!("index_list {table}: {e}")))?;
    let mut indexes = Vec::new();
    for r in rows {
        indexes.push(r.map_err(|e| LingmiaoError::memory(STORE, e.to_string()))?);
    }
    Ok(Some(TableSchema {
        name: table.to_string(),
        columns,
        indexes,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zone_paths_match_the_python_layout() {
        let mem = PathBuf::from("/proj/.memory");
        assert_eq!(
            Zone::Chat.db_path(&mem, "observations.db"),
            PathBuf::from("/proj/.memory/observations.db")
        );
        assert_eq!(
            Zone::Main.db_path(&mem, "observations.db"),
            PathBuf::from("/proj/.memory/main/observations.db")
        );
        assert_eq!(
            Zone::Auditor.db_path(&mem, "knowledge.db"),
            PathBuf::from("/proj/.memory/auditor/knowledge.db")
        );
    }

    #[test]
    fn open_db_creates_parents_and_enables_wal() {
        let dir = std::env::temp_dir().join(format!("lingmiao-mem-{}", std::process::id()));
        let path = dir.join("nested").join("observations.db");
        let _ = std::fs::remove_dir_all(&dir);
        let conn = open_db(&path).expect("open");
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .expect("pragma");
        assert_eq!(mode.to_lowercase(), "wal");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_table_schema_reads_the_live_catalog() {
        let conn = Connection::open_in_memory().expect("open");
        conn.execute_batch(
            "CREATE TABLE demo (id TEXT PRIMARY KEY, n INTEGER NOT NULL, blob BLOB);
             CREATE INDEX idx_demo_n ON demo(n);",
        )
        .expect("ddl");
        let schema = read_table_schema(&conn, "demo").unwrap().expect("present");
        assert_eq!(schema.name, "demo");
        let names: Vec<&str> = schema.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["id", "n", "blob"]);
        assert!(schema.columns[0].pk);
        assert!(schema.columns[1].not_null);
        assert_eq!(schema.columns[1].ty, "INTEGER");
        assert_eq!(schema.indexes, ["idx_demo_n"]);
        // A missing table is reported as None, not an error.
        assert!(read_table_schema(&conn, "nope").unwrap().is_none());
    }

    #[test]
    fn store_meta_roundtrips_and_missing_is_none() {
        let conn = Connection::open_in_memory().expect("open");
        conn.execute_batch(META_TABLE_DDL).expect("ddl");
        // Absent key → None, not an error.
        assert_eq!(read_meta(&conn, META_EMBEDDER_BACKEND), None);
        write_meta(&conn, META_EMBEDDER_BACKEND, "hashing").unwrap();
        assert_eq!(
            read_meta(&conn, META_EMBEDDER_BACKEND).as_deref(),
            Some("hashing")
        );
        // Upsert overwrites the previous value.
        write_meta(&conn, META_EMBEDDER_BACKEND, "fastembed/all-MiniLM-L6-v2").unwrap();
        assert_eq!(
            read_meta(&conn, META_EMBEDDER_BACKEND).as_deref(),
            Some("fastembed/all-MiniLM-L6-v2")
        );
        // A legacy DB without the table at all → None, never an error/panic.
        let bare = Connection::open_in_memory().expect("open");
        assert_eq!(read_meta(&bare, META_EMBEDDER_BACKEND), None);
    }
}

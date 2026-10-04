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
use std::time::{Duration, Instant};

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

/// How long a lock wait may last before we give up (原 `busy_timeout(10s)`).
///
/// The value mirrors the Python `sqlite3.connect(..., timeout=10)` fix for
/// cross-process `database is locked` errors; WAL mirrors the original's
/// `PRAGMA journal_mode=WAL`.
pub const BUSY_TIMEOUT: Duration = Duration::from_secs(10);

/// How often the lock wait samples its state (F 项: 「每 5 秒查询一次状态」).
const BUSY_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Sleep slice inside the busy handler — small enough that the 5s sampling
/// boundary is honoured promptly, and that a released lock is picked up fast.
const BUSY_SLICE: Duration = Duration::from_millis(50);

std::thread_local! {
    /// State of the lock wait in progress on this thread: what is being written
    /// (for the report), when it started, and how many 5s samples were taken.
    ///
    /// `thread_local!` rather than a struct field because SQLite's handler is a
    /// bare `fn` pointer with no captured state — and because one connection is
    /// used from one thread at a time, which is exactly the lifetime of a wait.
    static BUSY_WAIT: std::cell::RefCell<Option<BusyWait>> = const { std::cell::RefCell::new(None) };
    /// Label describing what this thread is writing (set by [`open_db`]).
    static BUSY_LABEL: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
}

struct BusyWait {
    /// When this wait began (first callback of the current wait).
    started: Instant,
    /// When the previous callback ran — used to recognise the *next* wait.
    ///
    /// SQLite gives no "the lock is free now" callback, so the only reliable
    /// signal that a new wait began is a gap between callbacks. Without this, a
    /// fresh wait would inherit the previous one's `started` and give up at once.
    last: Instant,
    samples: u32,
}

/// The lock-wait loop handed to SQLite as its busy handler (F 项 ⑧).
///
/// Behaviourally the old `busy_timeout(10s)` — it retries for up to
/// [`BUSY_TIMEOUT`] and then lets SQLite return `SQLITE_BUSY` — but it is now a
/// **poll**: every [`BUSY_POLL_INTERVAL`] it samples the wait and publishes the
/// state ([`lingmiao_core::polling::publish_sync_wait`]), so a lock wait is no
/// longer invisible to the rest of the turn.
///
/// A model cannot be consulted *here*: this is a synchronous callback inside
/// SQLite (no `await`, no capture). The ruling is therefore made by code — the
/// documented exception, alongside ① — while the published state is folded into
/// what the surrounding stage's judge is told, so the decision that *is* made by
/// a model can see that the database was the thing blocking.
fn busy_poll(_attempt: i32) -> bool {
    let now = Instant::now();
    let previous = BUSY_WAIT.with(|s| s.borrow_mut().take());
    let mut w = match previous {
        // Continuing the same wait (the callbacks are back-to-back)…
        Some(w) if now.duration_since(w.last) < BUSY_SLICE * 4 => w,
        // …or a *new* wait, which starts its own clock.
        _ => BusyWait {
            started: now,
            last: now,
            samples: 0,
        },
    };
    w.last = now;
    // One bounded slice per callback: SQLite re-enters us until we return false.
    std::thread::sleep(BUSY_SLICE);
    let elapsed = w.started.elapsed();
    let due = BUSY_POLL_INTERVAL * (w.samples + 1);
    if elapsed >= due {
        w.samples += 1;
        let label = BUSY_LABEL.with(|l| l.borrow().clone());
        lingmiao_core::polling::publish_sync_wait(format!("sqlite 锁等待（{label}）"), w.started);
        tracing::debug!(
            waited_ms = elapsed.as_millis() as u64,
            "memory: still waiting for the write lock"
        );
    }
    if elapsed >= BUSY_TIMEOUT {
        // Give up: report honestly (the caller surfaces `database is locked`).
        lingmiao_core::polling::end_sync_wait();
        return false;
    }
    // Also clear the published slot on completion of a *short* wait: the next
    // callback either continues (refreshing it) or, being far away in time,
    // starts a fresh wait that republishes.
    if elapsed < BUSY_POLL_INTERVAL {
        lingmiao_core::polling::end_sync_wait();
    }
    BUSY_WAIT.with(|s| *s.borrow_mut() = Some(w));
    true
}

/// Memory-map budget handed to SQLite on every connection (`PRAGMA mmap_size`).
///
/// Why this exists: the archive DB (`context_record.db`) stores each turn's
/// **whole message list** — ~208 KB/row, 105 MB for a few hundred turns — and
/// `.memory/` lives on `/mnt/c`, a 9p (drvfs) mount. Reading that file through
/// 9p's syscall path costs ~3.5 s per full scan, which is what made the archive
/// half of `search_memory` the dominant cost of every recall (2026-10-02).
///
/// Measured on a read-only copy of the live archive (275 rows / 105 MB), same
/// `SELECT * FROM turns WHERE embedding IS NOT NULL`:
///
/// | filesystem              | no mmap | mmap_size=256 MiB |
/// |-------------------------|---------|-------------------|
/// | `/tmp` (native ext4)    | 0.20 s  | —                 |
/// | `/mnt/c` (9p/drvfs)     | 3.51 s  | **0.16 s**        |
///
/// i.e. **~22× faster and back to native-filesystem speed**. A partial index
/// (`ON turns(id) WHERE embedding IS NOT NULL`) was also measured on the same
/// copy and only reached 3.66 s → 3.17 s (1.2×) — the bottleneck is the file
/// I/O path, not the table scan, so `mmap` is the correct fix. `cache_size`
/// was likewise ineffective (3.38 s).
///
/// 256 MiB covers today's 105 MB archive with headroom; SQLite caps the mapping
/// at the file size, and an unsupported filesystem silently reports 0 (the
/// pragma never errors), so this is safe everywhere.
pub const MMAP_SIZE_BYTES: i64 = 256 * 1024 * 1024;

/// Apply the connection-level pragmas every store shares (mmap + WAL).
///
/// Split out from [`open_db`] so the read-only opener can share the exact same
/// tuning — a memory-mapped read is the whole point of [`MMAP_SIZE_BYTES`].
fn tune_connection(conn: &Connection, writable: bool) -> Result<(), LingmiaoError> {
    // M2: fit the archive's large rows into a memory map instead of streaming
    // them through the 9p read path (see [`MMAP_SIZE_BYTES`]).
    conn.execute_batch(&format!("PRAGMA mmap_size={MMAP_SIZE_BYTES};"))
        .map_err(|e| LingmiaoError::memory(STORE, format!("set mmap_size: {e}")))?;
    if writable {
        conn.execute_batch("PRAGMA journal_mode=WAL;")
            .map_err(|e| LingmiaoError::memory(STORE, format!("enable WAL: {e}")))?;
    }
    Ok(())
}

/// Open a SQLite database in WAL mode with the polled lock wait.
///
/// Parent directories are created as needed.
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
    // F 项 ⑧: replace the opaque `busy_timeout` with the polled handler above —
    // same 10s ceiling, but the wait now samples and reports itself every 5s.
    let label = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    BUSY_LABEL.with(|l| *l.borrow_mut() = label);
    conn.busy_handler(Some(busy_poll))
        .map_err(|e| LingmiaoError::memory(STORE, format!("set busy handler: {e}")))?;
    tune_connection(&conn, true)?;
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
    // Same polled lock wait as the writable path (F 项 ⑧) — a read can block on a
    // writer's WAL checkpoint too, and it should be just as visible.
    let label = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    BUSY_LABEL.with(|l| *l.borrow_mut() = label);
    conn.busy_handler(Some(busy_poll))
        .map_err(|e| LingmiaoError::memory(STORE, format!("set busy handler: {e}")))?;
    // Read-only stores share the mmap tuning (no WAL switch — that is a write).
    tune_connection(&conn, false)?;
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
    fn both_openers_apply_the_mmap_budget() {
        // M2 (2026-10-02): the archive's big rows are read through a memory map
        // instead of the 9p syscall path. Both openers must apply it — a
        // read-only cross-project probe reads the same heavy tables.
        let dir = std::env::temp_dir().join(format!("lingmiao-mmap-{}", std::process::id()));
        let path = dir.join("context_record.db");
        let _ = std::fs::remove_dir_all(&dir);
        {
            let conn = open_db(&path).expect("open");
            let n: i64 = conn
                .query_row("PRAGMA mmap_size", [], |r| r.get(0))
                .expect("pragma");
            assert_eq!(n, MMAP_SIZE_BYTES, "writable opener maps the file");
        }
        let ro = open_db_readonly(&path).expect("open read-only");
        let n: i64 = ro
            .query_row("PRAGMA mmap_size", [], |r| r.get(0))
            .expect("pragma");
        assert_eq!(n, MMAP_SIZE_BYTES, "read-only opener maps the file too");
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
        // Upsert overwrites the previous value (any backend id round-trips).
        write_meta(&conn, META_EMBEDDER_BACKEND, crate::embed::EMBEDDER_BACKEND).unwrap();
        assert_eq!(
            read_meta(&conn, META_EMBEDDER_BACKEND).as_deref(),
            Some(crate::embed::EMBEDDER_BACKEND)
        );
        // A legacy DB without the table at all → None, never an error/panic.
        let bare = Connection::open_in_memory().expect("open");
        assert_eq!(read_meta(&bare, META_EMBEDDER_BACKEND), None);
    }
}

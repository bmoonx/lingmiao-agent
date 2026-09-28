//! Path resolution — ported from `core/paths.py`, reworked by 需求③.
//!
//! Claude-Code-style layout: the current working directory is the project root.
//! Two sibling directories sit under it (需求③ — the previous single state dir
//! mixed durable assets with volatile junk):
//!
//! * [`crate::brand::MEMORY_DIR`] (`.memory/`) — **durable** memory assets
//!   (the whitelisted SQLite DBs + `constraints/` / `trajectories/` /
//!   `embeddings/` / `whiteboard/`). Backed up / carried with the project.
//! * [`crate::brand::CACHE_DIR`] (`.cache/lingmiao/`) — **volatile** runtime
//!   artifacts (`logs/` / `tmp/` / `loop/`). Safe to delete.
//!
//! The AI file tools operate on the project files directly.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::brand;

/// Name of the per-project memory directory (see [`crate::brand::MEMORY_DIR`]).
pub const MEMORY_DIRNAME: &str = brand::MEMORY_DIR;

/// The seven SQLite store file names kept in the memory directory.
///
/// Anything else found at the memory-dir **top level** is removed on startup
/// (see [`Paths::cleanup_memory_dir`]), matching `_ALLOWED_DBS` in the original.
/// Sub-directories (`constraints/` / `trajectories/` / `embeddings/` /
/// `whiteboard/`) are left alone.
pub const ALLOWED_DBS: [&str; 7] = [
    "context_record.db",
    "observations.db",
    "knowledge.db",
    "business.db",
    "schedules.db",
    "whiteboard.db",
    "autonomous.db",
];

/// Allowed suffixes for SQLite journal files.
const ALLOW_SUFFIXES: [&str; 4] = ["", "-wal", "-shm", "-journal"];

/// True when `name` is a whitelisted DB file or one of its journals.
pub fn is_allowed_db(name: &str) -> bool {
    ALLOWED_DBS.iter().any(|db| {
        ALLOW_SUFFIXES
            .iter()
            .any(|suffix| name == format!("{db}{suffix}"))
    })
}

/// Resolved paths for the current project (需求③ layout).
#[derive(Debug, Clone)]
pub struct Paths {
    /// Project root — the process working directory.
    pub root: PathBuf,
    /// Durable memory directory (`<root>/.memory`).
    pub memory_dir: PathBuf,
    /// Volatile cache root (`<root>/.cache/lingmiao`).
    pub cache_dir: PathBuf,
    /// Scratch space (`cache_dir/tmp`).
    pub tmp_dir: PathBuf,
    /// Log output directory (`cache_dir/logs`).
    pub logs_dir: PathBuf,
    /// Loop state directory (`cache_dir/loop`).
    pub loop_dir: PathBuf,
}

impl Paths {
    /// Build the layout rooted at `root`.
    pub fn at(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let memory_dir = root.join(brand::MEMORY_DIR);
        let cache_dir = root.join(brand::CACHE_DIR).join(brand::BIN);
        Self {
            tmp_dir: cache_dir.join("tmp"),
            logs_dir: cache_dir.join("logs"),
            loop_dir: cache_dir.join("loop"),
            memory_dir,
            cache_dir,
            root,
        }
    }

    /// Detect the layout from the process working directory.
    pub fn detect() -> io::Result<Self> {
        Ok(Self::at(std::env::current_dir()?))
    }

    /// Path of a memory DB by its bare file name (e.g. `observations.db`).
    pub fn db(&self, name: &str) -> PathBuf {
        self.memory_dir.join(name)
    }

    /// Create the directory tree if missing.
    pub fn ensure_dirs(&self) -> io::Result<()> {
        for dir in [
            &self.memory_dir,
            &self.cache_dir,
            &self.tmp_dir,
            &self.logs_dir,
            &self.loop_dir,
        ] {
            fs::create_dir_all(dir)?;
        }
        Ok(())
    }

    /// Remove any **file** at the memory-dir top level that is not a whitelisted
    /// DB or its journal.
    ///
    /// Returns the names of the files that were removed. Sub-directories
    /// (`constraints/` / `trajectories/` / `embeddings/` / `whiteboard/`) are
    /// skipped, not recursed into.
    pub fn cleanup_memory_dir(&self) -> io::Result<Vec<String>> {
        let mut removed = Vec::new();
        if !self.memory_dir.exists() {
            return Ok(removed);
        }
        for entry in fs::read_dir(&self.memory_dir)? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if !is_allowed_db(&name) {
                fs::remove_file(entry.path())?;
                removed.push(name);
            }
        }
        Ok(removed)
    }

    /// Create 0-byte DB files for the four primary stores if missing.
    ///
    /// The original used `sqlite3.connect(...)` purely to touch the file, which
    /// also produces a 0-byte file until the first write — behaviour preserved.
    pub fn init_empty_dbs(&self) -> io::Result<()> {
        for name in [
            "context_record.db",
            "observations.db",
            "knowledge.db",
            "business.db",
        ] {
            let path = self.db(name);
            if !path.exists() {
                fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)?;
            }
        }
        Ok(())
    }

    /// Full startup preparation: directories, cleanup, empty DBs.
    pub fn prepare(&self) -> io::Result<()> {
        self.ensure_dirs()?;
        self.cleanup_memory_dir()?;
        self.init_empty_dbs()?;
        Ok(())
    }
}

/// A set of whitelisted DB names (helper for tests / diagnostics).
pub fn allowed_db_set() -> BTreeSet<&'static str> {
    ALLOWED_DBS.into_iter().collect()
}

static GLOBAL: OnceLock<Paths> = OnceLock::new();

/// Process-wide paths, detected once. Falls back to `.` on error.
pub fn global() -> &'static Paths {
    GLOBAL.get_or_init(|| Paths::detect().unwrap_or_else(|_| Paths::at(Path::new("."))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_splits_memory_from_cache() {
        let p = Paths::at("/tmp/proj");
        assert_eq!(p.memory_dir, PathBuf::from("/tmp/proj/.memory"));
        assert_eq!(p.cache_dir, PathBuf::from("/tmp/proj/.cache/lingmiao"));
        assert_eq!(p.tmp_dir, PathBuf::from("/tmp/proj/.cache/lingmiao/tmp"));
        assert_eq!(p.logs_dir, PathBuf::from("/tmp/proj/.cache/lingmiao/logs"));
        assert_eq!(p.loop_dir, PathBuf::from("/tmp/proj/.cache/lingmiao/loop"));
        assert_eq!(
            p.db("observations.db"),
            PathBuf::from("/tmp/proj/.memory/observations.db")
        );
    }

    #[test]
    fn whitelist_matches_dbs_and_journals() {
        assert!(is_allowed_db("observations.db"));
        assert!(is_allowed_db("observations.db-wal"));
        assert!(is_allowed_db("knowledge.db-journal"));
        assert!(!is_allowed_db("secrets.txt"));
        assert!(!is_allowed_db("observations.db.bak"));
    }

    #[test]
    fn cleanup_removes_stray_files_but_keeps_subdirs() {
        let root = std::env::temp_dir().join(format!("lingmiao-paths-{}", std::process::id()));
        let p = Paths::at(&root);
        p.ensure_dirs().unwrap();
        std::fs::write(p.memory_dir.join("stray.txt"), "x").unwrap();
        std::fs::create_dir_all(p.memory_dir.join("trajectories")).unwrap();
        std::fs::write(p.memory_dir.join("trajectories/t.jsonl"), "{}").unwrap();
        let removed = p.cleanup_memory_dir().unwrap();
        assert!(removed.contains(&"stray.txt".to_string()));
        assert!(p.memory_dir.join("trajectories/t.jsonl").exists());
        std::fs::remove_dir_all(&root).ok();
    }
}

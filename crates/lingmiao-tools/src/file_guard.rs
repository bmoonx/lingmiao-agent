//! Path sandbox + read-before-write guard (rules.md work style, Q6 `file_guard`).
//!
//! [`FileGuard`] does two jobs:
//!
//! 1. **Path resolution** — relative paths resolve against the project root,
//!    absolute paths are accepted, but every path is confined to the root (no
//!    `..` escape) and `.memory/` is reserved for the memory layer (需求③).
//! 2. **Stale-write protection** — a file must be read before it can be
//!    overwritten, and its content fingerprint is re-checked at write time so a
//!    write fails if the file changed since the last read.
//!
//! [`GuardedWrite`] is the reusable wrapper tools use for mutations.

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

use lingmiao_core::brand;

use crate::tool::ToolError;

/// Stable content fingerprint (a hash, never the bytes themselves).
pub fn fingerprint(content: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    content.hash(&mut hasher);
    hasher.finish()
}

/// Lexically normalise a path — collapse `.` and `..` without touching the
/// filesystem (so non-existent files, e.g. new writes, resolve cleanly).
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Path sandbox + read tracking for one workspace root.
pub struct FileGuard {
    root: PathBuf,
    memory_dir: PathBuf,
    read: Mutex<HashMap<PathBuf, u64>>,
}

impl FileGuard {
    /// Create a guard rooted at `root` (canonicalised when possible).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let root = fs::canonicalize(&root).unwrap_or(root);
        let memory_dir = root.join(brand::MEMORY_DIR);
        Self {
            root,
            memory_dir,
            read: Mutex::new(HashMap::new()),
        }
    }

    /// The sandbox root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolve a user path, enforcing the sandbox and memory-dir rules.
    pub fn resolve(&self, raw: &str) -> Result<PathBuf, ToolError> {
        if raw.trim().is_empty() {
            return Err(ToolError::PathNotAllowed {
                path: raw.to_string(),
                reason: "empty path".into(),
            });
        }
        let candidate = Path::new(raw);
        let joined = if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            self.root.join(candidate)
        };
        let normalized = normalize(&joined);
        if !normalized.starts_with(&self.root) {
            return Err(ToolError::PathNotAllowed {
                path: raw.to_string(),
                reason: "outside the project root (no `..` escape)".into(),
            });
        }
        if normalized.starts_with(&self.memory_dir) {
            return Err(ToolError::PathNotAllowed {
                path: raw.to_string(),
                reason: "`.memory/` is reserved for the memory layer".into(),
            });
        }
        Ok(normalized)
    }

    /// Project-relative display path (falls back to absolute).
    pub fn relative(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| path.display().to_string())
    }

    /// Record that `path` was read, storing its content fingerprint.
    pub fn record_read(&self, path: &Path, content: &str) {
        if let Ok(mut map) = self.read.lock() {
            map.insert(path.to_path_buf(), fingerprint(content));
        }
    }

    /// Whether `path` has been read in this session.
    pub fn was_read(&self, path: &Path) -> bool {
        self.read
            .lock()
            .map(|map| map.contains_key(path))
            .unwrap_or(false)
    }

    /// Ensure `path` was read before a mutation.
    pub fn require_read(&self, path: &Path) -> Result<(), ToolError> {
        if self.was_read(path) {
            Ok(())
        } else {
            Err(ToolError::ReadRequired(self.relative(path)))
        }
    }

    /// Fail if `path`'s current content differs from the fingerprint at read.
    pub fn check_fresh(&self, path: &Path, current: &str) -> Result<(), ToolError> {
        let snapshot = self.read.lock().ok().and_then(|m| m.get(path).copied());
        if let Some(previous) = snapshot {
            if previous != fingerprint(current) {
                return Err(ToolError::Stale(self.relative(path)));
            }
        }
        Ok(())
    }

    /// Write guard bound to this sandbox.
    pub fn write(&self) -> GuardedWrite<'_> {
        GuardedWrite::new(self)
    }
}

/// Reusable write wrapper: enforce read-before-write + stale detection.
pub struct GuardedWrite<'a> {
    guard: &'a FileGuard,
}

impl<'a> GuardedWrite<'a> {
    /// Bind to a guard.
    pub fn new(guard: &'a FileGuard) -> Self {
        Self { guard }
    }

    /// Write `content` to `raw_path` (creating parent dirs), enforcing the
    /// read-before-write and freshness rules for existing files.
    pub fn write(&self, raw_path: &str, content: &str) -> Result<PathBuf, ToolError> {
        let path = self.guard.resolve(raw_path)?;
        if path.exists() {
            self.guard.require_read(&path)?;
            let current = fs::read_to_string(&path)
                .map_err(|e| ToolError::io(self.guard.relative(&path), e))?;
            self.guard.check_fresh(&path, &current)?;
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| ToolError::io(self.guard.relative(parent), e))?;
        }
        fs::write(&path, content).map_err(|e| ToolError::io(self.guard.relative(&path), e))?;
        self.guard.record_read(&path, content);
        Ok(path)
    }

    /// Delete `raw_path`, enforcing that it was read first.
    pub fn delete(&self, raw_path: &str) -> Result<PathBuf, ToolError> {
        let path = self.guard.resolve(raw_path)?;
        self.guard.require_read(&path)?;
        fs::remove_file(&path).map_err(|e| ToolError::io(self.guard.relative(&path), e))?;
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lingmiao-guard-{tag}-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::canonicalize(&dir).unwrap()
    }

    #[test]
    fn rejects_parent_escape_and_memory_dir() {
        let root = temp_root("resolve");
        let guard = FileGuard::new(&root);
        assert!(guard.resolve("../../etc/passwd").is_err());
        assert!(guard.resolve(".memory/observations.db").is_err());
        assert!(guard.resolve("src/main.rs").is_ok());
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn guarded_write_requires_prior_read() {
        let root = temp_root("readbfr");
        fs::write(root.join("a.txt"), "old").unwrap();
        let guard = FileGuard::new(&root);
        // Unread existing file → refused.
        assert!(matches!(
            guard.write().write("a.txt", "new"),
            Err(ToolError::ReadRequired(_))
        ));
        // Read then write succeeds.
        let cur = fs::read_to_string(root.join("a.txt")).unwrap();
        guard.record_read(&root.join("a.txt"), &cur);
        guard.write().write("a.txt", "new").unwrap();
        assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "new");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn guarded_write_detects_stale_content() {
        let root = temp_root("stale");
        let file = root.join("a.txt");
        fs::write(&file, "v1").unwrap();
        let guard = FileGuard::new(&root);
        guard.record_read(&file, "v1");
        // External modification after the read.
        fs::write(&file, "v2").unwrap();
        assert!(matches!(
            guard.write().write("a.txt", "v3"),
            Err(ToolError::Stale(_))
        ));
        fs::remove_dir_all(&root).ok();
    }
}

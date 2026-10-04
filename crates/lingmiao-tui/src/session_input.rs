//! Un-submitted input **snapshot** — the queue + input-box draft, persisted so a
//! `kill -9` no longer destroys them (cli 2026-09-28「刚才队列里的提示词还能看到吗？
//! 进程突然被杀死了」).
//!
//! The queue ([`crate::app::App`]'s `queue`) and the live draft live only in
//! memory: mid-turn Enter pushes to a `Vec<String>`, and nothing else in the
//! crate ever writes them anywhere (the only persistent user text is
//! `turns.user_msg`, written **after** a turn completes — so anything still
//! queued when the process dies leaves zero trace on disk). A `SIGKILL` (or a
//! crash, or a `kill -9` aimed at the wrong pid) therefore loses every prompt the
//! user had lined up.
//!
//! The fix is deliberately small: snapshot the two values into the **volatile**
//! cache dir (`.cache/lingmiao/tmp/session-input.json`, per rules.md §3 —
//! deletable junk, not a durable memory asset), and read it back on startup. It
//! is *not* a session store: no transcript, no history, no ids — just "what the
//! user had typed but not yet submitted".
//!
//! Writes are atomic (temp file + `rename`) so a kill *during* the write cannot
//! leave a half-written file that fails to parse on the next start.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// File name inside `.cache/lingmiao/tmp/`.
pub const FILE_NAME: &str = "session-input.json";

/// The persisted shape: what was queued behind the running turn, and what was in
/// the input box.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInput {
    /// Messages queued mid-turn, in submission order.
    #[serde(default)]
    pub queue: Vec<String>,
    /// The input-box text that had not been submitted yet.
    #[serde(default)]
    pub draft: String,
}

impl SessionInput {
    /// Whether there is nothing worth persisting (an empty snapshot is written as
    /// a delete instead — see [`save`]).
    pub fn is_empty(&self) -> bool {
        self.queue.iter().all(|q| q.trim().is_empty()) && self.draft.trim().is_empty()
    }
}

/// Path of the snapshot inside a cache `tmp/` directory.
pub fn path(tmp_dir: &Path) -> PathBuf {
    tmp_dir.join(FILE_NAME)
}

/// Persist `snapshot` (atomically). An empty snapshot **removes** the file: there
/// is nothing to recover, and a stale file would resurrect a long-since-submitted
/// draft on the next start.
///
/// Best-effort: the caller has no useful reaction to a failure here beyond
/// logging, and losing this snapshot must never break a turn.
pub fn save(tmp_dir: &Path, snapshot: &SessionInput) -> std::io::Result<()> {
    let file = path(tmp_dir);
    if snapshot.is_empty() {
        return match std::fs::remove_file(&file) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        };
    }
    std::fs::create_dir_all(tmp_dir)?;
    let text = serde_json::to_string(snapshot).unwrap_or_else(|_| "{}".to_string());
    // Temp-then-rename: an interrupted write (the very scenario this module
    // exists for) leaves either the old file or a stray `.tmp`, never a corrupt
    // `session-input.json`.
    let tmp = tmp_dir.join(format!("{FILE_NAME}.tmp"));
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, &file)
}

/// Read the snapshot back, or `None` when there is nothing (missing / unreadable
/// / malformed).
pub fn load(tmp_dir: &Path) -> Option<SessionInput> {
    let text = std::fs::read_to_string(path(tmp_dir)).ok()?;
    let snapshot: SessionInput = serde_json::from_str(&text).ok()?;
    if snapshot.is_empty() {
        return None;
    }
    Some(snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each test gets its own directory so they cannot race (no `tempfile` dep in
    /// this crate).
    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lingmiao-session-input-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn round_trips_queue_and_draft() {
        let dir = scratch("roundtrip");
        let s = SessionInput {
            queue: vec!["第一条".into(), "第二条".into()],
            draft: "半句话".into(),
        };
        save(&dir, &s).unwrap();
        assert_eq!(load(&dir), Some(s));
        // A snapshot file is plain JSON under tmp/ (a human can read it after a
        // crash, which is the point of the feature).
        let raw = std::fs::read_to_string(path(&dir)).unwrap();
        assert!(raw.contains("第一条"), "raw snapshot: {raw}");
    }

    #[test]
    fn an_empty_snapshot_removes_the_file() {
        let dir = scratch("empty");
        save(
            &dir,
            &SessionInput {
                queue: vec!["x".into()],
                draft: String::new(),
            },
        )
        .unwrap();
        assert!(path(&dir).exists());
        // Whitespace-only input counts as empty: it is not something the user
        // typed and would only be noise on the next start.
        save(
            &dir,
            &SessionInput {
                queue: Vec::new(),
                draft: "   \n".into(),
            },
        )
        .unwrap();
        assert!(!path(&dir).exists());
        assert_eq!(load(&dir), None);
    }

    #[test]
    fn load_tolerates_missing_and_corrupt_files() {
        let dir = scratch("corrupt");
        assert_eq!(load(&dir), None);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(path(&dir), "{ not json").unwrap();
        assert_eq!(load(&dir), None);
    }

    #[test]
    fn save_leaves_no_temp_file_behind() {
        let dir = scratch("atomic");
        save(
            &dir,
            &SessionInput {
                queue: Vec::new(),
                draft: "d".into(),
            },
        )
        .unwrap();
        let leftovers: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp files left: {leftovers:?}");
    }
}

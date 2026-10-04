//! Embedded ripgrep — `grep` / `glob` shell out to `rg` (CC parity).
//!
//! Claude Code's `GrepTool` / `GlobTool` do **not** walk the tree in-process:
//! they spawn a (vendored) ripgrep child process and read its stdout. That is
//! why their searches are fast, abortable, `.gitignore`-aware and never block
//! the event loop. 灵妙 does the same here:
//!
//! * a small static `rg` binary is **embedded** in the crate
//!   (`vendor/rg/<target>/rg`, bundled, no external install needed);
//! * on first use it is extracted to the per-user cache (`~/.cache/lingmiao/bin`)
//!   and reused afterwards (size-checked, atomic replace);
//! * a system `rg` on `PATH` is the fallback when no binary is vendored for the
//!   target (e.g. a platform we don't ship a build for);
//! * when neither is available, callers fall back to their in-process search —
//!   a search never hard-fails just because `rg` is missing.
//!
//! Override the binary with `LINGMIAO_RG=/path/to/rg`.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

/// Version of the bundled ripgrep (`vendor/rg/<target>/rg`).
pub const BUNDLED_RG_VERSION: &str = "14.1.1";

/// The embedded ripgrep binary for the current target. Empty when the target is
/// not one we vendor — callers then fall back to a system `rg` / in-process.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const EMBEDDED_RG: &[u8] = include_bytes!("../vendor/rg/x86_64-unknown-linux-musl/rg");
#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
const EMBEDDED_RG: &[u8] = &[];

/// The result of one ripgrep invocation.
#[derive(Debug, Default)]
pub struct RgRun {
    /// stdout split into lines (trailing `\r` stripped, empties dropped).
    pub lines: Vec<String>,
    /// The process was killed after the site's own limit. `lines` is then empty:
    /// a timeout is reported distinctly from "no matches".
    pub timed_out: bool,
    /// F 项: the wait judge cut the search short (a silent, stalled `rg`). Carries
    /// the one-line reason so the tool can report honestly instead of pretending
    /// the search completed with no matches.
    pub aborted: Option<String>,
}

/// Run `rg` with `args` searching `target`, under the unified wait poll (F 项).
///
/// `Ok` covers both "matches found" (exit 0) and "no matches" (exit 1). A
/// timeout yields `Ok(RgRun { timed_out: true, .. })` (distinct from "no
/// matches"); a spawn failure or a ripgrep usage error yields `Err` so the
/// caller can fall back to its in-process search.
///
/// `limit` keeps its old meaning when no wait judge is in scope (unit tests,
/// library users) — it *is* `tokio::time::timeout(limit, cmd.output())`, byte for
/// byte. Inside a judged turn it becomes the silence threshold, and a search that
/// is producing output is never questioned however long it runs.
pub async fn run(args: &[String], target: &Path, limit: Duration) -> Result<RgRun, String> {
    let Some(bin) = rg_binary() else {
        return Err("ripgrep binary not found".to_string());
    };
    let mut cmd = tokio::process::Command::new(bin);
    cmd.args(args)
        .arg(target)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    crate::wait_exec::isolate_process_group(&mut cmd);
    let out = crate::wait_exec::run_polled(
        lingmiao_core::polling::WaitClass::Command,
        format!("rg {}", args.last().cloned().unwrap_or_default()),
        limit,
        crate::wait_exec::DEFAULT_MAX_CAPTURE,
        &mut cmd,
    )
    .await
    .map_err(|e| format!("failed to run rg: {e}"))?;

    if let Some(aborted) = out.aborted {
        return Ok(RgRun {
            lines: Vec::new(),
            timed_out: false,
            aborted: Some(aborted.message()),
        });
    }
    if out.timed_out {
        return Ok(RgRun {
            lines: Vec::new(),
            timed_out: true,
            aborted: None,
        });
    }
    let code = out.status.and_then(|s| s.code());
    // 0 = matches, 1 = no matches — both are success.
    if matches!(code, Some(0) | Some(1)) {
        Ok(RgRun {
            lines: parse_lines(&out.stdout),
            timed_out: false,
            aborted: None,
        })
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr);
        Err(format!(
            "rg exited with code {}: {}",
            code.map(|c| c.to_string())
                .unwrap_or_else(|| "signal".into()),
            stderr.trim()
        ))
    }
}
/// Resolve (once) the `rg` binary to use: the env override, then the embedded
/// binary (extracted to cache), then a system `rg` on `PATH`.
pub fn rg_binary() -> Option<&'static Path> {
    static CACHE: OnceLock<Option<PathBuf>> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            if let Some(p) = env_override() {
                return Some(p);
            }
            if let Some(p) = extract_embedded() {
                return Some(p);
            }
            find_on_path("rg")
        })
        .as_deref()
}

/// `LINGMIAO_RG` — an explicit, user-supplied ripgrep path (wins when it exists).
fn env_override() -> Option<PathBuf> {
    let raw = std::env::var_os(lingmiao_core::brand::env("RG"))?;
    let p = PathBuf::from(raw);
    if p.is_file() {
        tracing::debug!(
            "ripgrep: using {} override",
            lingmiao_core::brand::env("RG")
        );
        Some(p)
    } else {
        tracing::warn!(
            "ripgrep: {} points at a missing file; ignoring",
            lingmiao_core::brand::env("RG")
        );
        None
    }
}

/// Extract the embedded binary to `<home cache>/bin/rg-<version>` (once) and
/// return its path. Returns `None` when nothing is embedded for this target.
fn extract_embedded() -> Option<PathBuf> {
    if EMBEDDED_RG.is_empty() {
        return None;
    }
    let dir = cache_bin_dir()?;
    let dest = dir.join(format!("rg-{BUNDLED_RG_VERSION}"));
    // Reuse an already-extracted copy of the same size (a re-extract only when
    // the bundled version/size changed).
    if let Ok(meta) = std::fs::metadata(&dest) {
        if meta.len() == EMBEDDED_RG.len() as u64 {
            return Some(dest);
        }
    }
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!("ripgrep: cannot create {}: {e}", dir.display());
        return None;
    }
    // Write to a unique temp file then atomically rename — concurrent processes
    // extracting at once cannot corrupt each other.
    let tmp = dir.join(format!(
        "rg-{BUNDLED_RG_VERSION}.{}.tmp",
        std::process::id()
    ));
    if let Err(e) = std::fs::write(&tmp, EMBEDDED_RG) {
        tracing::warn!("ripgrep: cannot write {}: {e}", tmp.display());
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755));
    }
    if let Err(e) = std::fs::rename(&tmp, &dest) {
        let _ = std::fs::remove_file(&tmp);
        tracing::warn!("ripgrep: cannot install {}: {e}", dest.display());
        return None;
    }
    tracing::debug!(
        "ripgrep: extracted bundled {BUNDLED_RG_VERSION} to {}",
        dest.display()
    );
    Some(dest)
}

/// Directory holding the extracted ripgrep: `$HOME/.cache/lingmiao/bin`.
fn cache_bin_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(
        PathBuf::from(home)
            .join(".cache")
            .join(lingmiao_core::brand::BIN)
            .join("bin"),
    )
}

/// Locate an executable by name on `PATH` (no external crate).
fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let cand = dir.join(name);
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

/// Split captured stdout into trimmed lines.
fn parse_lines(bytes: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(|l| l.strip_suffix('\r').unwrap_or(l).to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_binary_is_present_and_runs() {
        // On the vendored target the binary must resolve and print its version.
        if EMBEDDED_RG.is_empty() {
            return; // not a vendored target — nothing to assert
        }
        let bin = rg_binary().expect("embedded rg must resolve");
        let out = std::process::Command::new(bin)
            .arg("--version")
            .output()
            .expect("run rg --version");
        assert!(out.status.success());
        let v = String::from_utf8_lossy(&out.stdout);
        assert!(v.starts_with("ripgrep "), "unexpected rg version: {v}");
    }

    #[tokio::test]
    async fn run_finds_and_reports_no_matches() {
        let dir = std::env::temp_dir().join(format!("lingmiao-rg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "hello world\n").unwrap();
        std::fs::write(dir.join("b.txt"), "nothing here\n").unwrap();
        let args = vec!["--hidden".to_string(), "hello".to_string()];
        let hit = run(&args, &dir, Duration::from_secs(10)).await.unwrap();
        assert!(!hit.timed_out);
        assert_eq!(hit.lines.len(), 1);
        assert!(hit.lines[0].contains("hello world"), "{:?}", hit.lines);

        let none = run(
            &["--hidden".to_string(), "zzz-nope".to_string()],
            &dir,
            Duration::from_secs(10),
        )
        .await
        .unwrap();
        assert!(none.lines.is_empty());
        assert!(!none.timed_out);
        std::fs::remove_dir_all(&dir).ok();
    }
}

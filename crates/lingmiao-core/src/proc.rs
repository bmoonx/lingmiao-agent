//! Process-group helpers for the wait poll (F 项).
//!
//! `kill_on_drop` signals only the **direct** child. A shell command that spawns
//! its own children (`sh -c "python x.py"`) leaves those grandchildren running
//! invisibly once the wait is abandoned — and in a full-stack test the "python"
//! is a fork of the very binary being killed, so leaving it alive means the test
//! harness itself never exits. This module makes the child a **group leader** and
//! kills the whole group when a wait ends.
//!
//! It lives in `lingmiao-core` rather than the tools crate because signalling a
//! group needs one `libc::kill` (unsafe): this crate already carries a single
//! documented `unsafe` for `envkeys`, whereas `lingmiao-tools` keeps
//! `#![forbid(unsafe_code)]` — weakening that guarantee for one syscall would be
//! a worse trade than keeping both narrow bits of unsafe in one place.

use tokio::process::Command;

/// Put `cmd`'s child in its own process group (a no-op off unix).
///
/// The child becomes the group leader, so its pid doubles as the group id and
/// [`kill_group`] can reach every descendant it spawns.
pub fn isolate_process_group(cmd: &mut Command) {
    #[cfg(unix)]
    cmd.process_group(0);
    #[cfg(not(unix))]
    let _ = cmd;
}

/// `SIGKILL` the whole process group led by `pid` (best effort).
///
/// `SIGKILL` rather than `SIGTERM`: the wait is already over — a command that
/// was cut short must stop now, not negotiate. A negative pid targets the group,
/// which is exactly the reach beyond "the process we spawned" that is wanted
/// here (and the only place in the codebase that signals outside its own child).
pub fn kill_group(pid: u32) {
    #[cfg(unix)]
    unsafe {
        // SAFETY: `libc::kill` is a syscall wrapper with no memory contract; the
        // only hazard is an invalid pid, for which the kernel simply returns
        // ESRCH. The return value is deliberately ignored (best effort).
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
    #[cfg(not(unix))]
    let _ = pid;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn kill_group_reaps_a_grandchild() {
        // The real case: `sh -c "sh -c 'sleep 300'"` — the sleeper is a
        // grandchild of this process, so `kill_on_drop` on the direct child would
        // leave it running.
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg("sh -c 'sleep 300' & wait")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        isolate_process_group(&mut cmd);
        let child = cmd.spawn().expect("spawn");
        let pid = child.id().expect("pid");
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        drop(child);
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        // `pgrep -x sleep` matches the *process name* exactly, so this probe can
        // never match itself (unlike `-f`, which matches its own command line).
        let alive = || {
            std::process::Command::new("pgrep")
                .arg("-x")
                .arg("sleep")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        };
        assert!(alive(), "the grandchild is running before the kill");
        kill_group(pid);
    }
}

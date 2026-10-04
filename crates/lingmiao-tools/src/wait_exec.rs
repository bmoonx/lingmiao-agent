//! Run a child process under the unified wait poll（F 项，cli 2026-10-05 拍板）.
//!
//! The three "wait for an external command" sites (⑤ `bash`, ⑥ 验收命令, ⑦ 代码搜索)
//! all used `Command::output()` — one blocking call with a hard timeout and **no
//! signal at all** during the wait. This module replaces it with a spawn + two
//! concurrent pipe readers that [`Progress::touch`] on every chunk, so the wait
//! state can honestly answer cli's question 「有没有新进展」:
//!
//! * a `cargo build` printing progress → the clock keeps being touched → the
//!   judge is never consulted, however long the build takes;
//! * a command that printed its last line and then hung → the clock goes silent →
//!   after its own limit the judge is asked (continue / interrupt).
//!
//! With **no judge in force** (unit tests, library users) the wait collapses back
//! to exactly the old `tokio::time::timeout(limit, cmd.output())` contract —
//! same bytes, same status, same `timed_out` outcome.

use std::process::ExitStatus;
use std::sync::Arc;
use std::time::Duration;

use lingmiao_core::polling::{Guarded, Progress, WaitAborted, WaitClass};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

/// Put the child in its own **process group** — see
/// [`lingmiao_core::proc::isolate_process_group`].
pub use lingmiao_core::proc::{isolate_process_group, kill_group};

/// What one polled child process produced.
#[derive(Debug)]
pub struct ExecResult {
    /// Captured stdout bytes.
    pub stdout: Vec<u8>,
    /// Captured stderr bytes.
    pub stderr: Vec<u8>,
    /// Exit status (`None` when the wait was cut short).
    pub status: Option<ExitStatus>,
    /// `true` when the site's own limit expired with no judge armed.
    pub timed_out: bool,
    /// `Some` when the judge ruled the wait stalled and the child was dropped.
    pub aborted: Option<WaitAborted>,
    /// The child (and its group) was force-killed rather than exiting on its own.
    pub killed: bool,
}

impl ExecResult {
    /// A cut-short result (no status, no output).
    fn cut(aborted: Option<WaitAborted>, timed_out: bool, killed: bool) -> Self {
        Self {
            stdout: Vec::new(),
            stderr: Vec::new(),
            status: None,
            timed_out,
            aborted,
            killed,
        }
    }
}

/// Default cap on the bytes captured from each of a child's two pipes.
///
/// F 项: `run_polled` reads the pipes itself (that is what lets it touch the
/// progress clock per chunk), so it also owns the bound — a runaway process must
/// not balloon memory. 4 MiB is far above any tool's own output cap
/// (`BASH_OUTPUT_CAP` / `GREP_OUTPUT_CAP` are 64 KiB), so nothing real is lost.
pub const DEFAULT_MAX_CAPTURE: usize = 4 * 1024 * 1024;

/// Read a pipe to EOF, touching `progress` on every chunk.
///
/// Chunk-level touching is what makes the state honest: an idle-but-alive
/// process (no output for minutes) is silent, a verbose one is not.
async fn drain<R>(mut r: R, progress: Arc<Progress>) -> Vec<u8>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut buf = [0u8; 8192];
    let mut out = Vec::new();
    loop {
        match r.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                out.extend_from_slice(&buf[..n]);
                progress.touch();
            }
            Err(_) => break,
        }
    }
    out
}

/// Spawn `cmd` and wait for it under the poll, with `legacy` as the site's limit.
///
/// The caller keeps ownership of the spawn configuration (cwd / stdio /
/// `kill_on_drop`) — this only consumes it, reads both pipes and waits. A stdout
/// / stderr read is capped at `max_capture` bytes each so a runaway process can
/// never balloon memory (the previous `output()` had no cap; the call sites cap
/// for display anyway).
pub async fn run_polled(
    class: WaitClass,
    what: impl Into<String>,
    legacy: Duration,
    max_capture: usize,
    cmd: &mut Command,
) -> std::io::Result<ExecResult> {
    let what = what.into();
    let mut child = cmd.spawn()?;
    let child_pid = child.id();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let progress = Progress::new();
    let inner_progress = progress.clone();
    let outcome = lingmiao_core::polling::guard(class, what, legacy, progress, async move {
        let read_out = async {
            match stdout {
                Some(s) => drain(s, inner_progress.clone()).await,
                None => Vec::new(),
            }
        };
        let read_err = async {
            match stderr {
                Some(s) => drain(s, inner_progress.clone()).await,
                None => Vec::new(),
            }
        };
        let (mut out, mut err) = tokio::join!(read_out, read_err);
        let status = child.wait().await.ok();
        out.truncate(max_capture);
        err.truncate(max_capture);
        ExecResult {
            stdout: out,
            stderr: err,
            status,
            timed_out: false,
            aborted: None,
            killed: false,
        }
    })
    .await;

    Ok(match outcome {
        Guarded::Done(r) => r,
        // No judge armed: the site's own limit expired — the same outcome as the
        // old `timeout(limit, cmd.output())`, except the whole group is now
        // reaped (previously `kill_on_drop` signalled only the direct child).
        Guarded::TimedOut => {
            if let Some(pid) = child_pid {
                kill_group(pid);
            }
            ExecResult::cut(None, true, true)
        }
        Guarded::Aborted(a) => {
            if let Some(pid) = child_pid {
                kill_group(pid);
            }
            ExecResult::cut(Some(a), false, true)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingmiao_core::polling::{CodeStall, Verdict, WaitJudge};
    use std::sync::Mutex;

    /// Drive an async test on a current-thread runtime with a generous stack.
    ///
    /// The wait stack (`run_polled` → `guard` → `polled` → the child futures) is
    /// several async layers deep; in a debug build the composed future is large
    /// enough to overflow the default 2 MiB test thread. That is a property of
    /// *tests* (a real turn already runs at this depth on the main task stack),
    /// so the tests give themselves room explicitly.
    fn run_on_big_stack<F, Fut, T>(make: F) -> T
    where
        F: FnOnce() -> Fut + Send,
        Fut: std::future::Future<Output = T> + Send,
        T: Send,
    {
        // Both the future and the runtime must live on the big-stack thread:
        // the composed wait future is deeply nested, and merely *constructing*
        // it (before it is ever polled) already costs a large stack frame in a
        // debug build. `block_on` on a current-thread runtime polls on the
        // calling thread, so the whole thing has to be created in here.
        std::thread::scope(|scope| {
            std::thread::Builder::new()
                .stack_size(32 * 1024 * 1024)
                .spawn_scoped(scope, || {
                    let fut = make();
                    tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("runtime")
                        .block_on(fut)
                })
                .expect("spawn test thread")
                .join()
                .expect("test thread")
        })
    }

    fn sh(script: &str) -> Command {
        let mut c = Command::new("sh");
        c.arg("-c")
            .arg(script)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        c
    }

    #[test]
    fn a_quick_command_passes_through_with_its_bytes_and_status() {
        run_on_big_stack(|| async {
            let r = run_polled(
                WaitClass::Command,
                "sh -c echo",
                Duration::from_secs(10),
                4096,
                &mut sh("echo hi; echo err >&2"),
            )
            .await
            .expect("spawn");
            assert!(!r.timed_out && r.aborted.is_none());
            assert_eq!(String::from_utf8_lossy(&r.stdout).trim(), "hi");
            assert_eq!(
                String::from_utf8_lossy(&r.stderr).trim(),
                "err",
                "both pipes are captured, as `output()` did"
            );
            assert_eq!(r.status.and_then(|s| s.code()), Some(0));
        })
    }

    #[test]
    fn without_a_judge_the_site_limit_still_cuts_the_wait() {
        run_on_big_stack(|| async {
            // No judge in scope → the legacy semantics must survive exactly.
            let r = run_polled(
                WaitClass::Command,
                "sh -c sleep",
                Duration::from_millis(40),
                4096,
                &mut sh("sleep 30"),
            )
            .await
            .expect("spawn");
            assert!(r.timed_out, "the site limit still applies without a judge");
            assert!(r.aborted.is_none());
            assert!(r.status.is_none());
        })
    }

    /// A judge that always interrupts, to prove the abort path is wired.
    struct AlwaysInterrupt;

    #[async_trait::async_trait]
    impl WaitJudge for AlwaysInterrupt {
        async fn judge(&self, _s: &lingmiao_core::polling::WaitState) -> Verdict {
            Verdict::Interrupt("测试判定停摆".into())
        }
    }

    #[test]
    fn a_silent_command_is_aborted_by_the_judge_inside_its_budget() {
        let judge: Arc<dyn WaitJudge> = Arc::new(AlwaysInterrupt);
        let r = run_on_big_stack(|| {
            lingmiao_core::polling::with_judge(Some(judge), async {
                run_polled(
                    WaitClass::Command,
                    "sh -c sleep",
                    // The judge (not the limit) has to be what stops it; a short
                    // gate keeps the test quick.
                    Duration::from_secs(3),
                    4096,
                    &mut sh("sleep 30"),
                )
                .await
                .expect("spawn")
            })
        });
        let aborted = r.aborted.expect("the judge interrupted the wait");
        assert!(
            aborted.message().contains("测试判定停摆"),
            "{}",
            aborted.message()
        );
        assert!(aborted.message().contains("sh -c sleep"));
        assert!(r.status.is_none());
    }

    #[test]
    fn a_chatty_command_is_never_questioned() {
        // The progress clock is touched by output, so a long-but-verbose command
        // is not mistaken for a stall — the whole point of the poll. The command
        // here prints for longer than the silence gate itself (5s), so "zero
        // rulings" really does prove the gate is progress-aware rather than
        // merely slow to fire.
        struct CountingJudge {
            calls: Arc<Mutex<u32>>,
        }
        #[async_trait::async_trait]
        impl WaitJudge for CountingJudge {
            async fn judge(&self, _s: &lingmiao_core::polling::WaitState) -> Verdict {
                *self.calls.lock().unwrap() += 1;
                Verdict::Continue
            }
        }
        let calls = Arc::new(Mutex::new(0));
        let judge: Arc<dyn WaitJudge> = Arc::new(CountingJudge {
            calls: calls.clone(),
        });
        let r = run_on_big_stack(|| {
            lingmiao_core::polling::with_judge(Some(judge), async {
                let mut c = sh("for i in $(seq 1 11); do echo tick; sleep 0.8; done");
                run_polled(
                    WaitClass::Command,
                    "sh -c ticking",
                    Duration::from_secs(5), // ample: the loop finishes well inside it
                    65536,
                    &mut c,
                )
                .await
                .expect("spawn")
            })
        });
        assert!(r.aborted.is_none() && !r.timed_out, "{r:?}");
        assert!(r.status.is_some(), "the command ran to completion");
        assert_eq!(
            *calls.lock().unwrap(),
            0,
            "a command that keeps printing is never judged"
        );
    }

    #[test]
    fn a_stalled_command_is_interrupted_by_code_when_that_is_the_judge() {
        run_on_big_stack(|| async {
            // ①-style site: the judge is `CodeStall`, so a long silence ends it.
            let judge = CodeStall::handle(Duration::from_millis(50));
            let r = lingmiao_core::polling::guard_with(
                Some(judge),
                WaitClass::Command,
                "sh -c stuck",
                Duration::from_millis(50),
                Progress::new(),
                async {
                    let mut c = sh("sleep 30");
                    let mut child = c.spawn().unwrap();
                    child.wait().await.unwrap();
                    ExecResult::cut(None, false, false)
                },
            )
            .await;
            match r {
                Guarded::Aborted(a) => assert!(a.message().contains("停摆"), "{}", a.message()),
                other => panic!("expected abort, got {other:?}"),
            }
        })
    }
}

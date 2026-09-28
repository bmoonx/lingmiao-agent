//! Structured logging — ported from `core/logging_config.py`.
//!
//! Two channels inside `<state_dir>/logs/`:
//!
//! * `lingmiao-<ts>.jsonl` — machine-readable, full data (one JSON object per line)
//! * `lingmiao-<ts>.log`   — human-readable
//!
//! (The `lingmiao-` stem comes from [`crate::brand::log_file`], not a literal.)
//!
//! The original used `structlog` with a local-time ISO-8601 millisecond
//! timestamp; the custom [`LocalTimer`] reproduces that exact format so the two
//! log streams stay comparable.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tracing_subscriber::filter::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::FormatTime;
use tracing_subscriber::prelude::*;

/// The pair of log files created for a session.
#[derive(Debug, Clone)]
pub struct LogFiles {
    /// Machine-readable JSONL log.
    pub jsonl: PathBuf,
    /// Human-readable log.
    pub log: PathBuf,
}

/// Timer producing `2026-09-15T17:43:50.123+08:00` (local ISO-8601, ms).
struct LocalTimer;

impl FormatTime for LocalTimer {
    fn format_time(&self, w: &mut Writer<'_>) -> std::fmt::Result {
        write!(
            w,
            "{}",
            chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, false)
        )
    }
}

/// A cloneable `MakeWriter` that serializes writes to one shared file handle.
#[derive(Clone)]
struct SharedWriter(Arc<Mutex<File>>);

struct SharedGuard(Arc<Mutex<File>>);

impl Write for SharedGuard {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.lock().unwrap().flush()
    }
}

impl<'a> MakeWriter<'a> for SharedWriter {
    type Writer = SharedGuard;
    fn make_writer(&'a self) -> Self::Writer {
        SharedGuard(self.0.clone())
    }
}

fn open_log(dir: &Path, name: &str) -> io::Result<SharedWriter> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(name))?;
    Ok(SharedWriter(Arc::new(Mutex::new(file))))
}

/// Configure the global tracing subscriber with the dual-channel writers.
///
/// Must be called once at startup. Returns the two log paths.
pub fn init(log_dir: &Path) -> io::Result<LogFiles> {
    fs::create_dir_all(log_dir)?;
    let ts = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let jsonl_name = crate::brand::log_file(&ts, "jsonl");
    let log_name = crate::brand::log_file(&ts, "log");

    let json_writer = open_log(log_dir, &jsonl_name)?;
    let text_writer = open_log(log_dir, &log_name)?;

    let json_layer = tracing_subscriber::fmt::layer()
        .json()
        .with_timer(LocalTimer)
        .with_target(false)
        .with_writer(json_writer);

    let text_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_timer(LocalTimer)
        .with_target(false)
        .with_writer(text_writer);

    // Level filter — default **INFO**; override with `<PREFIX>_LOG` (e.g.
    // `LINGMIAO_LOG=lingmiao=debug,tokenizers=off`) or the generic `RUST_LOG`.
    //
    // Without an explicit filter the registry installs **no** max level, so every
    // dependency (tokenizers/ort/ureq/h2) logs at TRACE. Formatting thousands of
    // TRACE lines per embedded string makes the startup embedder ~100× slower
    // (`memory: using fastembed…` → first paint went from ~1.6 s to ~3 min), so
    // the default must be INFO.
    let level = std::env::var(crate::brand::env("LOG"))
        .ok()
        .or_else(|| std::env::var("RUST_LOG").ok())
        .unwrap_or_else(|| "info".to_string());
    let filter = EnvFilter::try_new(&level).unwrap_or_else(|_| EnvFilter::new("info"));

    // Ignore an already-installed subscriber (e.g. a second call in tests).
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(json_layer)
        .with(text_layer)
        .try_init();

    Ok(LogFiles {
        jsonl: log_dir.join(jsonl_name),
        log: log_dir.join(log_name),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_creates_both_channels() {
        let dir = std::env::temp_dir().join(format!("lingmiao-log-test-{}", std::process::id()));
        let files = init(&dir).expect("init logging");
        assert!(files.jsonl.exists());
        assert!(files.log.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

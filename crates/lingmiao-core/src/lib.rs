//! # lingmiao-core — core infrastructure
//!
//! The horizontal infrastructure layer every other crate builds on
//! (Q8 decision). Module map:
//!
//! | module    | ported from              | purpose                                   |
//! |-----------|--------------------------|-------------------------------------------|
//! | [`brand`] | *(new)*                  | product name — the only place it's spelled |
//! | [`config`]| `core/config.py`         | embedded + overridable typed config       |
//! | [`paths`] | `core/paths.py`          | CC-style state-dir layout, DB whitelist   |
//! | [`errors`]| `core/errors.py`         | layered errors carrying `recoverable`     |
//! | [`events`]| `core/events.py`         | 14-variant `Event` enum + event bus       |
//! | [`envkeys`]| `core/envkeys.py`       | five-level `.env` resolution              |
//! | [`logging`]| `core/logging_config.py`| dual-channel (jsonl + log) tracing        |
//!
//! The Python originals live on disk only as a behaviour baseline (Q1); every
//! port targets field-level parity so a recorded event stream can be diffed.
//!
//! NOTE: `envkeys` needs a single `std::env::set_var` call (unsafe since edition
//! 2024) to mirror the original's `os.environ.setdefault` semantics, so this
//! crate does not `#![forbid(unsafe_code)]`. That call is the only unsafe code
//! in the crate and is confined to the startup path.

pub mod brand;
pub mod config;
pub mod envkeys;
pub mod errors;
pub mod events;
pub mod image;
pub mod logging;
pub mod paths;
pub mod polling;
pub mod proc;

pub use config::{Config, HelpTopic, embedded_help_topics};
pub use errors::{ConfigError, LingmiaoError, LingmiaoErrorKind};
pub use events::{Event, EventBus, Usage};
pub use paths::Paths;
pub use polling::{
    CodeStall, Guarded, PHASE_ASKING, PHASE_CONTINUE, PHASE_DONE, PHASE_INTERRUPT, PHASE_SAMPLING,
    PollPolicy, Progress, SYNC_WAIT_TTL, SyncWait, Verdict, WaitAborted, WaitClass, WaitJudge,
    WaitState, begin_sync_wait, current_judge, current_sync_wait, current_wait_bus, end_sync_wait,
    global_policy, guard, guard_at, guard_capped, guard_code, guard_with, poll_global, polled,
    publish_sync_wait, with_judge, with_wait_bus, with_wait_scope,
};

/// The product version string (mirrors the Python `__version__`).
///
/// NOTE: the version's single source of truth is the git tag; this constant is
/// only the in-binary fallback and must stay in sync with `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

//! Product identity — the single source of truth for the project's name.
//!
//! The product is **灵妙-Agent (lingmiao)**: the display **title** is
//! `灵妙-Agent`, while the identifier slug / binary / directories stay
//! `lingmiao` (cli 2026-09-29 — "只标题叫这个名字，其他还是叫 lingmiao").
//! Every place that renders or keys off the
//! name reads these constants instead of hardcoding the string. **To rename the
//! product, edit only this module** (plus the prose `description`s in the
//! `Cargo.toml` files and `README.md`, which cannot reference Rust constants).
//!
//! Values Rust cannot derive at compile time (env-var names, log-file names)
//! go through the helpers below. The `tests` module guards the constants
//! against drift, so a future rename cannot accidentally split them.

/// Human-facing product **title**, mixed case (boot banner, TUI title, prompts).
pub const NAME: &str = "灵妙-Agent";

/// Identifier slug, ASCII lowercase — drives the per-user directory, the
/// env-var prefix, the log-file stem and the tracing target.
pub const SLUG: &str = "lingmiao";

/// Binary / command name the user types (also the `msg:` prefix on stderr).
pub const BIN: &str = "lingmiao";

/// Per-project memory directory name (a fixed literal, like `.git`/`.env`).
/// Holds the durable memory assets — the whitelisted SQLite DBs plus the
/// `constraints/` / `trajectories/` / `embeddings/` / `whiteboard/` sub-dirs.
/// Independent of the brand so a rename never moves a user's memory.
pub const MEMORY_DIR: &str = ".memory";

/// Per-project cache directory name (a fixed literal) for volatile runtime
/// artifacts (logs / tmp / loop). Sits beside [`MEMORY_DIR`]; safe to delete.
pub const CACHE_DIR: &str = ".cache";

/// Per-user directory name under `$HOME`, holding the global `.env` key file.
/// Derived from the slug (`"." + SLUG`) so a rename carries it along.
pub const USER_DIR: &str = ".lingmiao";

/// Uppercase environment-variable prefix. Must equal `SLUG` uppercased.
pub const ENV_PREFIX: &str = "LINGMIAO";

/// Tracing target for event-bus lines. Must equal `SLUG + ".events"`.
pub const EVENTS_TARGET: &str = "lingmiao.events";

/// Build an environment-variable name from a suffix.
///
/// ```
/// # use lingmiao_core::brand;
/// assert_eq!(brand::env("HOME"), "LINGMIAO_HOME");
/// ```
pub fn env(suffix: &str) -> String {
    format!("{ENV_PREFIX}_{suffix}")
}

/// Project-relative cache directory for the running product — [`CACHE_DIR`]
/// joined with [`BIN`] (e.g. `.cache/lingmiao`).
pub fn cache_rel() -> String {
    format!("{CACHE_DIR}/{BIN}")
}

/// Log file name for a session: `lingmiao-<ts>.<ext>`.
pub fn log_file(ts: &str, ext: &str) -> String {
    format!("{SLUG}-{ts}.{ext}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_values_stay_consistent() {
        assert_eq!(USER_DIR, format!(".{SLUG}").as_str());
        assert_eq!(ENV_PREFIX, SLUG.to_uppercase());
        assert_eq!(EVENTS_TARGET, format!("{SLUG}.events").as_str());
        assert_eq!(env("CONFIG_DIR"), format!("{ENV_PREFIX}_CONFIG_DIR"));
        assert_eq!(cache_rel(), format!("{CACHE_DIR}/{BIN}"));
        assert_eq!(
            log_file("20260916-143000", "jsonl"),
            "lingmiao-20260916-143000.jsonl"
        );
    }

    #[test]
    fn state_dirs_are_brand_independent() {
        // 需求③: memory/cache dirs are fixed literals, NOT derived from SLUG —
        // a rename must never relocate a user's memory.
        assert_eq!(MEMORY_DIR, ".memory");
        assert_eq!(CACHE_DIR, ".cache");
        assert_ne!(MEMORY_DIR, format!(".{SLUG}"));
    }
}

//! Global API-key (.env) loading — ported from `core/envkeys.py`.
//!
//! API keys are global to the user/installation, not per-project. Resolution
//! order (already-exported env vars always win):
//!
//! 1. `~/<user_dir>/.env`           — user-level global (recommended)
//! 2. `$<PREFIX>_HOME/.env`         — distribution install dir
//! 3. `<exe dir>/.env`          — install root / dev checkout
//! 4. `cwd` walk-up (≤ 5 levels)— legacy per-project compat (engine only)
//! 5. `~/.claude/settings.json` — DeepSeek fallback (Claude Code settings)

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

/// Read a `.env` file into the process environment.
///
/// Existing variables always take precedence (`setdefault` semantics). Returns
/// `true` when the file existed and was read.
pub fn read_env_file(env_file: &Path) -> bool {
    let Ok(text) = fs::read_to_string(env_file) else {
        return false;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            let key = key.trim();
            if key.is_empty() {
                continue;
            }
            if std::env::var_os(key).is_none() {
                // SAFETY: called once during single-threaded startup, before any
                // worker threads exist — no concurrent env access can occur.
                unsafe { std::env::set_var(key, value.trim()) };
            }
        }
    }
    true
}

/// DeepSeek key/base-url fallback from Claude Code settings.
fn claude_settings_fallback() {
    if std::env::var_os("DEEPSEEK_API_KEY").is_some() {
        return;
    }
    let Some(settings) = home_dir().map(|h| h.join(".claude").join("settings.json")) else {
        return;
    };
    let Ok(text) = fs::read_to_string(&settings) else {
        return;
    };
    let Ok(cfg) = serde_json::from_str::<Value>(&text) else {
        return;
    };
    let env_cfg = &cfg["env"];
    let key = env_cfg["ANTHROPIC_API_KEY"].as_str().unwrap_or("");
    if key.is_empty() {
        return;
    }
    // SAFETY: startup path, single-threaded.
    unsafe { std::env::set_var("DEEPSEEK_API_KEY", key) };
    let base = env_cfg["ANTHROPIC_BASE_URL"].as_str().unwrap_or("");
    if !base.is_empty() && std::env::var_os("DEEPSEEK_BASE_URL").is_none() {
        unsafe { std::env::set_var("DEEPSEEK_BASE_URL", base) };
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Install root as seen by the running binary (its executable's directory).
fn install_root() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
}

/// Load API keys from the global locations listed in the module docs.
///
/// `cwd = None` skips the legacy per-project walk-up (engine-loop semantics);
/// pass a directory to enable it (`engine.py` semantics).
pub fn load_env(cwd: Option<&Path>) {
    // 1. User-level global
    if let Some(home) = home_dir() {
        read_env_file(&home.join(crate::brand::USER_DIR).join(".env"));
    }

    // 2. Distribution install dir
    if let Some(home) = std::env::var_os(crate::brand::env("HOME")).map(PathBuf::from)
        && !home.as_os_str().is_empty()
    {
        read_env_file(&home.join(".env"));
    }

    // 3. Install root (executable directory)
    if let Some(root) = install_root() {
        read_env_file(&root.join(".env"));
    }

    // 4. Legacy per-project walk-up
    if let Some(cwd) = cwd {
        let mut p = cwd.to_path_buf();
        for _ in 0..5 {
            if read_env_file(&p.join(".env")) {
                break;
            }
            match p.parent() {
                Some(parent) => p = parent.to_path_buf(),
                None => break,
            }
        }
    }

    // 5. Claude Code settings fallback
    claude_settings_fallback();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_returns_false() {
        assert!(!read_env_file(Path::new("/no/such/.env")));
    }
}

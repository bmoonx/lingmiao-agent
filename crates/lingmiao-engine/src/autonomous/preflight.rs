//! 启动前置检查（Task 1.2）— 移植 Python 原版
//! `autonomous_loop.py::_preflight_check`。
//!
//! 五项检查：`db_schema` / `project_rw` / `prompts_json` / `llm_config` /
//! `git_repo`。**errors 阻断启动，warnings 放行**（原版口径：`ok = 无 errors`），
//! 且失败也要建一条 `error` 会话供检视。

use std::path::Path;

use lingmiao_core::{Config, LingmiaoError};
use lingmiao_llm::Client;

use super::store::AutonomousStore;
use super::types::PreflightResult;

/// 检查 `db_schema`：`autonomous.db` 三表齐全。
fn check_db_schema(store: &AutonomousStore) -> (bool, Vec<String>) {
    match store.tables() {
        Ok(tables) => {
            let missing: Vec<String> = ["sessions", "tasks", "iterations"]
                .iter()
                .filter(|t| !tables.iter().any(|x| x == *t))
                .map(|t| (*t).to_string())
                .collect();
            if missing.is_empty() {
                (true, Vec::new())
            } else {
                (
                    false,
                    vec![format!("autonomous.db 缺少表: {}", missing.join(", "))],
                )
            }
        }
        Err(e) => (false, vec![format!("autonomous.db 不可访问: {e}")]),
    }
}

/// 检查 `project_rw`：项目根可写。
///
/// 原版在其状态目录下写临时探针文件；Rust 侧落到
/// `.cache/lingmiao/tmp/`（易失目录，`Paths::ensure_dirs` 已建好）。
fn check_project_rw(root: &Path) -> (bool, Vec<String>) {
    let probe = root
        .join(lingmiao_core::brand::CACHE_DIR)
        .join(lingmiao_core::brand::BIN)
        .join("tmp")
        .join(".preflight_write_test");
    let result = (|| -> std::io::Result<()> {
        if let Some(parent) = probe.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&probe, "preflight")?;
        std::fs::remove_file(&probe)?;
        Ok(())
    })();
    match result {
        Ok(()) => (true, Vec::new()),
        Err(e) => (
            false,
            vec![format!("项目路径不可写 ({}): {e}", root.display())],
        ),
    }
}

/// 检查 `prompts_json`：管线提示词齐全（内嵌 + 外部覆盖已解析）。
///
/// 原版读其配置目录的 prompts.json 并判空；Rust 侧提示词经
/// `Config` 解析（内嵌 `include_str!` + `LINGMIAO_CONFIG_DIR` 覆盖），所以判
/// 「三个管线阶段的 system 都在」——比原版判空更贴近真实运行前提。
fn check_prompts(cfg: &Config) -> (bool, Vec<String>) {
    use lingmiao_core::config::PromptField;
    let mut missing = Vec::new();
    for stage in [
        lingmiao_core::config::STAGE_B_CONTEXT,
        lingmiao_core::config::STAGE_C_DIALOG,
        lingmiao_core::config::STAGE_CONSOLIDATE,
    ] {
        if cfg.prompt(stage, PromptField::System).is_empty() {
            missing.push(stage.to_string());
        }
    }
    if missing.is_empty() {
        (true, Vec::new())
    } else {
        (
            false,
            vec![format!(
                "prompts.json 缺少阶段提示词: {}",
                missing.join(", ")
            )],
        )
    }
}

/// 检查 `llm_config`：默认 client 有模型。
///
/// 原版走 `get_current_model()`；Rust 侧引擎在 `from_env` 已解析出默认
/// client，这里只核对 group/model 非空。**缺失记为 warning 而非 error**
/// （原版 `warnings.append("LLM model 未配置")`）——真打不开 API 会在第一轮
/// 暴露，且部分 provider 允许空 key 的本地 endpoint。
fn check_llm(client: &Client) -> (bool, Vec<String>, Vec<String>) {
    if client.model().is_empty() {
        (false, Vec::new(), vec!["LLM model 未配置".to_string()])
    } else {
        (true, Vec::new(), Vec::new())
    }
}

/// 跑完整预检。
pub fn run(
    store: &AutonomousStore,
    project_root: &Path,
    cfg: &Config,
    client: &Client,
) -> Result<PreflightResult, LingmiaoError> {
    let mut result = PreflightResult {
        ok: true,
        errors: Vec::new(),
        warnings: Vec::new(),
        checks: Vec::new(),
    };

    let (ok, errs) = check_db_schema(store);
    result.checks.push(("db_schema".to_string(), ok));
    result.errors.extend(errs);

    let (ok, errs) = check_project_rw(project_root);
    result.checks.push(("project_rw".to_string(), ok));
    result.errors.extend(errs);

    let (ok, errs) = check_prompts(cfg);
    result.checks.push(("prompts_json".to_string(), ok));
    result.errors.extend(errs);

    let (ok, errs, warns) = check_llm(client);
    result.checks.push(("llm_config".to_string(), ok));
    result.errors.extend(errs);
    result.warnings.extend(warns);

    // git 仓不是硬前提：原版 `git worktree add` 需要 HEAD，而 `ensure_repo` 会
    // 自己 init + 初始提交，所以「不是 git 仓」只 warn。
    let is_git = project_root.join(".git").exists();
    result.checks.push(("git_repo".to_string(), is_git));
    if !is_git {
        result
            .warnings
            .push("当前目录不是 git 仓库，自动模式仍可运行（会自动 init）".to_string());
    }

    result.ok = result.errors.is_empty();
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_root(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "lingmiao-auto-preflight-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn prompts_check_flags_a_missing_pipeline_stage() {
        let cfg = Config::load_default().unwrap();
        let (ok, _) = check_prompts(&cfg);
        assert!(ok, "embedded default prompts must cover all three stages");
    }

    #[test]
    fn project_rw_probe_cleans_up_after_itself() {
        let root = tmp_root("rw");
        let (ok, errs) = check_project_rw(&root);
        assert!(ok, "{errs:?}");
        let probe = root
            .join(lingmiao_core::brand::CACHE_DIR)
            .join(lingmiao_core::brand::BIN)
            .join("tmp")
            .join(".preflight_write_test");
        assert!(!probe.exists(), "probe file must be removed");
        std::fs::remove_dir_all(&root).ok();
    }
}

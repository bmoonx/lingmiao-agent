//! 会话报告与逐轮 trajectory — 移植 Python 原版
//! `autonomous_loop.py::_generate_report` / `_record_trajectory`。
//!
//! 报告（`auto-report-<sid>.md`）是会话结束后的**人可读**产物：状态 / 轮次 /
//! 任务结果 / 教训 / Next Steps（review→merge→cleanup 三条 git 命令）。
//!
//! trajectory（`<sid>.jsonl`）是**机可读**的逐轮事件流（appendix-only）：
//! `MAIN_START` / `MAIN_END` / `AUDITOR_START` / `AUDITOR_END`，供 MetaLoop 与
//! 约束发现事后分析（原版 Task 2.1 / 3.1 / 3.2）。

use std::path::{Path, PathBuf};

use lingmiao_core::LingmiaoError;
use serde_json::{Value, json};

use super::store::AutonomousStore;
use super::types::{AutonomousSession, IterationResult, SessionStatus};

/// 报告 / trajectory 落点的目录结构。
#[derive(Debug, Clone)]
pub struct ReportPaths {
    /// 报告目录（`.cache/lingmiao/logs`）。
    pub logs_dir: PathBuf,
    /// trajectory 目录（`.memory/trajectories`）。
    pub trajectories_dir: PathBuf,
}

impl ReportPaths {
    /// 由项目根推导（沿用 `Paths` 的布局常量）。
    pub fn for_root(root: &Path, paths: &lingmiao_core::Paths) -> Self {
        let _ = root;
        Self {
            logs_dir: paths.logs_dir.clone(),
            trajectories_dir: paths.memory_dir.join("trajectories"),
        }
    }

    /// 报告文件路径 `auto-report-<sid>.md`。
    pub fn report_file(&self, session_id: &str) -> PathBuf {
        self.logs_dir.join(format!("auto-report-{session_id}.md"))
    }

    /// 逐会话日志 `auto-session-<sid>.log`。
    pub fn session_log(&self, session_id: &str) -> PathBuf {
        self.logs_dir.join(format!("auto-session-{session_id}.log"))
    }

    /// trajectory 文件 `<sid>.jsonl`。
    pub fn trajectory_file(&self, session_id: &str) -> PathBuf {
        self.trajectories_dir.join(format!("{session_id}.jsonl"))
    }
}

/// 输出摘要（原版 `_summarize_output`）：取前 300 字符，尽量切在句末。
pub fn summarize_output(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len().min(300);
    let truncated: String = chars[..n].iter().collect();
    // 找最后一个句末（中文「。」或英文 ". "），按**字符**下标比较 ——
    // 原版 Python 的 `rfind` 返回字符下标；Rust 的 `str::rfind` 返回**字节**下标，
    // 直接用它切中文串会 panic（`byte index is not a char boundary`）。
    let last_cn = truncated
        .char_indices()
        .rev()
        .find(|(_, c)| *c == '。')
        .map(|(i, _)| i);
    let last_en = truncated.rfind(". ");
    // 统一成**字符**下标再比较（原版 Python 的 `rfind` 就是字符下标；Rust 的
    // `str::rfind` 给字节下标，中英混排时两者不同）。
    let char_idx = |byte: usize| truncated[..byte].chars().count();
    let cut = match (last_cn.map(char_idx), last_en.map(char_idx)) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    };
    match cut {
        // 句末太靠前就不切（否则摘要只剩一句话）。
        Some(i) if i > 50 => chars[..=i].iter().collect(),
        _ => truncated,
    }
}

/// 追加一条逐轮 trajectory 记录（原版 `_record_trajectory`）。
///
/// 一次追加 4 行（MAIN_START / MAIN_END / AUDITOR_START / AUDITOR_END），每行
/// 一个 JSON 对象；失败只告警，不影响会话。
pub fn record_trajectory(
    paths: &ReportPaths,
    session: &AutonomousSession,
    it: &IterationResult,
) -> Result<(), LingmiaoError> {
    std::fs::create_dir_all(&paths.trajectories_dir)
        .map_err(|e| LingmiaoError::memory(super::STORE, format!("mkdir trajectories: {e}")))?;
    let file = paths.trajectory_file(&session.id);
    let ts = super::now_iso();

    let base = |event: &str| {
        json!({
            "ts": ts,
            "session_id": session.id,
            "iteration": it.iteration,
            "task_id": it.task_id,
            "event": event,
        })
    };
    let mut main_end = base("MAIN_END");
    main_end["summary"] = json!(summarize_output(
        it.main_output
            .get("response")
            .and_then(Value::as_str)
            .unwrap_or("")
    ));
    main_end["tokens"] = it.main_output.get("tokens").cloned().unwrap_or(Value::Null);

    let mut auditor_end = base("AUDITOR_END");
    auditor_end["summary"] = json!(summarize_output(
        it.auditor_output
            .get("response")
            .and_then(Value::as_str)
            .unwrap_or("")
    ));
    auditor_end["continue"] = json!(it.auditor_result.continue_);
    auditor_end["feedback"] = json!(truncate_chars(&it.auditor_result.feedback, 500));
    auditor_end["evidence"] = json!(it.auditor_result.evidence);
    auditor_end["counterfactual"] = it.auditor_result.counterfactual.clone();

    let mut out = String::new();
    for ev in [
        base("MAIN_START"),
        main_end,
        base("AUDITOR_START"),
        auditor_end,
    ] {
        out.push_str(&serde_json::to_string(&ev).unwrap_or_else(|_| "{}".to_string()));
        out.push('\n');
    }
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&file)
        .map_err(|e| LingmiaoError::memory(super::STORE, format!("open trajectory: {e}")))?;
    f.write_all(out.as_bytes())
        .map_err(|e| LingmiaoError::memory(super::STORE, format!("write trajectory: {e}")))?;
    Ok(())
}

/// 按字符数截断（不切坏 UTF-8）。
pub fn truncate_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// 生成会话报告（原版 `_generate_report`）。返回报告路径。
pub fn generate_report(
    paths: &ReportPaths,
    store: &AutonomousStore,
    session: &AutonomousSession,
    lessons: &[String],
) -> Result<String, LingmiaoError> {
    let tasks = store.get_tasks(&session.id)?;
    let done_count = tasks.iter().filter(|t| t["status"] == "done").count();
    let failed_count = tasks.iter().filter(|t| t["status"] == "failed").count();

    let mut lines: Vec<String> = vec![
        format!("# Auto Report — Session {}", session.id),
        String::new(),
        format!("- **Status**: {}", session.status.as_str()),
        format!("- **Iterations**: {}", session.current_iteration),
        format!("- **Tasks done**: {done_count}"),
        format!("- **Tasks failed**: {failed_count}"),
        format!("- **Branch**: `{}`", session.branch_name),
        format!("- **Worktree**: `{}`", session.worktree_path),
        String::new(),
        "## Goal".to_string(),
        String::new(),
        session.request.goal.clone(),
        String::new(),
        "## Task Results".to_string(),
        String::new(),
    ];
    for task in &tasks {
        let icon = if task["status"] == "done" {
            "✅"
        } else {
            "❌"
        };
        lines.push(format!(
            "- {icon} {} — {}",
            task["description"].as_str().unwrap_or(""),
            task["result"].as_str().unwrap_or("")
        ));
    }
    lines.push(String::new());
    lines.push("## Lessons Learned".to_string());
    lines.push(String::new());
    if lessons.is_empty() {
        lines.push("- None".to_string());
    } else {
        for l in lessons {
            lines.push(format!("- {l}"));
        }
    }
    lines.push(String::new());

    if !session.error.is_empty() {
        lines.push("## Error".to_string());
        lines.push(String::new());
        lines.push(format!("```\n{}\n```", session.error));
        lines.push(String::new());
    }

    lines.push("## Next Steps".to_string());
    lines.push(String::new());
    lines.push(format!("1. Review: `git diff {}`", session.branch_name));
    lines.push(format!("2. Merge: `git merge {}`", session.branch_name));
    lines.push(format!(
        "3. Cleanup: `git worktree remove {}`",
        session.worktree_path
    ));

    std::fs::create_dir_all(&paths.logs_dir)
        .map_err(|e| LingmiaoError::memory(super::STORE, format!("mkdir logs: {e}")))?;
    let file = paths.report_file(&session.id);
    std::fs::write(&file, lines.join("\n"))
        .map_err(|e| LingmiaoError::memory(super::STORE, format!("write report: {e}")))?;
    Ok(file.display().to_string())
}

/// 会话状态的中文终局文案（TUI notice 用）。
pub fn status_notice(status: SessionStatus, brief_goal: &str) -> String {
    match status {
        SessionStatus::Done => "✅ 自动模式完成".to_string(),
        SessionStatus::Stopped => "⏹ 自动模式已停止".to_string(),
        SessionStatus::Error => format!("❌ 自动模式出错 · {brief_goal}"),
        SessionStatus::Pending | SessionStatus::Running => {
            format!("⏳ 自动模式进行中 · {brief_goal}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingmiao_core::Paths;

    #[test]
    fn summarize_truncates_at_sentence_boundary_when_possible() {
        assert_eq!(summarize_output(""), "");
        assert_eq!(summarize_output("短"), "短");
        // 句末在 >50 字处 → 切在句末（原版 `if last_period > 50`）。
        let long = "这是一个足够长的句子用来把句末推过五十个字符的位置。".repeat(3)
            + "后面还有很多内容"
            + &"x".repeat(400);
        let s = summarize_output(&long);
        assert!(
            s.chars().count() <= 301,
            "cap at 300 chars: {}",
            s.chars().count()
        );
        assert!(s.ends_with('。'), "got {s:?}");
        // 句末太靠前（前 50 字内）→ 不切，直接给满 300 字（原版口径）。
        let early = "短句。".to_string() + &"x".repeat(400);
        let s2 = summarize_output(&early);
        assert_eq!(s2.chars().count(), 300);
        assert!(!s2.ends_with('。'), "early sentence must not force a cut");
    }

    #[test]
    fn report_paths_follow_the_layout_constants() {
        let p = Paths::at("/tmp/proj");
        let rp = ReportPaths::for_root(std::path::Path::new("/tmp/proj"), &p);
        assert_eq!(
            rp.report_file("abc"),
            std::path::PathBuf::from("/tmp/proj/.cache/lingmiao/logs/auto-report-abc.md")
        );
        assert_eq!(
            rp.trajectory_file("abc"),
            std::path::PathBuf::from("/tmp/proj/.memory/trajectories/abc.jsonl")
        );
    }

    #[test]
    fn status_notice_speaks_plain_chinese() {
        assert!(status_notice(SessionStatus::Done, "g").contains("完成"));
        assert!(status_notice(SessionStatus::Stopped, "g").contains("停止"));
        assert!(status_notice(SessionStatus::Error, "目标").contains("目标"));
    }
}

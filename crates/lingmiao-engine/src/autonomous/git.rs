//! 自动模式的 git worktree 与 checkpoint 管理 — 移植 Python 原版
//! `core/autonomous_git.py`。
//!
//! 每个自动会话跑在自己的 worktree 里（`<root>/auto-sessions/<sid>`，分支
//! `loop/<sid>`），这样 Main 的改动不污染主工作树，且每轮前可 checkpoint
//! （提交）——Auditor 验收的始终是一个干净、可回退的快照。

use std::path::{Path, PathBuf};
use std::process::Stdio;

use lingmiao_core::LingmiaoError;

use super::STORE;

/// 会话的 git worktree/branch/checkpoint 管理器。
pub struct AutonomousGit {
    /// 主项目根。
    pub project_root: PathBuf,
    /// 会话 id。
    pub session_id: String,
    /// worktree 路径（`<root>/auto-sessions/<sid>`）。
    pub worktree_path: PathBuf,
    /// 分支名（`loop/<sid>`）。
    pub branch_name: String,
}

/// 循环产生的提交带显式身份，使无全局 `user.name` / `user.email` 的机器上
/// checkpoint 也能工作（原版 `_IDENTITY`）。
const IDENTITY: [&str; 4] = [
    "-c",
    "user.name=lingmiao",
    "-c",
    "user.email=lingmiao@local",
];

/// 拼一条带身份的 `git commit` 参数表。
///
/// **`-c` 必须排在子命令之前** —— 它是 git 的全局选项（`git -c k=v commit …`）。
/// 写成 `git commit -c k=v -m msg` 会被 git 直接拒绝：
/// `fatal: options '-m' and '-c' cannot be used together`（本文件曾在
/// `args.splice(1..1, IDENTITY)` 处犯过这个错，骨架因此从未真正提交成功）。
fn identity_commit_args(message: &str) -> Vec<&str> {
    vec![
        IDENTITY[0],
        IDENTITY[1],
        IDENTITY[2],
        IDENTITY[3],
        "commit",
        "-m",
        message,
    ]
}

impl AutonomousGit {
    /// 为一个会话构建管理器（不触碰磁盘）。
    pub fn new(project_root: impl Into<PathBuf>, session_id: impl Into<String>) -> Self {
        let project_root = project_root.into();
        let session_id = session_id.into();
        Self {
            worktree_path: project_root.join("auto-sessions").join(&session_id),
            branch_name: format!("loop/{session_id}"),
            project_root,
            session_id,
        }
    }

    /// 在 `cwd` 里跑一条 git 命令，返回是否成功 + stdout。
    fn run(&self, cwd: &Path, args: &[&str]) -> Result<(bool, String), LingmiaoError> {
        let (ok, stdout, stderr) = self.run_verbose(cwd, args)?;
        if !ok {
            // 失败必须留痕：静默的 `git` 失败曾让 `create_worktree` 报出
            // 「create worktree failed: 」这种无信息量的错误。
            tracing::warn!(
                cwd = %cwd.display(),
                args = ?args,
                stderr = %stderr.trim(),
                "auto-mode git command failed"
            );
        }
        Ok((ok, stdout))
    }

    /// 同 [`Self::run`]，但把 **stderr** 一并带出来。
    fn run_verbose(
        &self,
        cwd: &Path,
        args: &[&str],
    ) -> Result<(bool, String, String), LingmiaoError> {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| LingmiaoError::memory(STORE, format!("spawn git {args:?}: {e}")))?;
        Ok((
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).to_string(),
            String::from_utf8_lossy(&out.stderr).to_string(),
        ))
    }

    /// 确保 `project_root` 是含至少一个提交的 git 仓（原版 `ensure_repo`）。
    ///
    /// `git worktree add` 需要一个 HEAD 作为分叉点。若项目不是 git 仓（或
    /// 是没有提交的空仓），就 init + 初始提交；`.memory/`、`.cache/`、
    /// `auto-sessions/` 通过 `git add -A` 前的 `.gitignore` 排除，避免把大
    /// SQLite 库提交进去。
    pub fn ensure_repo(&self) -> Result<(), LingmiaoError> {
        let git_dir = self.project_root.join(".git");
        if !git_dir.exists() {
            self.run(&self.project_root, &["init"])?;
            let gi = self.project_root.join(".gitignore");
            let needs_ignore = match std::fs::read_to_string(&gi) {
                Ok(s) => !s.contains("auto-sessions/"),
                Err(_) => true,
            };
            if needs_ignore {
                let mut s = std::fs::read_to_string(&gi).unwrap_or_default();
                if !s.is_empty() && !s.ends_with('\n') {
                    s.push('\n');
                }
                s.push_str(".memory/\n.cache/\nauto-sessions/\n");
                std::fs::write(&gi, s)
                    .map_err(|e| LingmiaoError::memory(STORE, format!("write .gitignore: {e}")))?;
            }
            self.run(&self.project_root, &["add", "-A"])?;
            self.run(
                &self.project_root,
                &identity_commit_args("chore: initial commit (auto by lingmiao)"),
            )?;
            return Ok(());
        }
        let (has_head, _) = self.run(&self.project_root, &["rev-parse", "--verify", "HEAD"])?;
        if !has_head {
            self.run(&self.project_root, &["add", "-A"])?;
            self.run(
                &self.project_root,
                &identity_commit_args("chore: initial commit (auto by lingmiao)"),
            )?;
        }
        Ok(())
    }

    /// 为本会话创建 worktree（原版 `create_worktree`）。
    pub fn create_worktree(&self) -> Result<PathBuf, LingmiaoError> {
        if let Some(parent) = self.worktree_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| LingmiaoError::memory(STORE, format!("mkdir auto-sessions: {e}")))?;
        }
        if self.worktree_path.exists() {
            // 原版仅告警后复用既有 worktree。
            return Ok(self.worktree_path.clone());
        }
        self.ensure_repo()?;
        // 从当前 HEAD 建分支（已存在则忽略）。
        self.run(&self.project_root, &["branch", &self.branch_name])?;
        let wt = self.worktree_path.display().to_string();
        let (ok, stdout) = self.run(
            &self.project_root,
            &["worktree", "add", &wt, &self.branch_name],
        )?;
        if !ok {
            return Err(LingmiaoError::memory(
                STORE,
                format!("create worktree failed: {stdout}"),
            ));
        }
        Ok(self.worktree_path.clone())
    }

    /// 把当前状态提交为 checkpoint（原版 `checkpoint`）。
    ///
    /// worktree 不存在时返回 `false`（不报错）——外部删掉 worktree 或创建失败
    /// 时会走到这条路径。
    pub fn checkpoint(&self, message: &str) -> Result<bool, LingmiaoError> {
        if !self.worktree_path.exists() {
            return Ok(false);
        }
        self.run(&self.worktree_path, &["add", "-A"])?;
        let (_, status) = self.run(&self.worktree_path, &["status", "--porcelain"])?;
        if status.trim().is_empty() {
            return Ok(true); // 无内容可提交
        }
        let (ok, stdout) = self.run(
            &self.worktree_path,
            &identity_commit_args(&format!("checkpoint: {message}")),
        )?;
        if !ok {
            tracing::warn!(out = %stdout, "auto-mode checkpoint failed");
        }
        Ok(ok)
    }

    /// 回退 worktree 到最近一次提交（原版 `reset_to_last_checkpoint`）。
    pub fn reset_to_last_checkpoint(&self) -> Result<bool, LingmiaoError> {
        let (ok, _) = self.run(&self.worktree_path, &["reset", "--hard", "HEAD"])?;
        Ok(ok)
    }

    /// 相对 HEAD 的 diff 摘要（原版 `diff_summary`, `git diff --stat HEAD`）。
    pub fn diff_stat(&self) -> Result<String, LingmiaoError> {
        let (ok, stdout) = self.run(&self.worktree_path, &["diff", "--stat", "HEAD"])?;
        Ok(if ok {
            stdout.trim().to_string()
        } else {
            String::new()
        })
    }

    /// 完整 diff（原版 `full_diff`）。
    pub fn full_diff(&self) -> Result<String, LingmiaoError> {
        let (ok, stdout) = self.run(&self.worktree_path, &["diff", "HEAD"])?;
        Ok(if ok { stdout } else { String::new() })
    }

    /// 移除 worktree 与分支（原版 `remove_worktree`）。
    pub fn remove_worktree(&self) -> Result<(), LingmiaoError> {
        let wt = self.worktree_path.display().to_string();
        let _ = self.run(&self.project_root, &["worktree", "remove", &wt]);
        let _ = self.run(&self.project_root, &["branch", "-D", &self.branch_name]);
        if self.worktree_path.exists() {
            std::fs::remove_dir_all(&self.worktree_path).ok();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "lingmiao-auto-git-{tag}-{}-{:?}",
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
    fn ensure_repo_creates_initial_commit_and_ignores_state_dirs() {
        let root = tmp_root("repo");
        std::fs::write(root.join("a.txt"), "hello").unwrap();
        std::fs::create_dir_all(root.join(".memory")).unwrap();
        std::fs::write(root.join(".memory/observations.db"), "x").unwrap();
        let g = AutonomousGit::new(&root, "s1");
        g.ensure_repo().unwrap();
        let (ok, _) = g.run(&root, &["rev-parse", "--verify", "HEAD"]).unwrap();
        assert!(ok, "HEAD should exist after ensure_repo");
        let (_, ls) = g.run(&root, &["ls-files"]).unwrap();
        assert!(ls.contains("a.txt"));
        assert!(!ls.contains(".memory/"), "memory dir must be excluded");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn worktree_checkpoint_and_diff() {
        let root = tmp_root("wt");
        std::fs::write(root.join("a.txt"), "hello").unwrap();
        let g = AutonomousGit::new(&root, "s2");
        let wt = g.create_worktree().unwrap();
        assert!(wt.exists());
        assert_eq!(g.branch_name, "loop/s2");
        // 无改动 checkpoint 也返回 true。
        assert!(g.checkpoint("noop").unwrap());
        // 在 worktree 里改文件 → checkpoint 提交 → diff_stat 为空。
        std::fs::write(wt.join("a.txt"), "changed").unwrap();
        assert!(g.checkpoint("edit").unwrap());
        let stat = g.diff_stat().unwrap();
        assert!(
            stat.is_empty(),
            "after checkpoint diff vs HEAD is empty: {stat}"
        );
        g.remove_worktree().unwrap();
        assert!(!wt.exists());
        std::fs::remove_dir_all(&root).ok();
    }
}

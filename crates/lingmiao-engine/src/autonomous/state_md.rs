//! 跨轮共享状态 STATE.md — 移植 Python 原版 `core/autonomous_state_md.py`。
//!
//! 每轮 Main/Auditor 都读到同一份 `STATE.md`（目标 / 约束 / 计划 / 已完成 /
//! 失败 / 当前任务 / 增量 / 教训）——这是「一个会话内跨轮记忆」的载体，补足
//! 引擎无状态、每轮从归档重建上下文之外的那部分**会话内**连续性。

use std::path::{Path, PathBuf};

/// 读写循环 STATE.md。
pub struct StateMd {
    /// 文件落点（`.cache/lingmiao/loop/loop-state-<sid>.md`）。
    pub path: PathBuf,
}

/// `write` 的输入（原版 `StateMdManager.write` 的具名参数）。
pub struct StateMdInput<'a> {
    pub goal: &'a str,
    pub plan: &'a [serde_json::Value],
    pub current_task: Option<&'a serde_json::Value>,
    pub lessons: &'a [String],
    pub constraints: &'a str,
    pub increments: &'a str,
}

impl StateMd {
    /// 绑定到 `path`。
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// 写整份 STATE.md。
    pub fn write(&self, input: &StateMdInput<'_>) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut out = String::new();
        out.push_str("# Loop Engineering State\n\n");
        out.push_str(&format!("## Goal\n\n{}\n\n", input.goal));
        out.push_str("## Constraints\n\n");
        out.push_str(if input.constraints.is_empty() {
            "None"
        } else {
            input.constraints
        });
        out.push_str("\n\n## Plan\n");
        for t in input.plan {
            let status = t["status"].as_str().unwrap_or("pending");
            let mark = if status == "done" { "x" } else { " " };
            out.push_str(&format!(
                "- [{mark}] {}: {}\n",
                t["id"].as_str().unwrap_or(""),
                t["description"].as_str().unwrap_or("")
            ));
        }
        out.push('\n');

        out.push_str("## Completed Tasks\n");
        let mut any_done = false;
        for t in input.plan {
            if t["status"].as_str() == Some("done") {
                out.push_str(&format!(
                    "- {}: {}\n",
                    t["id"].as_str().unwrap_or(""),
                    t["description"].as_str().unwrap_or("")
                ));
                any_done = true;
            }
        }
        if !any_done {
            out.push_str("- None\n");
        }
        out.push('\n');

        out.push_str("## Failed Tasks\n");
        let mut any_failed = false;
        for t in input.plan {
            if t["status"].as_str() == Some("failed") {
                out.push_str(&format!(
                    "- {}: {} — {}\n",
                    t["id"].as_str().unwrap_or(""),
                    t["description"].as_str().unwrap_or(""),
                    t["result"].as_str().unwrap_or("")
                ));
                any_failed = true;
            }
        }
        if !any_failed {
            out.push_str("- None\n");
        }
        out.push('\n');

        out.push_str("## Current Task\n");
        match input.current_task {
            Some(t) => out.push_str(&format!(
                "- {}: {}\n",
                t["id"].as_str().unwrap_or(""),
                t["description"].as_str().unwrap_or("")
            )),
            None => out.push_str("- None\n"),
        }
        out.push('\n');

        out.push_str("## Completed Increments\n");
        out.push_str(if input.increments.is_empty() {
            "- None"
        } else {
            input.increments
        });
        out.push_str("\n\n");

        out.push_str("## Lessons Learned\n");
        if input.lessons.is_empty() {
            out.push_str("- None\n");
        } else {
            for l in input.lessons {
                out.push_str(&format!("- {l}\n"));
            }
        }
        out.push('\n');

        std::fs::write(&self.path, out)
    }

    /// 读 STATE.md（缺失或读失败返回空串）。
    pub fn read(&self) -> String {
        std::fs::read_to_string(&self.path).unwrap_or_default()
    }

    /// 路径字符串。
    pub fn path_str(&self) -> String {
        self.path.display().to_string()
    }

    /// 便捷：仅取落点（供调用点预生成路径）。
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn write_and_read_round_trip() {
        let dir = std::env::temp_dir().join(format!(
            "lingmiao-state-md-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let md = StateMd::new(dir.join("loop-state-x.md"));
        let plan = vec![
            json!({"id": "t1", "description": "第一步", "status": "done", "result": ""}),
            json!({"id": "t2", "description": "第二步", "status": "pending", "result": ""}),
        ];
        let lessons = vec!["先读再改".to_string()];
        md.write(&StateMdInput {
            goal: "把事做完",
            plan: &plan,
            current_task: Some(&plan[1]),
            lessons: &lessons,
            constraints: "不许猜",
            increments: "第1轮进行中",
        })
        .unwrap();
        let s = md.read();
        assert!(s.contains("# Loop Engineering State"));
        assert!(s.contains("- [x] t1: 第一步"));
        assert!(s.contains("- [ ] t2: 第二步"));
        assert!(s.contains("## Completed Increments"));
        assert!(s.contains("第1轮进行中"));
        assert!(s.contains("- 先读再改"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_file_reads_empty() {
        let md = StateMd::new("/nonexistent/path/state.md");
        assert_eq!(md.read(), "");
    }
}

//! 自动模式（2-Agent Autonomous Loop）— 一比一移植 Python 原版
//! `core/autonomous_loop.py`。
//!
//! 架构（原版 ADR 0011）：**Main Agent**（完整管线 + 全部工具）产出 →
//! **Auditor Agent**（完整管线 + 只读 + 检索）核验 → `continue:true` 把
//! feedback 回注 Main 下一轮；`continue:false` 退出循环。**没有硬性
//! `max_iterations`**，终止只由 Auditor 判定或用户停止决定。
//!
//! 模块划分（对照原版）：
//!
//! | Rust                  | Python                              |
//! |-----------------------|-------------------------------------|
//! | [`types`]             | `autonomous_types.py`               |
//! | [`store`]             | `autonomous_store.py`               |
//! | [`git`]               | `autonomous_git.py`                 |
//! | [`state_md`]          | `autonomous_state_md.py`            |
//! | [`role`]              | `autonomous_loop.py::_create_role_loops` / `role_runtime.py` |
//! | [`preflight`]         | `autonomous_loop.py::_preflight_check` |
//! | [`run`]               | `autonomous_loop.py::_run_session`  |
//! | [`report`]            | `autonomous_loop.py::_generate_report` |
//! | [`meta`]              | `autonomous_loop.py::_run_meta_loop` / `_discover_constraints` |

pub mod git;
pub mod meta;
pub mod preflight;
pub mod report;
pub mod role;
pub mod run;
pub mod state_md;
pub mod store;
pub mod types;

/// 存储/日志里用的 store 名（供子模块 `use super::STORE`）。
pub const STORE: &str = "autonomous";

/// ISO-8601 秒级时间戳（原版 `time.strftime("%Y-%m-%dT%H:%M:%S")`）。
pub fn now_iso() -> String {
    chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
}

pub use role::{ROLE_AUDITOR, ROLE_MAIN, RoleKind};
pub use run::{AutonomousLoop, RoundOutcome};
pub use types::{AuditorResult, AutonomousRequest, AutonomousSession, SessionStatus, TaskSeed};

//! 自动模式的数据类型 — 一比一移植 Python 原版 `core/autonomous_types.py`。
//!
//! 架构（原版 ADR 0011）：**Main Agent**（完整管线 + 全部工具）产出 →
//! **Auditor Agent**（完整管线 + 只读 + 检索）核验 → `continue:true` 把
//! feedback 回注 Main 下一轮；`continue:false` 退出循环。**没有硬性
//! `max_iterations`**，终止只由 Auditor 判定或用户停止决定。

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 会话生命周期状态（原版 `SessionStatus`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionStatus {
    Pending,
    Running,
    Done,
    Error,
    Stopped,
}

impl SessionStatus {
    /// 与 Python 枚举值一致的稳定小写串。
    pub fn as_str(self) -> &'static str {
        match self {
            SessionStatus::Pending => "pending",
            SessionStatus::Running => "running",
            SessionStatus::Done => "done",
            SessionStatus::Error => "error",
            SessionStatus::Stopped => "stopped",
        }
    }

    /// 从存储的小写串还原（未知值按 `pending` 处理）。
    pub fn parse(s: &str) -> Self {
        match s {
            "running" => SessionStatus::Running,
            "done" => SessionStatus::Done,
            "error" => SessionStatus::Error,
            "stopped" => SessionStatus::Stopped,
            _ => SessionStatus::Pending,
        }
    }
}

/// 一次自动会话的启动请求（原版 `AutonomousRequest`）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AutonomousRequest {
    /// 总目标（自由文本，或 Markdown front-matter 的正文）。
    pub goal: String,
    /// 显式任务列表（为空时由 `goal` 合成单任务）。
    pub tasks: Vec<TaskSeed>,
    /// 时间上限（分钟）；`0` = 不限。
    pub time_limit_minutes: u64,
    /// 每轮之间的间隔秒数（原版默认 5）。
    pub interval_seconds: u64,
    /// 成功标准（交由 Auditor 逐条核验）。
    pub success_criteria: Vec<String>,
    /// 额外约束文本。
    pub constraints: String,
}

/// 任务种子（请求里声明的任务，尚未带上会话 id）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskSeed {
    /// 请求内的局部 id（落库前会被加上 `{session_id}-` 前缀）。
    pub id: String,
    /// 排序序号。
    pub seq: u64,
    /// 任务描述。
    pub description: String,
    /// 依赖的局部 id 列表。
    pub depends_on: Vec<String>,
}

/// 会话中的一条任务（原版 `AutonomousTask`）。
#[derive(Debug, Clone, Default)]
pub struct AutonomousTask {
    pub id: String,
    pub seq: u64,
    pub description: String,
    /// `pending` / `done` / `failed`。
    pub status: String,
    pub depends_on: Vec<String>,
    pub result: String,
}

/// Auditor 的一条结构化反馈（原版 `FeedbackIssue`，Task 2.3 AgentFixer）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FeedbackIssue {
    /// `missing_output` | `quality` | `wrong_approach` | `hallucination`。
    pub category: String,
    pub file: String,
    pub line_hint: String,
    pub description: String,
    pub suggested_fix: String,
    /// Main 处理完后标记（原版由 Main 侧回填）。
    pub resolved: bool,
}

/// 一个审计周期的判定（原版 `AuditorResult`）。
///
/// 承载三层验证结果：`evidence`（完整性 + 证据映射）、`counterfactual`
/// （反事实交叉审计，防幻影修复）、`issues`（结构化反馈条目）。
#[derive(Debug, Clone, Default)]
pub struct AuditorResult {
    /// `true` = 未完成，反馈回注 Main 下一轮。
    pub continue_: bool,
    pub feedback: String,
    pub evidence: Vec<Value>,
    pub counterfactual: Value,
    pub issues: Vec<FeedbackIssue>,
    /// 非空表示本轮 Auditor **自身** fault（超时 / 无进展 / 传输错误）——
    /// 此时 `continue_` 按 `true` 处理（不阻塞主循环，原版口径）。
    pub fault: String,
}

/// 一轮 Main → Auditor 的结果（原版 `IterationResult`）。
#[derive(Debug, Clone, Default)]
pub struct IterationResult {
    pub iteration: u64,
    pub task_id: String,
    /// `{"response": …, "tokens": …}`。
    pub main_output: Value,
    pub auditor_output: Value,
    pub auditor_result: AuditorResult,
    pub error: String,
}

/// 会话的运行视图（原版 `AutonomousSession`）。
#[derive(Debug, Clone)]
pub struct AutonomousSession {
    pub id: String,
    pub request: AutonomousRequest,
    pub status: SessionStatus,
    pub current_iteration: u64,
    pub current_task_id: String,
    pub consecutive_errors: u64,
    pub report_path: String,
    pub worktree_path: String,
    pub branch_name: String,
    pub error: String,
}

impl AutonomousSession {
    /// 所有字段取默认值的占位会话 —— 供结构体更新语法
    /// （`..AutonomousSession::placeholder()`）填充未逐项写出的字段。
    ///
    /// 直接用 `Default` 会要求 `AutonomousRequest: Default`（它确有），但
    /// `SessionStatus` 也需要 `Default`；这里显式给出，避免给枚举加 `Default`
    /// （`Pending` 是唯一合理默认，但不值得为它挂 trait）。
    pub fn placeholder() -> Self {
        Self {
            id: String::new(),
            request: AutonomousRequest::default(),
            status: SessionStatus::Pending,
            current_iteration: 0,
            current_task_id: String::new(),
            consecutive_errors: 0,
            report_path: String::new(),
            worktree_path: String::new(),
            branch_name: String::new(),
            error: String::new(),
        }
    }

    /// 新会话（状态 `Pending`）。
    ///
    /// `request.tasks` 里的任务 id 在 [`super::run::AutonomousLoop::start`] 里
    /// 被加上 `{session.id}-` 前缀（原版口径）；store 落库**原样使用**，不再二次
    /// 拼接（见 `store.rs::create_session` 的注释与回归测试）。
    pub fn new(id: impl Into<String>, request: AutonomousRequest) -> Self {
        Self {
            id: id.into(),
            request,
            status: SessionStatus::Pending,
            current_iteration: 0,
            current_task_id: String::new(),
            consecutive_errors: 0,
            report_path: String::new(),
            worktree_path: String::new(),
            branch_name: String::new(),
            error: String::new(),
        }
    }
}

/// 启动前置检查结果（原版 `PreflightResult`，Task 1.2）。
#[derive(Debug, Clone, Default)]
pub struct PreflightResult {
    /// 无 errors 即通过（warnings 放行）。
    pub ok: bool,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
    /// 逐项检查结果（`db_schema` / `project_rw` / `prompts_json` /
    /// `llm_config` / `git_repo`）。
    pub checks: Vec<(String, bool)>,
}

/// MetaLoop 发现的一条流程改进（原版 `MetaImprovement`，Task 3.1）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MetaImprovement {
    pub pattern_name: String,
    pub pattern_description: String,
    pub suggestion: String,
    /// `minor` | `major` | `critical`。
    pub severity: String,
    pub source_session: String,
    pub created_at: String,
}

/// 自学习约束（原版 `AutoConstraint`，Task 3.2 EPO-Safe）。
///
/// 格式：『当 [条件] 时，禁止 [动作]，因为 [原因]』。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AutoConstraint {
    pub condition: String,
    pub prohibited_action: String,
    pub reason: String,
    pub source_session: String,
    pub iteration: u64,
    pub created_at: String,
    /// 若后续证明有误可停用。
    pub active: bool,
}

impl AutoConstraint {
    /// 新建一条 active 约束。
    pub fn new(
        condition: impl Into<String>,
        prohibited_action: impl Into<String>,
        reason: impl Into<String>,
        source_session: impl Into<String>,
        iteration: u64,
        created_at: impl Into<String>,
    ) -> Self {
        Self {
            condition: condition.into(),
            prohibited_action: prohibited_action.into(),
            reason: reason.into(),
            source_session: source_session.into(),
            iteration,
            created_at: created_at.into(),
            active: true,
        }
    }
}

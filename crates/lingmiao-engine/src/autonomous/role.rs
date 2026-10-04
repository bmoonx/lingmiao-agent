//! 角色运行时 — 移植 Python 原版 `autonomous_loop.py::_create_role_loops`
//! 与 `role_runtime.py::create_role_loops`。
//!
//! 自动模式的两个角色各是一套**独立**的运行时：
//!
//! | 角色 | 工具 | 记忆区 | 角色提示词 |
//! |------|------|--------|-----------|
//! | **Main** | 全部（含写工具） | `.memory/main/` | 无（`""`） |
//! | **Auditor** | 只读 + 检索（禁 7 个写工具） | `.memory/auditor/` | `prompts.json` → `A-验收` |
//!
//! Rust 侧与原版的三点结构性差异（如实记录，见 `docs/auto-mode.md`）：
//!
//! 1. **工具 root 是构建期绑定的**：原版靠 `chdir_locked()`（进程级锁 +
//!    `os.chdir`）把工具跑在 worktree 里；Rust 的 `FileGuard` 在**构建注册表
//!    时**绑定 root，所以 worktree 隔离必须**按会话重建整套运行时**（本模块
//!    的 [`RoleSet::for_worktree`]），而不是改 cwd。
//! 2. **不跑角色内层的 A-J**：原版每个角色是一个完整 `QueryLoop`（自带 B 阶段
//!    上下文选择）；Rust 侧只跑「工作阶段式」的工具循环 —— 角色消息里已经带了
//!    总目标 / 任务 / STATE.md / 反馈，够用，且省一次 LLM 往返。角色的
//!    `{prefix}` 上下文槽留空。
//! 3. **模型路由**：两个角色共用引擎的默认 client（原版 `role_models` 的按角色
//!    覆盖，Rust 侧暂未接）。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use lingmiao_core::{Config, EventBus, LingmiaoError};
use lingmiao_llm::{Client, Message, ModelJudge};
use lingmiao_memory::Memory;
use lingmiao_tools::ToolRegistry;
use serde_json::Value;

use crate::engine::{TOOL_DISCIPLINE, fill_prompt};
use crate::stage_agent::{StageAgent, StageOutcome};

/// Main 角色的稳定标识。
pub const ROLE_MAIN: &str = "main";
/// Auditor 角色的稳定标识。
pub const ROLE_AUDITOR: &str = "auditor";

/// Auditor 禁用的写工具（原版 `_AUDITOR_DISABLED_TOOLS`）。
pub const AUDITOR_DISABLED_TOOLS: [&str; 7] = [
    "write_file",
    "edit",
    "delete_file",
    "copy_file",
    "business_db_execute",
    "update_memory",
    "update_knowledge",
];

/// 角色种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoleKind {
    /// 产出方：完整管线 + 全部工具。
    Main,
    /// 验收方：完整管线 + 只读 + 检索。
    Auditor,
}

impl RoleKind {
    /// 稳定标识（`main` / `auditor`）。
    pub const fn name(self) -> &'static str {
        match self {
            RoleKind::Main => ROLE_MAIN,
            RoleKind::Auditor => ROLE_AUDITOR,
        }
    }

    /// 显示名（`Main` / `Auditor`）。
    pub const fn display(self) -> &'static str {
        match self {
            RoleKind::Main => "Main",
            RoleKind::Auditor => "Auditor",
        }
    }

    /// 事件流 / TUI 图标（原版 `_ROLE_ICONS`）。
    pub const fn icon(self) -> &'static str {
        match self {
            RoleKind::Main => "🤖",
            RoleKind::Auditor => "🔍",
        }
    }

    /// 事件 `stage` 字段用的名字 —— TUI 靠它把角色的工具卡 / 正文与主线回合
    /// 区分开（`Auto·Main` / `Auto·Auditor`）。
    pub const fn stage_name(self) -> &'static str {
        match self {
            RoleKind::Main => "Auto·Main",
            RoleKind::Auditor => "Auto·Auditor",
        }
    }

    /// 角色提示词在 `prompts.json` 的键（Main 无角色块）。
    pub const fn prompt_key(self) -> &'static str {
        match self {
            RoleKind::Main => "",
            RoleKind::Auditor => "A-验收",
        }
    }
}

/// 一个角色的运行结果（原版 `_run_role` 的返回值）。
#[derive(Debug, Clone, Default)]
pub struct RoleOutput {
    /// 该角色回复的文本。
    pub response: String,
    /// 该角色回复里提取出的第一个 JSON 对象（Auditor 的判定就靠它）。
    pub json: Value,
    /// 累计 token 用量。
    pub usage: lingmiao_core::events::Usage,
    /// 工具调用次数。
    pub tool_calls: u64,
    /// 模型推理（`reasoning_content`）全文。
    pub reasoning: String,
    /// 逐条工具调用记录（归档 / trajectory 用）。
    pub tool_call_records: Vec<Value>,
    /// 非空表示该角色本轮 fault（超时 / 无进展 / 传输错误）。
    ///
    /// 原版把角色异常**分类后降级**返回（不让循环炸掉），Rust 侧同口径：
    /// [`RoleRuntime::run`] 把 `Err` 转成 `fault` 而非向上抛。
    pub fault: String,
}

impl RoleOutput {
    /// 该角色本轮是否正常完成。
    pub fn ok(&self) -> bool {
        self.fault.is_empty()
    }
}

/// 一套角色运行时（Main + Auditor），共享工具层的**装配来源**。
///
/// `registry` 与 `memory` 都按会话（worktree）重建 —— 见模块头的差异 ①。
pub struct RoleSet {
    /// Main 角色。
    pub main: RoleRuntime,
    /// Auditor 角色。
    pub auditor: RoleRuntime,
}

/// 单个角色的运行时（独立注册表 + 独立记忆区 + 独立角色提示词）。
pub struct RoleRuntime {
    /// 角色种类。
    pub kind: RoleKind,
    /// 该角色的工具注册表（Auditor 已物理剔除 7 个写工具）。
    pub registry: Arc<ToolRegistry>,
    /// 该角色的记忆区（`main` / `auditor` zone）。
    pub memory: Option<Arc<Memory>>,
    /// 该角色说话用的 LLM client。
    pub client: Client,
    /// 角色提示词块（注入 C 模板的 `{role_block}` 槽）。
    pub role_block: String,
    /// 该角色的工具白名单（角色定死：Main = 全量，Auditor = 全量减 7）。
    pub tools: Vec<String>,
    /// 解析后的配置（渲染 system 用）。
    cfg: Config,
    /// 事件总线。
    bus: Arc<EventBus>,
    /// 每轮时间预算（`0` = 不限）。
    timeout: Duration,
    /// 该角色跑在哪个目录（worktree）；仅用于日志 / 报告。
    pub work_root: PathBuf,
}

impl RoleRuntime {
    /// 组装一个角色。
    ///
    /// `tools` 传 `None` 时按角色默认（Main 全量 / Auditor 全量减写工具）。
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        kind: RoleKind,
        cfg: Config,
        bus: Arc<EventBus>,
        client: Client,
        registry: Arc<ToolRegistry>,
        memory: Option<Arc<Memory>>,
        work_root: impl Into<PathBuf>,
        timeout: Duration,
    ) -> Self {
        let role_block = {
            let key = kind.prompt_key();
            if key.is_empty() {
                String::new()
            } else {
                cfg.prompt(key, lingmiao_core::config::PromptField::System)
                    .to_string()
            }
        };
        let mut tools = registry.names();
        if kind == RoleKind::Auditor {
            // 物理剔除（原版 `registry.disable(t)`）：不在白名单里就不会发给模型，
            // 也不会被 `schemas_for` 解出 schema。
            tools.retain(|n| !AUDITOR_DISABLED_TOOLS.contains(&n.as_str()));
        }
        Self {
            kind,
            registry,
            memory,
            client,
            role_block,
            tools,
            cfg,
            bus,
            timeout,
            work_root: work_root.into(),
        }
    }

    /// 该角色的工具白名单（供面板 / 报告如实列示）。
    pub fn tool_names(&self) -> &[String] {
        &self.tools
    }

    /// 渲染该角色的 system（C 模板 + 角色块；`{prefix}` 留空 —— 见模块头差异 ②）。
    fn render_system(&self) -> String {
        let template = self
            .cfg
            .prompt(crate::STAGE_C, lingmiao_core::config::PromptField::System);
        if template.is_empty() {
            return String::new();
        }
        // `{cwd}` 指向 **work_root（会话 worktree）**，不是进程 cwd：工具层的 root
        // 是 worktree（真隔离），模型必须被告知同一个目录，否则它会按进程 cwd 拼
        // 绝对路径、把改动写进主项目根 —— 隔离当场失效（实测主根 `git status`
        // 出现 `M calc.py` 而 worktree 是空的）。
        let cwd = self.work_root.display().to_string();
        let env_block = fill_prompt(
            self.cfg.env_prompt(),
            &[
                ("cwd", &cwd),
                ("version", lingmiao_core::VERSION),
                ("model", self.client.model()),
                ("brand", lingmiao_core::brand::NAME),
                ("memory_dir", lingmiao_core::brand::MEMORY_DIR),
                ("cache_dir", &lingmiao_core::brand::cache_rel()),
            ],
        );
        let base = fill_prompt(
            self.cfg.base_prompt(),
            &[
                ("brand", lingmiao_core::brand::NAME),
                ("version", lingmiao_core::VERSION),
                ("startup_time", &super::now_iso()),
                ("local_now", &super::now_iso()),
            ],
        );
        let tools_prose = self
            .tools
            .iter()
            .filter_map(|n| {
                self.registry
                    .get(n)
                    .map(|t| format!("- {n}: {}", t.description()))
            })
            .collect::<Vec<_>>()
            .join("\n");
        let mut s = fill_prompt(
            template,
            &[
                ("version", lingmiao_core::VERSION),
                ("brand", lingmiao_core::brand::NAME),
                ("memory_dir", lingmiao_core::brand::MEMORY_DIR),
                ("cache_dir", &lingmiao_core::brand::cache_rel()),
                ("env_prefix", lingmiao_core::brand::ENV_PREFIX),
                ("startup_time", &super::now_iso()),
                ("local_now", &super::now_iso()),
                ("env_block", &env_block),
                ("base", &base),
                // 角色内层不跑 A→B 上下文装配（模块头差异 ②）。
                ("prefix", ""),
                ("reply_instruction", "用中文直接回复用户。"),
                ("project_root_block", ""),
                ("role_block", &self.role_block),
                ("tools", &tools_prose),
            ],
        );
        if !env_block.is_empty() && !s.contains(&env_block) {
            s.push('\n');
            s.push_str(&env_block);
        }
        let strategy = self.cfg.search_strategy();
        if !strategy.is_empty() {
            s.push_str("\n\n");
            s.push_str(strategy);
        }
        s
    }

    /// 跑一次该角色（一次完整的工具循环）。
    ///
    /// 与原版一致：**异常不向上抛**，分类成 [`RoleOutput::fault`] 返回，循环照常
    /// 往下走（原版 `except → format_error_summary`）。
    pub async fn run(&self, prompt: &str) -> RoleOutput {
        let system = self.render_system();
        let tools = self.registry.schemas_for(&self.tools);
        let messages = vec![Message::user(format!("{TOOL_DISCIPLINE}\n\n{prompt}"))];
        let agent = StageAgent::new(
            self.client.clone(),
            self.bus.clone(),
            self.registry.clone(),
            self.timeout,
        )
        // 角色跑的是「工作阶段式」循环，核心锁同工作阶段。
        .with_core_lock(self.cfg.full_core_lock(crate::STAGE_C))
        .with_wait_judge(Some(ModelJudge::handle(self.client.clone())));
        match agent
            .work(self.kind.stage_name(), system, messages, tools)
            .await
        {
            Ok(out) => RoleOutput::from_outcome(out),
            Err(e) => RoleOutput {
                fault: e.message().to_string(),
                ..Default::default()
            },
        }
    }
}

impl RoleOutput {
    /// 由 `StageAgent` 的产出构造（fault 透传）。
    fn from_outcome(out: StageOutcome) -> Self {
        Self {
            response: out.content,
            json: out.json,
            usage: out.usage,
            tool_calls: out.tool_calls,
            reasoning: out.reasoning,
            tool_call_records: out.tool_call_records,
            fault: out.fault,
        }
    }
}

impl RoleSet {
    /// 为一次会话组装两个角色。
    ///
    /// `work_root` 决定工具层的 root（worktree 或项目根）；`registry` 由调用点
    /// 按 `work_root` **重建**（`FileGuard` 构建期绑 root —— 见模块头差异①），
    /// 两个角色共享它，Auditor 的只读性由**角色白名单**保证（原版是同一个
    /// `ToolRegistry` 上 `disable()`，同形）。
    ///
    /// `memory_for` 给出每个角色该用哪个记忆区句柄（`None` = 该会话不启用角色
    /// 记忆，例如记忆层打不开时降级）。
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cfg: Config,
        bus: Arc<EventBus>,
        client: Client,
        work_root: &Path,
        registry: Arc<ToolRegistry>,
        main_memory: Option<Arc<Memory>>,
        auditor_memory: Option<Arc<Memory>>,
        timeout: Duration,
    ) -> Self {
        Self {
            main: RoleRuntime::new(
                RoleKind::Main,
                cfg.clone(),
                bus.clone(),
                client.clone(),
                registry.clone(),
                main_memory,
                work_root,
                timeout,
            ),
            auditor: RoleRuntime::new(
                RoleKind::Auditor,
                cfg,
                bus,
                client,
                registry,
                auditor_memory,
                work_root,
                timeout,
            ),
        }
    }

    /// 按角色取运行时。
    pub fn get(&self, kind: RoleKind) -> &RoleRuntime {
        match kind {
            RoleKind::Main => &self.main,
            RoleKind::Auditor => &self.auditor,
        }
    }
}

/// 解析一个 role 串（大小写不敏感），未知则 `None`。
pub fn parse_role(s: &str) -> Option<RoleKind> {
    match s.trim().to_ascii_lowercase().as_str() {
        "main" => Some(RoleKind::Main),
        "auditor" => Some(RoleKind::Auditor),
        _ => None,
    }
}

/// 供调用点构造「角色名 → 记忆区」映射时的辅助错误。
pub fn missing_zone(kind: RoleKind) -> LingmiaoError {
    LingmiaoError::memory(
        super::STORE,
        format!("role zone `{}` unavailable", kind.name()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_metadata_is_stable() {
        assert_eq!(RoleKind::Main.name(), "main");
        assert_eq!(RoleKind::Auditor.name(), "auditor");
        assert_eq!(RoleKind::Main.prompt_key(), "");
        assert_eq!(RoleKind::Auditor.prompt_key(), "A-验收");
        assert_eq!(RoleKind::Main.stage_name(), "Auto·Main");
        assert_eq!(RoleKind::Auditor.stage_name(), "Auto·Auditor");
        assert_eq!(parse_role(" MAIN "), Some(RoleKind::Main));
        assert_eq!(parse_role("nope"), None);
    }

    #[test]
    fn auditor_blocklist_covers_every_write_tool_the_python_original_blocks() {
        // 原版 `_AUDITOR_DISABLED_TOOLS` 的逐项锁步：少一个都会让 Auditor 能写。
        for name in [
            "write_file",
            "edit",
            "delete_file",
            "copy_file",
            "business_db_execute",
            "update_memory",
            "update_knowledge",
        ] {
            assert!(
                AUDITOR_DISABLED_TOOLS.contains(&name),
                "auditor must not be able to call `{name}`"
            );
        }
    }
}

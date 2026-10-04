//! 自动模式主循环 — 移植 Python 原版 `autonomous_loop.py::AutonomousLoop`。
//!
//! **Main → Auditor → `continue?` → 回注 feedback** 的无 `max_iterations` 循环：
//! 唯一终止条件是 Auditor 判 `continue:false`、所有任务完成、超时到点，或用户停。
//!
//! Rust 侧与原版的三处载体差异（其余机制逐条照抄）：
//!
//! | 原版 | Rust | 为什么 |
//! |------|------|--------|
//! | `threading.Thread` | `tokio::spawn` | 全栈 tokio（Q3） |
//! | `_stop_event`（`threading.Event`） | `Arc<AtomicBool>` | 同上 |
//! | `chdir_locked()` 改 cwd | 为会话**重建** root=worktree 的注册表 | Rust 的 `FileGuard` 在构建期绑 root，改 cwd 无效（见 `role.rs` 模块头） |

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lingmiao_core::events::{Event, Usage};
use lingmiao_core::{Config, EventBus, LingmiaoError};
use lingmiao_llm::Client;
use lingmiao_memory::{Memory, Zone};
use serde::Deserialize;
use serde_json::{Value, json};

use super::git::AutonomousGit;
use super::meta::{self, MetaPaths, MetaState};
use super::preflight;
use super::report::{self, ReportPaths};
use super::role::{RoleOutput, RoleSet};
use super::state_md::{StateMd, StateMdInput};
use super::store::AutonomousStore;
use super::types::{
    AuditorResult, AutonomousRequest, AutonomousSession, AutonomousTask, FeedbackIssue,
    IterationResult, SessionStatus, TaskSeed,
};

/// Main 产出摘要注入 Auditor 的字符上限（原版 `[:3000]`）。
const AUDITOR_MAIN_SUMMARY_CHARS: usize = 3000;

/// 探针关键词（原版 `_PROBE_KEYWORDS`）：命中即视为「无需实质工作」的存活探针。
const PROBE_KEYWORDS: [&str; 5] = [
    "黑盒探针",
    "无需执行任何任务",
    "立即 Esc 停止",
    "启动后会被立即",
    "仅确认存活",
];

/// 探针 goal 缓存容量（原版 `_goal_cache_max = 50`）。
const GOAL_CACHE_MAX: usize = 50;

/// 一次运行中的会话句柄（`stop()` / `status()` 用）。
struct SessionHandle {
    stop: Arc<AtomicBool>,
}

/// 自动模式编排器。
///
/// 由引擎侧构造一次（持有 store / client / config），跨会话复用；一次
/// [`AutonomousLoop::run_session`] 跑一个会话。
pub struct AutonomousLoop {
    /// 项目根（会话 worktree 从这里分叉）。
    project_root: PathBuf,
    /// 解析后的配置。
    cfg: Config,
    /// 事件总线（TUI 观察同一份流）。
    bus: Arc<EventBus>,
    /// 该角色说话用的 LLM client（Main / Auditor 共用；原版可按角色覆盖）。
    client: Client,
    /// 可选的 chat zone 记忆句柄 —— 用来复用进程内 embedder，并读制度性约束。
    chat_memory: Option<Arc<Memory>>,
    /// `autonomous.db`。
    store: Arc<AutonomousStore>,
    /// 每轮时间预算（`0` = 不限）。
    role_timeout: Duration,
    /// 运行中的会话（`stop()` 靠它找到 `AtomicBool`）。
    running: Mutex<HashMap<String, SessionHandle>>,
    /// MetaLoop / 约束发现的去重状态（两套独立 LRU）。
    meta_state: Mutex<MetaState>,
    /// 探针 goal 缓存（原版 `_goal_cache`，插入序即 LRU）。
    goal_cache: Mutex<Vec<(String, CachedProbe)>>,
    /// 主项目根解析出的布局（状态产物 / 角色记忆区锚点）。
    paths: Arc<lingmiao_core::Paths>,
}

/// 探针缓存的一条。
#[derive(Clone)]
struct CachedProbe {
    orig_id: String,
    branch_name: String,
    worktree_path: String,
}

impl AutonomousLoop {
    /// 构造编排器。
    pub fn new(
        project_root: impl Into<PathBuf>,
        cfg: Config,
        bus: Arc<EventBus>,
        client: Client,
        store: Arc<AutonomousStore>,
        chat_memory: Option<Arc<Memory>>,
    ) -> Self {
        let root = project_root.into();
        Self {
            paths: Arc::new(lingmiao_core::Paths::at(&root)),
            project_root: root,
            cfg,
            bus,
            client,
            chat_memory,
            store,
            role_timeout: Duration::ZERO,
            running: Mutex::new(HashMap::new()),
            meta_state: Mutex::new(MetaState::new()),
            goal_cache: Mutex::new(Vec::new()),
        }
    }

    /// `autonomous.db` 的落点（`<memory_dir>/autonomous.db`）。
    pub fn db_path(paths: &lingmiao_core::Paths) -> PathBuf {
        paths.db("autonomous.db")
    }

    /// 主项目根解析出的路径（状态产物 / 角色记忆区都锚在这里，**不是**会话
    /// worktree）—— 见 `run_session_inner` 的说明。
    pub fn with_paths(mut self, paths: impl Into<PathBuf>) -> Self {
        self.paths = Arc::new(lingmiao_core::Paths::at(paths));
        self
    }

    /// 向 TUI 发一条自动模式提示（`tag`: `message` / `status` / `error`）。
    fn emit(&self, text: impl Into<String>, tag: &str) {
        self.bus.push(Event::AutoNotice {
            tag: tag.to_string(),
            text: text.into(),
            iteration: 0,
        });
    }

    /// 同上，但带上**当前轮次**（cli 2026-10-04「自动模式需要显示轮次，也就是到第
    /// 几轮了」）。TUI 用它把轮次钉在一个**持续可见**的位置（footer 左槽 + 活动
    /// 行），而不是只让「🤖 Main·第N轮 — 开始」随对话流滚走、要回头翻找。
    fn emit_round(&self, text: impl Into<String>, tag: &str, iteration: u64) {
        self.bus.push(Event::AutoNotice {
            tag: tag.to_string(),
            text: text.into(),
            iteration,
        });
    }

    /// 目标简写（原版 `_brief_goal`，40 字符）。
    fn brief_goal(request: &AutonomousRequest, limit: usize) -> String {
        let goal = request
            .goal
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let chars: Vec<char> = goal.chars().collect();
        if chars.len() > limit {
            format!(
                "{}…",
                chars[..limit.saturating_sub(1)].iter().collect::<String>()
            )
        } else {
            goal
        }
    }

    /// 是否是无实质工作的探针目标。
    fn is_noop_probe(goal: &str) -> bool {
        PROBE_KEYWORDS.iter().any(|kw| goal.contains(kw))
    }

    /// 启动一个会话（预检 → worktree → 建会话 → 后台跑）。
    ///
    /// 预检 errors 会**阻断**并返回一条 `error` 会话（原版口径）；warnings 发
    /// 提示但放行。
    pub async fn start(
        &self,
        request: AutonomousRequest,
    ) -> Result<AutonomousSession, LingmiaoError> {
        // ── 预检 ──
        let pre = preflight::run(&self.store, &self.project_root, &self.cfg, &self.client)?;
        for w in &pre.warnings {
            self.emit(format!("⚠️ 预检警告：{w}"), "status");
        }
        if !pre.ok {
            for e in &pre.errors {
                self.emit(format!("❌ 预检错误：{e}"), "error");
            }
            let sid = short_sid();
            let session = AutonomousSession {
                id: sid.clone(),
                request,
                status: SessionStatus::Error,
                error: format!("preflight failed: {}", pre.errors.join("; ")),
                ..AutonomousSession::placeholder()
            };
            self.store.create_session(&session)?;
            return Ok(session);
        }

        // ── 探针缓存 ──
        let goal_key = request
            .goal
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if Self::is_noop_probe(&goal_key) {
            if let Some(cached) = self.lookup_probe(&goal_key) {
                let sid = short_sid();
                let session = AutonomousSession {
                    id: sid.clone(),
                    request,
                    status: SessionStatus::Done,
                    branch_name: cached.branch_name.clone(),
                    worktree_path: cached.worktree_path.clone(),
                    ..AutonomousSession::placeholder()
                };
                self.store.create_session(&session)?;
                self.emit(
                    format!(
                        "⚡ 探针缓存命中 · 跳过工作树/LLM · 原会话 {}",
                        cached.orig_id
                    ),
                    "status",
                );
                return Ok(session);
            }
        }

        // ── worktree ──
        let sid = short_sid();
        let git = AutonomousGit::new(&self.project_root, &sid);
        let mut session = AutonomousSession {
            id: sid.clone(),
            request,
            status: SessionStatus::Pending,
            ..AutonomousSession::placeholder()
        };
        match git.create_worktree() {
            Ok(wt) => {
                session.worktree_path = wt.display().to_string();
                session.branch_name = git.branch_name.clone();
            }
            Err(e) => {
                session.status = SessionStatus::Error;
                session.error = format!("Failed to create worktree: {e}");
                self.store.create_session(&session)?;
                return Ok(session);
            }
        }

        // 报告落**主项目根**（原版在其状态目录的 logs/ 下写 auto-report-*.md）。
        let report_paths = ReportPaths::for_root(&self.project_root, &self.paths);
        session.report_path = report_paths.report_file(&sid).display().to_string();

        // 空任务表 → 用 goal 合成单任务（原版口径）。
        if session.request.tasks.is_empty() {
            session.request.tasks = vec![TaskSeed {
                id: format!("{sid}-t1"),
                seq: 1,
                description: session.request.goal.clone(),
                depends_on: Vec::new(),
            }];
        } else {
            for task in &mut session.request.tasks {
                task.depends_on = task
                    .depends_on
                    .iter()
                    .map(|d| format!("{sid}-{d}"))
                    .collect();
                task.id = format!("{sid}-{}", task.id);
            }
        }

        self.store.create_session(&session)?;
        Ok(session)
    }

    /// 同步跑完一个会话（TUI 侧在后台 task 里调用）。
    pub async fn run_session(
        &self,
        mut session: AutonomousSession,
    ) -> Result<AutonomousSession, LingmiaoError> {
        let stop = Arc::new(AtomicBool::new(false));
        if let Ok(mut m) = self.running.lock() {
            m.insert(session.id.clone(), SessionHandle { stop: stop.clone() });
        }
        let result = self.run_session_inner(&mut session, stop).await;
        if let Ok(mut m) = self.running.lock() {
            m.remove(&session.id);
        }
        result
    }

    /// 停止一个运行中的会话。
    pub fn stop(&self, session_id: &str) -> Value {
        let handle = self
            .running
            .lock()
            .ok()
            .and_then(|m| m.get(session_id).map(|h| h.stop.clone()));
        match handle {
            Some(flag) => {
                flag.store(true, Ordering::SeqCst);
                json!({"session_id": session_id, "status": "stopped"})
            }
            None => json!({"error": "session not found"}),
        }
    }

    /// 会话状态（内存优先，回落 store）。
    pub fn status(&self, session_id: &str) -> Value {
        match self.store.get_session(session_id) {
            Ok(Some(row)) => row,
            _ => json!({"error": "session not found"}),
        }
    }

    /// 会话报告内容。
    pub fn report(&self, session_id: &str) -> Value {
        let Ok(Some(row)) = self.store.get_session(session_id) else {
            return json!({"error": "session not found"});
        };
        let report_path = row
            .get("report_path")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let content = std::fs::read_to_string(&report_path).unwrap_or_default();
        json!({
            "session_id": session_id,
            "status": row.get("status"),
            "report_path": report_path,
            "content": content,
        })
    }

    /// 会话逐轮记录。
    pub fn iterations(&self, session_id: &str) -> Value {
        match self.store.get_iterations(session_id) {
            Ok(rows) => json!({"session_id": session_id, "iterations": rows}),
            Err(e) => json!({"error": e.message()}),
        }
    }

    /// 主循环体。
    async fn run_session_inner(
        &self,
        session: &mut AutonomousSession,
        stop: Arc<AtomicBool>,
    ) -> Result<AutonomousSession, LingmiaoError> {
        session.status = SessionStatus::Running;
        self.store.update_session(session)?;

        let worktree = session.worktree_path.clone();
        // **状态产物落主项目根，不落 worktree**（原版口径）：原版把所有运行期
        // 产物写在主项目根的状态目录下（`_session_log`、`state_md_path`、
        // `report_dir`、trajectories、constraints），而 worktree 只承载 **Main 的
        // 代码改动** —— `git add -A` 于是只提交代码，不会把 sqlite 库 / 日志 /
        // STATE.md 卷进 checkpoint（实测落 worktree 时一次 checkpoint 提交了
        // 十几个 `.memory/*.db` 二进制文件，与「`.memory/` 不进 git」的规范冲突）。
        // 角色记忆区同理是全局的（原版 `agent_memory_paths()`），不按会话分叉。
        let paths = &*self.paths;
        let report_paths = ReportPaths::for_root(&self.project_root, paths);
        let meta_paths = MetaPaths::for_memory_dir(&paths.memory_dir);
        let session_log = report_paths.session_log(&session.id);
        // 先建日志目录再开 append —— 否则开头的 SESSION START / ITER 行会因目录
        // 不存在而静默丢失（目录要到收尾 `generate_report` 才创建，实测日志里
        // 只剩最后一条 SESSION END）。
        if let Some(parent) = session_log.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let log = |msg: &str| {
            use std::io::Write;
            let line = format!("[{}] {msg}\n", super::now_iso());
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&session_log)
            {
                let _ = f.write_all(line.as_bytes());
            }
        };

        log(&format!(
            "SESSION START · goal={}",
            truncate(&session.request.goal, 200)
        ));
        log(&format!("  worktree={}", session.worktree_path));
        self.emit(
            format!(
                "▶ 自动模式启动 · {}",
                Self::brief_goal(&session.request, 40)
            ),
            "message",
        );
        self.emit(format!("📁 工作路径：{}", session.worktree_path), "message");
        self.emit(format!("📋 日志：{}", session_log.display()), "status");

        // 角色运行时：**工具 root 按 worktree 重建**（真隔离，见模块头）；
        // 记忆区仍是全局的（`paths` = 主项目根），与原版 `agent_memory_paths()`
        // 落在全局角色记忆目录下同形。
        let roles = self.build_roles(Path::new(&worktree), paths)?;
        let state_md = StateMd::new(paths.loop_dir.join(format!("loop-state-{}.md", session.id)));
        // git 管理器的 **project_root 必须是主项目根**：它自己会在其下拼
        // `auto-sessions/<sid>`。传 worktree 会得到 `<worktree>/auto-sessions/<sid>`
        // —— 那个目录不存在，`checkpoint()` 于是每次都返回 false（实测日志里
        // 「ITER checkpoint failed」而 `git status` 明明有改动）。
        let git = AutonomousGit::new(&self.project_root, &session.id);

        let lessons: Vec<String> = Vec::new();
        let start = Instant::now();
        let mut auditor_feedback = String::new();
        let mut unresolved_issues: Vec<FeedbackIssue> = Vec::new();

        let outcome: Result<(), LingmiaoError> = async {
            loop {
                if stop.load(Ordering::SeqCst) || session.status == SessionStatus::Stopped {
                    session.status = SessionStatus::Stopped;
                    log("SESSION STOPPED by user");
                    return Ok(());
                }
                if session.request.time_limit_minutes > 0
                    && start.elapsed().as_secs() / 60 >= session.request.time_limit_minutes
                {
                    log("SESSION time limit reached");
                    return Ok(());
                }

                session.current_iteration += 1;

                let task = {
                    let tasks = self.store.get_tasks(&session.id)?;
                    match pick_next_task(&tasks) {
                        Some(t) => t,
                        None => {
                            log("SESSION all tasks done");
                            return Ok(());
                        }
                    }
                };
                session.current_task_id = task["id"].as_str().unwrap_or("").to_string();
                self.store.update_session(session)?;

                // STATE.md 每轮刷新（跨轮共享状态）。
                {
                    let tasks = self.store.get_tasks(&session.id)?;
                    self.update_state_md(&state_md, session, &tasks, Some(&task), &lessons)?;
                }

                let mut it = IterationResult {
                    iteration: session.current_iteration,
                    task_id: session.current_task_id.clone(),
                    ..Default::default()
                };

                // ── Main ──
                log(&format!(
                    "ITER {} MAIN start · task={}",
                    it.iteration, it.task_id
                ));
                self.emit_round(
                    format!("🤖 Main·第{}轮 — 开始", session.current_iteration),
                    "message",
                    session.current_iteration,
                );
                let main_prompt = self.build_main_prompt(
                    session,
                    &state_md,
                    &task,
                    &auditor_feedback,
                    &unresolved_issues,
                    &meta_paths,
                );
                let main_out = roles.main.run(&main_prompt).await;
                it.main_output = role_output_json(&main_out);
                log(&format!(
                    "ITER {} MAIN done · tokens={:?}",
                    it.iteration, main_out.usage
                ));

                if stop.load(Ordering::SeqCst) {
                    session.status = SessionStatus::Stopped;
                    log("SESSION STOPPED by user (mid-iteration)");
                    return Ok(());
                }

                // ── checkpoint（Auditor 前留一个干净快照）──
                if !git.checkpoint(&format!("before auditor on task {}", task["id"]))? {
                    log("ITER checkpoint failed — continuing anyway");
                }

                // ── Auditor ──
                log(&format!("ITER {} AUDITOR start", it.iteration));
                self.emit_round(
                    format!("🔍 Auditor·第{}轮 — 开始", session.current_iteration),
                    "message",
                    session.current_iteration,
                );
                let auditor_prompt =
                    self.build_auditor_prompt(session, &state_md, &task, &main_out);
                let auditor_out = roles.auditor.run(&auditor_prompt).await;
                it.auditor_output = role_output_json(&auditor_out);
                let verdict = parse_auditor_result(&auditor_out);
                it.auditor_result = verdict.clone();
                log(&format!(
                    "ITER {} AUDITOR done · continue={}",
                    it.iteration, verdict.continue_
                ));
                self.emit_round(
                    format!(
                        "🔍 Auditor·第{}轮｜continue={}｜{}",
                        it.iteration,
                        verdict.continue_,
                        truncate(&verdict.feedback, 120)
                    ),
                    "status",
                    it.iteration,
                );

                if verdict.continue_ {
                    auditor_feedback = verdict.feedback.clone();
                    unresolved_issues.extend(verdict.issues.iter().cloned());
                    self.emit_round(
                        format!("➡️ 第{}轮未完成，反馈已注入下一轮 Main", it.iteration),
                        "status",
                        it.iteration,
                    );
                } else {
                    self.mark_task_done(&session.id, &task, "auditor verified — all criteria met")?;
                    auditor_feedback.clear();
                    unresolved_issues.clear();
                }

                session.consecutive_errors = 0;
                self.store.record_iteration(&session.id, &it)?;
                self.store.update_session(session)?;
                report::record_trajectory(&report_paths, session, &it)?;
                // 任务 `done` 后立即刷新 STATE.md：否则 Plan 里仍写 `[ ]`、
                // Completed Tasks 仍写 `None`（下一轮若还有任务，模型会看到
                // 自相矛盾的状态）。收尾再刷一次，把终态写进文件。
                {
                    let tasks = self.store.get_tasks(&session.id)?;
                    self.update_state_md(&state_md, session, &tasks, None, &lessons)?;
                }

                if session.request.interval_seconds > 0 {
                    tokio::time::sleep(Duration::from_secs(session.request.interval_seconds)).await;
                }
            }
        }
        .await;

        if let Err(e) = outcome {
            session.consecutive_errors += 1;
            session.status = SessionStatus::Error;
            session.error = e.message().to_string();
            log(&format!("SESSION ERROR: {}", e.message()));
        }

        // ── 收尾 ──
        if session.status != SessionStatus::Error && session.status != SessionStatus::Stopped {
            session.status = SessionStatus::Done;
        }
        self.store.update_session(session)?;
        report::generate_report(&report_paths, &self.store, session, &lessons)?;

        // Phase 3：事后分析（两个**独立**去重集，见 meta.rs 模块头）。
        if let Ok(mut st) = self.meta_state.lock() {
            if let Err(e) = meta::run_meta_loop(&mut st, &meta_paths, &session.id) {
                tracing::warn!("MetaLoop failed for session {}: {e}", session.id);
            }
            if let Err(e) = meta::discover_constraints(&mut st, &meta_paths, &session.id) {
                tracing::warn!(
                    "Constraint discovery failed for session {}: {e}",
                    session.id
                );
            }
        }

        log(&format!(
            "SESSION END · status={} · iterations={}",
            session.status.as_str(),
            session.current_iteration
        ));
        self.emit(
            report::status_notice(session.status, &Self::brief_goal(&session.request, 40)),
            if session.status == SessionStatus::Done {
                "message"
            } else {
                "status"
            },
        );
        if session.status == SessionStatus::Error {
            if let Some(first) = session.error.lines().next() {
                self.emit(format!("📌 {first}"), "status");
            }
            self.emit(format!("📋 详情：{}", session_log.display()), "status");
        }

        // 探针结果入缓存（原版 `_goal_cache`）。
        let goal_key = session
            .request
            .goal
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if Self::is_noop_probe(&goal_key) {
            self.store_probe(
                &goal_key,
                CachedProbe {
                    orig_id: session.id.clone(),
                    branch_name: session.branch_name.clone(),
                    worktree_path: session.worktree_path.clone(),
                },
            );
        }

        Ok(session.clone())
    }

    // ── 组装 ──────────────────────────────────────────

    /// 为会话组装两个角色（root=worktree 的**真隔离**）。
    fn build_roles(
        &self,
        work_root: &Path,
        paths: &lingmiao_core::Paths,
    ) -> Result<RoleSet, LingmiaoError> {
        // 工具层按 worktree 重建 —— FileGuard 在构建期绑 root，改 cwd 无效。
        let reranker: Option<Arc<dyn lingmiao_tools::Reranker>> = Some(Arc::new(
            crate::rerank::LlmReranker::new(self.client.clone()),
        ));
        // 角色记忆区（`main` / `auditor`）；复用进程内 embedder（不重载 85 MB 模型）。
        let embedder = self.chat_memory.as_ref().and_then(|m| m.embedder());
        let main_memory = Memory::open(paths, Zone::Main, embedder.clone())
            .map(Arc::new)
            .map_err(|e| {
                tracing::warn!("auto: main zone unavailable, continuing without role memory: {e}");
                e
            })
            .ok();
        let auditor_memory = Memory::open(paths, Zone::Auditor, embedder)
            .map(Arc::new)
            .map_err(|e| {
                tracing::warn!("auto: auditor zone unavailable: {e}");
                e
            })
            .ok();
        // 注册表按 worktree root 重建（两个角色共用同一个注册表 —— Auditor 的
        // 只读性由**角色白名单**保证，见 role.rs 模块头差异①）。
        let registry = match &main_memory {
            Some(m) => lingmiao_tools::full_registry(work_root, m.clone(), &self.cfg, reranker),
            None => lingmiao_tools::default_registry(work_root),
        };
        // 角色记忆句柄只用于「制度性约束」读取等只读用途；工具层已在 registry
        // 里绑定了自己的 Memory（与角色 zone 无关）。
        Ok(RoleSet::new(
            self.cfg.clone(),
            self.bus.clone(),
            self.client.clone(),
            work_root,
            Arc::new(registry),
            main_memory,
            auditor_memory,
            self.role_timeout,
        ))
    }

    /// Main 角色消息（原版 `_run_main` 的 user 组装，逐块照抄）。
    fn build_main_prompt(
        &self,
        session: &AutonomousSession,
        state_md: &StateMd,
        current_task: &Value,
        auditor_feedback: &str,
        unresolved_issues: &[FeedbackIssue],
        meta_paths: &MetaPaths,
    ) -> String {
        let success = if session.request.success_criteria.is_empty() {
            "未显式指定 — 请从总目标中推断必须达成的结果，逐条产出。\n\n\
             ⚠️ 你必须使用 write_file/edit/bash 等工具实际创建/修改文件。不能只输出计划——计划不算产出。每个产出必须有对应的文件改动，配置修改不算实质性产出。\n\n\
             ⚠️ 离线模式：用户不在场。遇到需要决策的问题时：\n\
             - 用 search_memory 查用户之前的 decision/preference\n\
             - 可逆的工程选择（命名/文件结构/实现方式）→ 自己拍板，不要等\n\
             - 只有不可逆方向决策且无历史依据时 → 记录但不阻塞当前任务\n\
             - 禁止说「等待用户确认」「需要用户拍板」— 用户不在这里"
                .to_string()
        } else {
            session
                .request
                .success_criteria
                .iter()
                .map(|c| format!("- {c}"))
                .collect::<Vec<_>>()
                .join("\n")
        };

        let mut feedback_block = String::new();
        if !auditor_feedback.is_empty() {
            feedback_block.push_str(&format!(
                "\n\n# Auditor 上轮反馈（必须逐条处理）\n{auditor_feedback}"
            ));
        }
        if !unresolved_issues.is_empty() {
            feedback_block.push_str("\n\n# Auditor 结构化问题清单（逐条处理，处理完标记 ✅）");
            for (i, iss) in unresolved_issues.iter().enumerate() {
                feedback_block.push_str(&format!("\n\n## Issue {}: [{}]", i + 1, iss.category));
                if !iss.file.is_empty() {
                    feedback_block.push_str(&format!("\n- 文件：`{}`", iss.file));
                }
                feedback_block.push_str(&format!("\n- 问题：{}", iss.description));
                if !iss.suggested_fix.is_empty() {
                    feedback_block.push_str(&format!("\n- 建议修复：{}", iss.suggested_fix));
                }
            }
        }
        feedback_block.push_str(&meta::meta_improvements_block(meta_paths));
        feedback_block.push_str(&meta::auto_constraints_block(meta_paths));
        feedback_block.push_str(&self.institutional_constraints_block());

        format!(
            "# 总目标\n{}\n\n# 当前任务\n{}\n\n# 成功标准\n{}\n\n# 约束\n{}\n\n# 共享状态 (STATE.md)\n{}{}",
            session.request.goal,
            current_task["description"].as_str().unwrap_or(""),
            success,
            if session.request.constraints.is_empty() {
                "无"
            } else {
                &session.request.constraints
            },
            state_md.read(),
            feedback_block
        )
    }

    /// Auditor 角色消息（原版 `_run_auditor` 的 user 组装，逐块照抄）。
    fn build_auditor_prompt(
        &self,
        session: &AutonomousSession,
        state_md: &StateMd,
        current_task: &Value,
        main_out: &RoleOutput,
    ) -> String {
        let (success, instruction) = if session.request.success_criteria.is_empty() {
            (
                "未显式指定。你必须从总目标和 Main Agent 产出中自主提取验收标准。".to_string(),
                AUDITOR_INSTRUCTION_IMPLICIT.to_string(),
            )
        } else {
            (
                session
                    .request
                    .success_criteria
                    .iter()
                    .map(|c| format!("- {c}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
                AUDITOR_INSTRUCTION_EXPLICIT.to_string(),
            )
        };
        format!(
            "# 总目标\n{}\n\n# 成功标准（每个都必须验证）\n{}\n\n# 当前任务\n{}\n\n# Main Agent 本轮产出摘要\n{}\n\n# 共享状态 (STATE.md)\n{}\n\n# 指令\n{}",
            session.request.goal,
            success,
            current_task["description"].as_str().unwrap_or(""),
            truncate(&main_out.response, AUDITOR_MAIN_SUMMARY_CHARS),
            state_md.read(),
            instruction
        )
    }

    /// 制度性约束（原版 `_load_institutional_constraints`）：chat 区
    /// `kind=constraint` 观测，最新 10 条，反转使最新在尾部。
    fn institutional_constraints_block(&self) -> String {
        let Some(m) = &self.chat_memory else {
            return String::new();
        };
        let Ok(rows) = m.observations.by_kind("constraint", 10) else {
            return String::new();
        };
        if rows.is_empty() {
            return String::new();
        }
        let mut out = String::from("\n\n# 📏 规范约束（强制执行的行为规则）\n");
        // `by_kind` 是 newest-first；反转让最新落在尾部（recency）。
        for row in rows.iter().rev() {
            let name = row.name.trim();
            let content = row.content.split_whitespace().collect::<Vec<_>>().join(" ");
            if !name.is_empty() && !content.is_empty() {
                out.push_str(&format!("\n- **{name}**: {}", truncate(&content, 300)));
            }
        }
        out
    }

    // ── 小工具 ────────────────────────────────────────

    fn lookup_probe(&self, key: &str) -> Option<CachedProbe> {
        self.goal_cache
            .lock()
            .ok()?
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
    }

    fn store_probe(&self, key: &str, value: CachedProbe) {
        if let Ok(mut cache) = self.goal_cache.lock() {
            if let Some(slot) = cache.iter_mut().find(|(k, _)| k == key) {
                slot.1 = value;
            } else {
                cache.push((key.to_string(), value));
            }
            while cache.len() > GOAL_CACHE_MAX {
                cache.remove(0);
            }
        }
    }

    fn update_state_md(
        &self,
        state_md: &StateMd,
        session: &AutonomousSession,
        tasks: &[Value],
        current_task: Option<&Value>,
        lessons: &[String],
    ) -> Result<(), LingmiaoError> {
        state_md
            .write(&StateMdInput {
                goal: &session.request.goal,
                plan: tasks,
                current_task,
                lessons,
                constraints: &session.request.constraints,
                increments: &format!("第{}轮进行中", session.current_iteration),
            })
            .map_err(|e| LingmiaoError::memory(super::STORE, format!("write STATE.md: {e}")))
    }

    fn mark_task_done(
        &self,
        session_id: &str,
        task: &Value,
        result: &str,
    ) -> Result<(), LingmiaoError> {
        let t = AutonomousTask {
            id: task["id"].as_str().unwrap_or("").to_string(),
            seq: task["seq"].as_u64().unwrap_or(0),
            description: task["description"].as_str().unwrap_or("").to_string(),
            status: "done".to_string(),
            depends_on: Vec::new(),
            result: result.to_string(),
        };
        self.store.update_task(session_id, &t)
    }

    /// 阻塞式跑一个会话（供 `/round` 等同步调用点 / 测试）。
    pub async fn run_blocking(
        &self,
        request: AutonomousRequest,
    ) -> Result<AutonomousSession, LingmiaoError> {
        let session = self.start(request).await?;
        if session.status == SessionStatus::Error || session.status == SessionStatus::Done {
            return Ok(session);
        }
        self.run_session(session).await
    }

    /// 单轮模式（`/round <目标>`）— 移植原版 `run_single_round`。
    ///
    /// Main 产出 → Auditor 核验；`continue:true` 就把 feedback 回注 Main 再来
    /// 一次，直到 Auditor 通过或撞上重试上限（原版 `max_retries = 20`）。
    /// **不建 worktree、不落库**：全部在当前项目里同步跑（原版口径）。
    ///
    /// 收尾写两份产物（与原版一致）：`var/notes/{ts}_round_handoff.md`（人读
    /// 交接单）与 `<memory>/trajectories/round-{id}.jsonl`（机读轨迹，供
    /// MetaLoop / 约束发现跨模式分析）。
    pub async fn run_single_round(&self, goal: &str) -> Result<RoundOutcome, LingmiaoError> {
        let paths = &*self.paths;
        let roles = self.build_roles(&self.project_root, paths)?;

        let round_id = short_sid();
        let task = json!({
            "id": format!("round-{round_id}-t1"),
            "seq": 1,
            "description": goal,
            "status": "pending",
            "depends_on": "[]",
            "result": "",
        });
        let state_md = StateMd::new(paths.tmp_dir.join(format!("round-state-{round_id}.md")));
        state_md
            .write(&StateMdInput {
                goal,
                plan: std::slice::from_ref(&task),
                current_task: Some(&task),
                lessons: &[],
                constraints: "",
                increments: "单轮模式",
            })
            .map_err(|e| LingmiaoError::memory(super::STORE, format!("write round state: {e}")))?;

        let request = parse_request(goal);
        let session = AutonomousSession {
            id: format!("round-{round_id}"),
            request,
            ..AutonomousSession::placeholder()
        };

        self.emit(
            format!("🎯 单轮审计启动 · {}", truncate(goal, 80)),
            "message",
        );

        let max_retries = 20u64;
        let mut retry_count = 0u64;
        let mut auditor_feedback = String::new();
        let mut main_out = RoleOutput::default();
        let mut auditor_out = RoleOutput::default();
        let mut verdict: Option<AuditorResult> = None;

        while retry_count <= max_retries {
            retry_count += 1;
            if retry_count > 1 {
                self.emit_round(
                    format!("🔄 单轮审计·第{retry_count}次重试 — Auditor 不通过，反馈已注入 Main"),
                    "status",
                    retry_count,
                );
            }
            let label = if retry_count > 1 {
                format!("单轮·第{retry_count}次")
            } else {
                "单轮".to_string()
            };

            self.emit_round(format!("🤖 Main·{label} — 开始"), "message", retry_count);
            let main_prompt = self.build_main_prompt(
                &session,
                &state_md,
                &task,
                &auditor_feedback,
                &[],
                &MetaPaths::for_memory_dir(&paths.memory_dir),
            );
            main_out = roles.main.run(&main_prompt).await;
            self.emit(format!("🤖 Main·{label} — 完成"), "message");

            self.emit_round(format!("🔍 Auditor·{label} — 开始"), "message", retry_count);
            let auditor_prompt = self.build_auditor_prompt(&session, &state_md, &task, &main_out);
            auditor_out = roles.auditor.run(&auditor_prompt).await;
            let v = parse_auditor_result(&auditor_out);
            self.emit_round(
                format!(
                    "🔍 Auditor·{label}｜continue={}｜{}",
                    v.continue_,
                    truncate(&v.feedback, 120)
                ),
                "status",
                retry_count,
            );
            let passed = !v.continue_;
            auditor_feedback = v.feedback.clone();
            verdict = Some(v);
            if passed {
                break;
            }
        }

        let _ = std::fs::remove_file(state_md.path());

        let outcome = RoundOutcome {
            goal: goal.to_string(),
            retries: retry_count,
            main_response: main_out.response.clone(),
            auditor_response: auditor_out.response.clone(),
            passed: verdict.as_ref().map(|v| !v.continue_).unwrap_or(false),
            feedback: verdict
                .as_ref()
                .map(|v| v.feedback.clone())
                .unwrap_or_default(),
            main_tokens: usage_json(&main_out.usage),
            auditor_tokens: usage_json(&auditor_out.usage),
        };

        // 收尾产物：交接单 + 轨迹（原版 `_write_round_handoff` / `_record_round_trajectory`）。
        let _ = self.write_round_handoff(&outcome);
        let _ = self.record_round_trajectory(&outcome);

        self.emit(
            if outcome.passed {
                if outcome.retries == 1 {
                    "✅ 单轮审计完成 — Auditor 判定：通过".to_string()
                } else {
                    format!("✅ 单轮审计完成 — 第{}次尝试通过 Auditor", outcome.retries)
                }
            } else {
                format!("⚠️ 单轮审计 — 已达最大重试次数({max_retries})，Auditor 仍未通过")
            },
            "message",
        );
        Ok(outcome)
    }

    /// `/round` 的人读交接单（原版 `_write_round_handoff`）。
    fn write_round_handoff(&self, o: &RoundOutcome) -> Result<(), LingmiaoError> {
        let notes_dir = self.project_root.join("var").join("notes");
        std::fs::create_dir_all(&notes_dir)
            .map_err(|e| LingmiaoError::memory(super::STORE, format!("mkdir var/notes: {e}")))?;
        let ts = super::now_iso().replace([':', '-'], "").replace('T', "_");
        let file = notes_dir.join(format!("{ts}_round_handoff.md"));
        let status = if o.passed {
            if o.retries == 1 {
                "✅ 通过".to_string()
            } else {
                format!("✅ 通过（第{}次）", o.retries)
            }
        } else {
            format!("⚠️ 未通过（已达最大重试 {}）", o.retries)
        };
        let body = format!(
            "# /round 会话 · {ts}\n\n- **状态**: {status}\n- **重试次数**: {}\n- **目标**: {}\n\n## Auditor 判定\n\n- 通过: {}\n- feedback: {}\n\n## Main 产出摘要\n\n{}\n\n## Auditor 产出摘要\n\n{}\n\n## Token 用量\n\n- Main: {}\n- Auditor: {}\n",
            o.retries,
            truncate(&o.goal, 200),
            o.passed,
            truncate(&o.feedback, 500),
            report::summarize_output(&o.main_response),
            report::summarize_output(&o.auditor_response),
            o.main_tokens,
            o.auditor_tokens,
        );
        std::fs::write(&file, body)
            .map_err(|e| LingmiaoError::memory(super::STORE, format!("write round handoff: {e}")))
    }

    /// `/round` 的机读轨迹（原版 `_record_round_trajectory`，与 auto 同 schema）。
    fn record_round_trajectory(&self, o: &RoundOutcome) -> Result<(), LingmiaoError> {
        let dir = self.paths.memory_dir.join("trajectories");
        std::fs::create_dir_all(&dir)
            .map_err(|e| LingmiaoError::memory(super::STORE, format!("mkdir trajectories: {e}")))?;
        let sid = format!("round-{}", short_sid());
        let ts = super::now_iso();
        let mk = |event: &str| {
            json!({"ts": ts, "session_id": sid, "iteration": 1,
                   "task_id": format!("{sid}-t1"), "event": event})
        };
        let mut main_end = mk("MAIN_END");
        main_end["summary"] = json!(report::summarize_output(&o.main_response));
        main_end["tokens"] = o.main_tokens.clone();
        let mut auditor_end = mk("AUDITOR_END");
        auditor_end["summary"] = json!(report::summarize_output(&o.auditor_response));
        auditor_end["continue"] = json!(!o.passed);
        auditor_end["feedback"] = json!(truncate(&o.feedback, 500));
        let mut round_end = mk("ROUND_END");
        round_end["goal"] = json!(truncate(&o.goal, 300));
        round_end["retries"] = json!(o.retries);
        round_end["mode"] = json!("round");
        let body = [
            mk("MAIN_START"),
            main_end,
            mk("AUDITOR_START"),
            auditor_end,
            round_end,
        ]
        .iter()
        .map(|v| serde_json::to_string(v).unwrap_or_else(|_| "{}".into()))
        .collect::<Vec<_>>()
        .join("\n");
        std::fs::write(dir.join(format!("{sid}.jsonl")), body).map_err(|e| {
            LingmiaoError::memory(super::STORE, format!("write round trajectory: {e}"))
        })
    }

    /// 打开会话对应的 store（供调用点）。
    pub fn store(&self) -> &Arc<AutonomousStore> {
        &self.store
    }
}

/// `/round` 一次单轮审计的结果（原版 `run_single_round` 的返回 dict）。
#[derive(Debug, Clone, Default)]
pub struct RoundOutcome {
    /// 目标原文。
    pub goal: String,
    /// 实际重试次数（1 = 一次通过）。
    pub retries: u64,
    /// Main 的最终回复。
    pub main_response: String,
    /// Auditor 的最终回复。
    pub auditor_response: String,
    /// Auditor 是否判定通过（`continue:false`）。
    pub passed: bool,
    /// 最后一次 Auditor feedback。
    pub feedback: String,
    /// Main 的 token 用量。
    pub main_tokens: Value,
    /// Auditor 的 token 用量。
    pub auditor_tokens: Value,
}

/// Auditor 指令块（显式成功标准版，逐条照抄原版）。
const AUDITOR_INSTRUCTION_EXPLICIT: &str = "0. 先做方向对齐：对照总目标和成功标准，判断 Main 当前方向是否符合用户输入和预期。方向走偏（做了用户没要求的东西、或没做用户要求的核心产出）→ continue=true 并拉回正轨\n\
1. 逐一解析以上成功标准，为每个标准设计验收测试\n\
2. 用工具实际执行测试（read_file/bash/grep 等）\n\
3. 执行反事实检查：如果回退 Main 的改动（git diff HEAD~1），当前状态是否也能满足目标？改动是否真的必要？\n\
4. 输出 JSON 判定：continue=true（仍有缺 / 证据不足 / 幻觉修复 / 方向偏离）或 continue=false（三层验证全部通过）\n\
5. 每个结论必须有工具输出证据";

/// Auditor 指令块（成功标准缺省版，逐条照抄原版）。
const AUDITOR_INSTRUCTION_IMPLICIT: &str = "0. 先用 git diff --stat HEAD 和 list_directory 检查工作路径，确认 Main Agent 实际产出了什么文件\n\
1. 从总目标中提取可测需求，为每个需求设计 ≥1 个验收测试\n\
2. 用工具实际执行测试（read_file/bash/grep 等）\n\
3. 验证 Main Agent 是否完成了总目标要求的实际产出\n\
4. 严格区分「环境搭建」和「实质性产出」：\n\
   - 配置修改（.json/.gitignore）、git hooks、路径修正、格式调整 = 环境搭建，不算完成\n\
   - 只有总目标要求的代码改动/新功能/优化/重构才算实质性产出\n\
5. 输出 JSON 判定：continue=true（产出不完整或质量不达标）或 continue=false（总目标完全达成）\n\
6. 每个结论必须有工具输出证据（git diff --stat、read_file 结果等）\n\
7. IMPORTANT：\n\
   - 第 1-2 轮默认 continue=true\n\
   - continue=false 仅当 Main Agent 产出了与总目标直接相关的实质性代码改动\n\
   - 如果只有配置/环境修改，即使看起来完整也必须 continue=true\n\
8. 反事实检查（防幻觉修复）：在判定 continue=false 之前，必须回答：\n\
   - 如果回退 Main 的所有改动（git stash 或 git diff HEAD~1 查看），当前状态是否也能满足目标？\n\
   - Main 的改动是否真的必要？有没有在「修复不存在的问题」？\n\
   - 如果改动无实际价值（仅格式/重命名/空壳），即使看起来完整也必须 continue=true\n\
9. ⚠️ 掌舵人职责 — 你的 feedback 就是 Main 下一轮的提示词：\n\
   - 先做方向对齐：对照总目标和成功标准，判断 Main 当前方向是否符合用户输入和预期。方向走偏比细节瑕疵更严重，必须 continue=true 并给出拉回正轨的具体指令。\n\
   - 不只验收操作有效性，还要评估设计质量：是否有更优的架构/接口/命名选择？设计平庸但功能可用时，给出设计级改进建议（方案+理由+涉及文件）。\n\
   - feedback 必须具体：包含文件路径、行号、要改什么、怎么改\n\
   - 不要写「修复错误处理」— 写「在 app.py 第 342 行加 try/except」\n\
   - 用 search_memory 检索用户的历史决策和偏好，融入 feedback\n\
10. ⚠️ 离线模式 — 当 Main 说「需要用户拍板」「等待确认」时：\n\
   - 步骤 1：用 search_memory 搜索 chat 分区的 decision/preference\n\
   - 步骤 2：找到了 → feedback 中直接告诉 Main：「用户已决定 X，按此执行」\n\
   - 步骤 3：找不到但属于可逆工程选择（命名/文件结构/实现方式）→ 你直接拍板\n\
   - 步骤 4：只有真正的不可逆方向决策才记录为待办，同时指定替代产出\n\
   - 反例：Main 说「4 个决定待拍板」→ 你说「请处理」→ 死循环 ❌\n\
   - 正例：Main 说「4 个决定待拍板」→ 你用 search_memory 查历史 → 直接拍板 3 个 + 记录 1 个 → continue=true 继续推进 ✅";

/// 任务调度（原版 `_pick_next_task`）：seq 最小、`pending`、依赖全 done。
pub fn pick_next_task(tasks: &[Value]) -> Option<Value> {
    let done: Vec<&str> = tasks
        .iter()
        .filter(|t| t["status"] == "done")
        .filter_map(|t| t["id"].as_str())
        .collect();
    let mut pending: Vec<&Value> = tasks.iter().filter(|t| t["status"] == "pending").collect();
    pending.sort_by_key(|t| t["seq"].as_u64().unwrap_or(0));
    pending
        .into_iter()
        .find(|t| {
            let deps: Vec<String> = t["depends_on"]
                .as_str()
                .and_then(|s| serde_json::from_str::<Vec<String>>(s).ok())
                .unwrap_or_default();
            deps.iter().all(|d| done.contains(&d.as_str()))
        })
        .cloned()
}

/// 角色产出 → 归档用的 `{"response", "tokens"}` 形状（原版口径）。
fn role_output_json(out: &RoleOutput) -> Value {
    json!({
        "response": out.response,
        "tokens": usage_json(&out.usage),
        "fault": out.fault,
    })
}

/// `Usage` → JSON。
fn usage_json(u: &Usage) -> Value {
    json!({
        "input_tokens": u.input_tokens,
        "output_tokens": u.output_tokens,
        "total_tokens": u.input_tokens + u.output_tokens,
    })
}

/// 从角色回复文本里提取 Auditor 判定（原版 `_extract_json_object` +
/// `_parse_auditor_result`）。
///
/// **取最后一个含 `continue` 键的对象** —— 审计经常在 JSON 前后夹带散文与
/// 代码片段，贪婪 `{.*}` 会从第一个花括号抓到最后一个导致解析失败（原版实测
/// 多次退化成「unparseable → 默认 continue」）。
pub fn parse_auditor_result(out: &RoleOutput) -> AuditorResult {
    let data = extract_json_object(&out.response);
    let Some(data) = data else {
        // 角色 fault（超时 / 传输错误）与「JSON 无法解析」都按 continue=true 处理
        // —— 不阻塞主循环（原版口径）。
        let reason = if out.fault.is_empty() {
            "Auditor output unparseable — defaulting to continue"
        } else {
            "Auditor run faulted — defaulting to continue"
        };
        return AuditorResult {
            continue_: true,
            feedback: reason.to_string(),
            fault: out.fault.clone(),
            ..Default::default()
        };
    };
    let issues: Vec<FeedbackIssue> = data
        .get("issues")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|ri| {
                    ri.as_object()?;
                    let s = |k: &str| ri.get(k).and_then(Value::as_str).unwrap_or("").to_string();
                    Some(FeedbackIssue {
                        category: s("category"),
                        file: s("file"),
                        line_hint: s("line_hint"),
                        description: s("description"),
                        suggested_fix: s("suggested_fix"),
                        resolved: ri.get("resolved").and_then(Value::as_bool).unwrap_or(false),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    AuditorResult {
        continue_: data
            .get("continue")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        feedback: data
            .get("feedback")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        evidence: data
            .get("evidence")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        counterfactual: data.get("counterfactual").cloned().unwrap_or(Value::Null),
        issues,
        fault: out.fault.clone(),
    }
}

/// 从自由文本里抽出 JSON 对象（原版 `_extract_json_object`）。
pub fn extract_json_object(text: &str) -> Option<Value> {
    let mut candidates: Vec<Value> = Vec::new();
    for (i, _) in text.match_indices('{') {
        let mut de = serde_json::Deserializer::from_str(&text[i..]);
        if let Ok(v) = Value::deserialize(&mut de) {
            if v.is_object() {
                candidates.push(v);
            }
        }
    }
    if candidates.is_empty() {
        return None;
    }
    for obj in candidates.iter().rev() {
        if obj.get("continue").is_some() {
            return Some(obj.clone());
        }
    }
    candidates.pop()
}

/// 12 位十六进制会话 id（原版 `uuid.uuid4().hex[:12]`）。
fn short_sid() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id() as u128;
    let mix = nanos ^ (pid << 64);
    format!("{:012x}", mix & 0xffff_ffff_ffff)
}

/// 按字符截断。
fn truncate(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// 请求解析（原版 `parse_request`）：Markdown front-matter 支持。
pub fn parse_request(text: &str) -> AutonomousRequest {
    let mut request = AutonomousRequest {
        goal: text.to_string(),
        ..Default::default()
    };
    if !text.starts_with("---") {
        return request;
    }
    let parts: Vec<&str> = text.splitn(3, "---").collect();
    if parts.len() < 3 {
        return request;
    }
    let frontmatter = parts[1].trim();
    let body = parts[2].trim();
    let mut goal = String::new();
    let mut success: Vec<String> = Vec::new();
    let mut interval = 5u64;
    let mut limit = 0u64;
    for line in frontmatter.lines() {
        let Some((key, val)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let val = val.trim();
        if val.starts_with('[') && val.ends_with(']') {
            let list: Vec<String> = val[1..val.len() - 1]
                .split(',')
                .map(|v| v.trim().trim_matches('"').trim_matches('\'').to_string())
                .filter(|v| !v.is_empty())
                .collect();
            if key == "success_criteria" {
                success = list;
            }
            continue;
        }
        match key {
            "goal" => goal = val.to_string(),
            "time_limit_minutes" => limit = val.parse().unwrap_or(0),
            "interval_seconds" => interval = val.parse().unwrap_or(5),
            _ => {}
        }
    }
    request.goal = if goal.is_empty() {
        body.to_string()
    } else {
        goal
    };
    request.success_criteria = success;
    request.interval_seconds = interval;
    request.time_limit_minutes = limit;
    request.constraints = body.to_string();
    request
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pick_next_task_respects_seq_and_dependencies() {
        let tasks = vec![
            json!({"id": "s-t2", "seq": 2, "status": "pending", "depends_on": "[\"s-t1\"]"}),
            json!({"id": "s-t1", "seq": 1, "status": "pending", "depends_on": "[]"}),
        ];
        let t = pick_next_task(&tasks).unwrap();
        assert_eq!(t["id"], "s-t1");
        // t1 done → t2 becomes eligible.
        let tasks = vec![
            json!({"id": "s-t2", "seq": 2, "status": "pending", "depends_on": "[\"s-t1\"]"}),
            json!({"id": "s-t1", "seq": 1, "status": "done", "depends_on": "[]"}),
        ];
        assert_eq!(pick_next_task(&tasks).unwrap()["id"], "s-t2");
        // nothing pending → None.
        let done = vec![json!({"id": "s-t1", "seq": 1, "status": "done", "depends_on": "[]"})];
        assert!(pick_next_task(&done).is_none());
    }

    #[test]
    fn json_extraction_prefers_the_last_object_carrying_continue() {
        let text = "先看 {\"fixed\": true} 这个片段。\n\n## 判定\n\
                    ```json\n{\"continue\": true, \"feedback\": \"再改\"}\n```\n";
        let v = extract_json_object(text).unwrap();
        assert_eq!(v["continue"], true);
        assert_eq!(v["feedback"], "再改");
    }

    #[test]
    fn unparseable_auditor_output_defaults_to_continue() {
        let out = RoleOutput {
            response: "没有 JSON".to_string(),
            ..Default::default()
        };
        let r = parse_auditor_result(&out);
        assert!(r.continue_);
        assert!(r.feedback.contains("unparseable"));
    }

    #[test]
    fn auditor_result_parses_structured_issues() {
        let out = RoleOutput {
            response: "{\"continue\": true, \"feedback\": \"x\", \"issues\": [{\"category\": \"quality\", \"file\": \"a.rs\", \"description\": \"d\", \"suggested_fix\": \"f\"}]}".to_string(),
            ..Default::default()
        };
        let r = parse_auditor_result(&out);
        assert_eq!(r.issues.len(), 1);
        assert_eq!(r.issues[0].category, "quality");
        assert_eq!(r.issues[0].file, "a.rs");
    }

    #[test]
    fn request_parsing_understands_frontmatter() {
        let req = parse_request(
            "---\ngoal: 把事做完\ntime_limit_minutes: 30\ninterval_seconds: 2\nsuccess_criteria: [\"a\", \"b\"]\n---\n约束正文",
        );
        assert_eq!(req.goal, "把事做完");
        assert_eq!(req.time_limit_minutes, 30);
        assert_eq!(req.interval_seconds, 2);
        assert_eq!(req.success_criteria, vec!["a", "b"]);
        assert_eq!(req.constraints, "约束正文");
    }

    #[test]
    fn plain_goal_parses_to_itself() {
        let req = parse_request(" 只是目标 ");
        assert_eq!(req.goal, " 只是目标 ");
        assert_eq!(req.interval_seconds, 0, "no front matter → struct default");
    }

    #[test]
    fn probe_detection_matches_the_python_keyword_list() {
        assert!(AutonomousLoop::is_noop_probe("黑盒探针"));
        assert!(!AutonomousLoop::is_noop_probe("重构模块"));
    }

    #[test]
    fn session_ids_are_twelve_hex_chars() {
        let id = short_sid();
        assert_eq!(id.len(), 12);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }
}

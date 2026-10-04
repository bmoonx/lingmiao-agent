//! 自动模式的 SQLite 持久化 — 一比一移植 Python 原版 `core/autonomous_store.py`。
//!
//! 三张表：
//! * `sessions`   — 会话元数据
//! * `tasks`      — 任务清单与状态
//! * `iterations` — 每轮 Main/Auditor 记录
//!
//! 原版用 `threading.Lock` 串行化写；Rust 侧用 `Mutex<Connection>` —— 一个
//! 进程内单连接串行访问，语义等价且更省 fd。

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use lingmiao_core::LingmiaoError;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;

use super::types::{AutonomousRequest, AutonomousSession, AutonomousTask, IterationResult};

/// 错误上下文里的 store 名。
pub const STORE: &str = "autonomous";

/// 自动会话的 SQLite 存储（线程安全）。
pub struct AutonomousStore {
    conn: Mutex<Connection>,
    path: PathBuf,
}

impl AutonomousStore {
    /// 打开（或创建）`db_path`，建表并跑迁移。
    pub fn open(db_path: impl Into<PathBuf>) -> Result<Self, LingmiaoError> {
        let path = db_path.into();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                LingmiaoError::memory(STORE, format!("create {}: {e}", parent.display()))
            })?;
        }
        let conn = Connection::open(&path)
            .map_err(|e| LingmiaoError::memory(STORE, format!("open {}: {e}", path.display())))?;
        // timeout=10s：跨进程写竞争时忙等而不是立刻 `database is locked`
        //（原版 `sqlite3.connect(..., timeout=10)`）。
        conn.busy_timeout(std::time::Duration::from_secs(10))
            .map_err(|e| LingmiaoError::memory(STORE, format!("busy_timeout: {e}")))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY,
                status TEXT NOT NULL,
                request TEXT NOT NULL,
                current_iteration INTEGER DEFAULT 0,
                current_task_id TEXT DEFAULT '',
                consecutive_errors INTEGER DEFAULT 0,
                report_path TEXT DEFAULT '',
                worktree_path TEXT DEFAULT '',
                branch_name TEXT DEFAULT '',
                error TEXT DEFAULT '',
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS tasks (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                seq INTEGER NOT NULL,
                description TEXT NOT NULL,
                status TEXT NOT NULL,
                depends_on TEXT DEFAULT '[]',
                result TEXT DEFAULT '',
                created_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS iterations (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                iteration INTEGER NOT NULL,
                task_id TEXT NOT NULL,
                main_output TEXT DEFAULT '{}',
                auditor_output TEXT DEFAULT '{}',
                auditor_result TEXT DEFAULT '{\"continue\": true}',
                error TEXT DEFAULT '',
                created_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_tasks_session ON tasks(session_id);
            CREATE INDEX IF NOT EXISTS idx_iterations_session ON iterations(session_id);",
        )
        .map_err(|e| LingmiaoError::memory(STORE, format!("create schema: {e}")))?;
        Ok(Self {
            conn: Mutex::new(conn),
            path,
        })
    }

    /// 存储文件路径。
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>, LingmiaoError> {
        self.conn
            .lock()
            .map_err(|_| LingmiaoError::memory(STORE, "store lock poisoned".to_string()))
    }

    /// 建会话 + 落任务清单（原版 `create_session`）。
    pub fn create_session(&self, session: &AutonomousSession) -> Result<(), LingmiaoError> {
        let now = super::now_iso();
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO sessions (id, status, request, current_iteration, current_task_id,
                consecutive_errors, report_path, worktree_path, branch_name,
                error, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                session.id,
                session.status.as_str(),
                serde_json::to_string(&session.request).unwrap_or_else(|_| "{}".into()),
                session.current_iteration as i64,
                session.current_task_id,
                session.consecutive_errors as i64,
                session.report_path,
                session.worktree_path,
                session.branch_name,
                session.error,
                now,
                now,
            ],
        )
        .map_err(|e| LingmiaoError::memory(STORE, format!("insert session: {e}")))?;
        for task in &session.request.tasks {
            // 任务 id 在 `start()` 里已加过 `{session_id}-` 前缀（原版口径，
            // `AutonomousLoop.start` 的 `task["id"] = f"{session.id}-{task['id']}"`），
            // 这里**原样落库** —— 再拼一次会得到 `sid-sid-t1` 这种双前缀，
            // 而 `_pick_next_task` 的依赖比对用的是单前缀 id，两边对不上。
            conn.execute(
                "INSERT INTO tasks (id, session_id, seq, description, status, depends_on, result, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    task.id,
                    session.id,
                    task.seq as i64,
                    task.description,
                    "pending",
                    serde_json::to_string(&task.depends_on).unwrap_or_else(|_| "[]".into()),
                    "",
                    now,
                ],
            )
            .map_err(|e| LingmiaoError::memory(STORE, format!("insert task: {e}")))?;
        }
        Ok(())
    }

    /// 更新会话行（原版 `update_session`）。
    pub fn update_session(&self, session: &AutonomousSession) -> Result<(), LingmiaoError> {
        let now = super::now_iso();
        let conn = self.lock()?;
        conn.execute(
            "UPDATE sessions SET status=?1, request=?2, current_iteration=?3, current_task_id=?4,
                consecutive_errors=?5, report_path=?6, worktree_path=?7, branch_name=?8,
                error=?9, updated_at=?10
             WHERE id=?11",
            params![
                session.status.as_str(),
                serde_json::to_string(&session.request).unwrap_or_else(|_| "{}".into()),
                session.current_iteration as i64,
                session.current_task_id,
                session.consecutive_errors as i64,
                session.report_path,
                session.worktree_path,
                session.branch_name,
                session.error,
                now,
                session.id,
            ],
        )
        .map_err(|e| LingmiaoError::memory(STORE, format!("update session: {e}")))?;
        Ok(())
    }

    /// 更新一条任务的状态与结果（原版 `update_task`）。
    pub fn update_task(
        &self,
        session_id: &str,
        task: &AutonomousTask,
    ) -> Result<(), LingmiaoError> {
        let conn = self.lock()?;
        conn.execute(
            "UPDATE tasks SET status=?1, result=?2 WHERE id=?3 AND session_id=?4",
            params![task.status, task.result, task.id, session_id],
        )
        .map_err(|e| LingmiaoError::memory(STORE, format!("update task: {e}")))?;
        Ok(())
    }

    /// 记录一轮（原版 `record_iteration`）。
    pub fn record_iteration(
        &self,
        session_id: &str,
        it: &IterationResult,
    ) -> Result<(), LingmiaoError> {
        let now = super::now_iso();
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO iterations (id, session_id, iteration, task_id, main_output,
                auditor_output, auditor_result, error, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                format!("iter-{}-{}", session_id, it.iteration),
                session_id,
                it.iteration as i64,
                it.task_id,
                serde_json::to_string(&it.main_output).unwrap_or_else(|_| "{}".into()),
                serde_json::to_string(&it.auditor_output).unwrap_or_else(|_| "{}".into()),
                serde_json::to_string(&serde_json::json!({
                    "continue": it.auditor_result.continue_,
                    "feedback": it.auditor_result.feedback,
                    "evidence": it.auditor_result.evidence,
                }))
                .unwrap_or_else(|_| "{}".into()),
                it.error,
                now,
            ],
        )
        .map_err(|e| LingmiaoError::memory(STORE, format!("record iteration: {e}")))?;
        Ok(())
    }

    /// 会话行（原始 JSON 串，便于 RPC/面板直接读）。
    pub fn get_session(&self, session_id: &str) -> Result<Option<Value>, LingmiaoError> {
        let conn = self.lock()?;
        let row = conn
            .query_row(
                "SELECT id, status, request, current_iteration, current_task_id,
                        consecutive_errors, report_path, worktree_path, branch_name, error,
                        created_at, updated_at
                 FROM sessions WHERE id=?1",
                params![session_id],
                |r| {
                    Ok(serde_json::json!({
                        "id": r.get::<_, String>(0)?,
                        "status": r.get::<_, String>(1)?,
                        "request": r.get::<_, String>(2)?,
                        "current_iteration": r.get::<_, i64>(3)?,
                        "current_task_id": r.get::<_, String>(4)?,
                        "consecutive_errors": r.get::<_, i64>(5)?,
                        "report_path": r.get::<_, String>(6)?,
                        "worktree_path": r.get::<_, String>(7)?,
                        "branch_name": r.get::<_, String>(8)?,
                        "error": r.get::<_, String>(9)?,
                        "created_at": r.get::<_, String>(10)?,
                        "updated_at": r.get::<_, String>(11)?,
                    }))
                },
            )
            .optional()
            .map_err(|e| LingmiaoError::memory(STORE, format!("get session: {e}")))?;
        Ok(row)
    }

    /// 会话的任务清单（按 seq 排序）。
    pub fn get_tasks(&self, session_id: &str) -> Result<Vec<Value>, LingmiaoError> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT id, seq, description, status, depends_on, result
                 FROM tasks WHERE session_id=?1 ORDER BY seq",
            )
            .map_err(|e| LingmiaoError::memory(STORE, format!("prepare tasks: {e}")))?;
        let rows = stmt
            .query_map(params![session_id], |r| {
                Ok(serde_json::json!({
                    "id": r.get::<_, String>(0)?,
                    "seq": r.get::<_, i64>(1)?,
                    "description": r.get::<_, String>(2)?,
                    "status": r.get::<_, String>(3)?,
                    "depends_on": r.get::<_, String>(4)?,
                    "result": r.get::<_, String>(5)?,
                }))
            })
            .map_err(|e| LingmiaoError::memory(STORE, format!("query tasks: {e}")))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| LingmiaoError::memory(STORE, format!("collect tasks: {e}")))
    }

    /// 会话的逐轮记录（按 iteration 排序，三个 JSON 列解码为对象）。
    pub fn get_iterations(&self, session_id: &str) -> Result<Vec<Value>, LingmiaoError> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT id, session_id, iteration, task_id, main_output, auditor_output,
                        auditor_result, error, created_at
                 FROM iterations WHERE session_id=?1 ORDER BY iteration",
            )
            .map_err(|e| LingmiaoError::memory(STORE, format!("prepare iterations: {e}")))?;
        let rows = stmt
            .query_map(params![session_id], |r| {
                let decode = |s: String| serde_json::from_str::<Value>(&s).unwrap_or(Value::Null);
                Ok(serde_json::json!({
                    "id": r.get::<_, String>(0)?,
                    "session_id": r.get::<_, String>(1)?,
                    "iteration": r.get::<_, i64>(2)?,
                    "task_id": r.get::<_, String>(3)?,
                    "main_output": decode(r.get::<_, String>(4)?),
                    "auditor_output": decode(r.get::<_, String>(5)?),
                    "auditor_result": decode(r.get::<_, String>(6)?),
                    "error": r.get::<_, String>(7)?,
                    "created_at": r.get::<_, String>(8)?,
                }))
            })
            .map_err(|e| LingmiaoError::memory(STORE, format!("query iterations: {e}")))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| LingmiaoError::memory(STORE, format!("collect iterations: {e}")))
    }

    /// 全部会话（按 updated_at 倒序）。
    pub fn list_sessions(&self) -> Result<Vec<Value>, LingmiaoError> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare("SELECT id, status, current_iteration, updated_at FROM sessions ORDER BY updated_at DESC")
            .map_err(|e| LingmiaoError::memory(STORE, format!("prepare list: {e}")))?;
        let rows = stmt
            .query_map([], |r| {
                Ok(serde_json::json!({
                    "id": r.get::<_, String>(0)?,
                    "status": r.get::<_, String>(1)?,
                    "current_iteration": r.get::<_, i64>(2)?,
                    "updated_at": r.get::<_, String>(3)?,
                }))
            })
            .map_err(|e| LingmiaoError::memory(STORE, format!("query list: {e}")))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| LingmiaoError::memory(STORE, format!("collect list: {e}")))
    }

    /// 表名集合（预检 `db_schema` 用）。
    pub fn tables(&self) -> Result<Vec<String>, LingmiaoError> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table'")
            .map_err(|e| LingmiaoError::memory(STORE, format!("prepare tables: {e}")))?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(|e| LingmiaoError::memory(STORE, format!("query tables: {e}")))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| LingmiaoError::memory(STORE, format!("collect tables: {e}")))
    }
}

/// 仅用于把请求里的任务种子转成可落库的形式（供调用点复用）。
pub fn task_seed_id(session_id: &str, local_id: &str) -> String {
    format!("{session_id}-{local_id}")
}

// 供 `types::AutonomousRequest` 反序列化失败的兜底 —— 空请求。
impl AutonomousRequest {
    /// 反序列化失败时的空请求。
    pub fn empty() -> Self {
        Self::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autonomous::types::{SessionStatus, TaskSeed};

    fn tmp_store(tag: &str) -> AutonomousStore {
        let dir = std::env::temp_dir().join(format!(
            "lingmiao-auto-store-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        AutonomousStore::open(dir.join("autonomous.db")).unwrap()
    }

    #[test]
    fn schema_has_three_tables() {
        let s = tmp_store("schema");
        let t = s.tables().unwrap();
        for want in ["sessions", "tasks", "iterations"] {
            assert!(t.contains(&want.to_string()), "missing table {want}");
        }
    }

    #[test]
    fn session_round_trips_with_tasks() {
        let s = tmp_store("rt");
        let mut req = AutonomousRequest {
            goal: "做一件事".into(),
            // 前缀由 `AutonomousLoop::start` 加（`{sid}-{local_id}`）—— store 原样落库。
            tasks: vec![TaskSeed {
                id: "sess1-t1".into(),
                seq: 1,
                description: "第一步".into(),
                depends_on: vec![],
            }],
            ..Default::default()
        };
        req.interval_seconds = 5;
        let sess = AutonomousSession::new("sess1", req);
        s.create_session(&sess).unwrap();
        let row = s.get_session("sess1").unwrap().expect("session row");
        assert_eq!(row["status"], "pending");
        let tasks = s.get_tasks("sess1").unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0]["id"], "sess1-t1");
        assert_eq!(tasks[0]["status"], "pending");

        let mut sess2 = sess.clone();
        sess2.status = SessionStatus::Running;
        sess2.current_iteration = 2;
        s.update_session(&sess2).unwrap();
        let row = s.get_session("sess1").unwrap().unwrap();
        assert_eq!(row["status"], "running");
        assert_eq!(row["current_iteration"], 2);
    }

    #[test]
    fn task_ids_are_not_prefixed_twice() {
        // 回归：store 曾再拼一次 `{session_id}-{task.id}`，而 `start()` 已经加过
        // 前缀 → 落库成 `sid-sid-t1`，与 `pick_next_task` 的依赖比对（单前缀）
        // 对不上（实测 `current_task_id` 出现 `2b31…-2b31…-t1`）。
        let s = tmp_store("prefix");
        let sess = AutonomousSession::new(
            "abc123",
            AutonomousRequest {
                goal: "g".into(),
                tasks: vec![TaskSeed {
                    id: "abc123-t1".into(),
                    seq: 1,
                    description: "d".into(),
                    depends_on: vec!["abc123-t0".into()],
                }],
                ..Default::default()
            },
        );
        s.create_session(&sess).unwrap();
        let tasks = s.get_tasks("abc123").unwrap();
        assert_eq!(tasks[0]["id"], "abc123-t1");
        let deps: Vec<String> =
            serde_json::from_str(tasks[0]["depends_on"].as_str().unwrap()).unwrap();
        assert_eq!(deps, vec!["abc123-t0"], "dependency ids must stay as given");
    }

    #[test]
    fn iteration_records_and_decodes() {
        let s = tmp_store("iter");
        let req = AutonomousRequest {
            goal: "g".into(),
            ..Default::default()
        };
        let sess = AutonomousSession::new("sess2", req);
        s.create_session(&sess).unwrap();
        let it = IterationResult {
            iteration: 1,
            task_id: "sess2-t1".into(),
            main_output: serde_json::json!({"response": "hi"}),
            auditor_output: serde_json::json!({"response": "ok"}),
            auditor_result: super::super::types::AuditorResult {
                continue_: true,
                feedback: "再改".into(),
                ..Default::default()
            },
            error: String::new(),
        };
        s.record_iteration("sess2", &it).unwrap();
        let rows = s.get_iterations("sess2").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["main_output"]["response"], "hi");
        assert_eq!(rows[0]["auditor_result"]["continue"], true);
    }
}

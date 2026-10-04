//! 渐进学习：MetaLoop（Task 3.1）+ 自主约束发现（Task 3.2 EPO-Safe）
//! — 移植 Python 原版 `autonomous_loop.py::_run_meta_loop` /
//! `_discover_constraints` / `_load_*` / `_save_*`。
//!
//! 两者都在会话**结束后**跑，读同一份 trajectory，写两个不同的 JSON：
//!
//! | 环 | 输出 | 注入下一会话的方式 |
//! |----|------|-------------------|
//! | MetaLoop | `.memory/constraints/improvements.json` | Main 消息的「历史经验」块（critical 最近 3 / major 最近 5） |
//! | 约束发现 | `.memory/constraints/auto_learned.json` | Main 消息的「自学习约束」块（active 最近 5） |
//!
//! **两者各用独立的去重集** —— 原版曾因共用一个 set 导致 `_discover_constraints`
//! 每次都命中去重、静默跳过全部会话（v0.2.222 的 bug-scan 发现）。Rust 侧用两个
//! 独立 `HashSet`，并在测试里锁死这条不变量。

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};

use lingmiao_core::LingmiaoError;
use serde_json::Value;

use super::types::{AutoConstraint, MetaImprovement};

/// 去重集容量上限（原版 `_processed_meta_max` / `_processed_constraint_max` = 500）。
pub const PROCESSED_MAX: usize = 500;
/// improvements.json 最多保留条目（原版 `len(unique) > 20 → unique[-20:]`）。
pub const MAX_IMPROVEMENTS: usize = 20;
/// 约束按 `condition` 前 N 字符去重（原版 `c.condition[:60]`）。
const CONSTRAINT_DEDUP_PREFIX: usize = 60;

/// MetaLoop + 约束发现的状态（两个**独立**的 LRU 去重集）。
#[derive(Default)]
pub struct MetaState {
    /// 已做过 MetaLoop 分析的会话 id（FIFO LRU）。
    processed_meta: VecDeque<String>,
    /// 已做过约束发现的会话 id（**独立**于上者 —— 见模块头）。
    processed_constraints: VecDeque<String>,
}

impl MetaState {
    /// 新状态。
    pub fn new() -> Self {
        Self::default()
    }

    /// 登记一个会话已做 MetaLoop 分析；已登记过返回 `false`。
    fn mark_meta(&mut self, session_id: &str) -> bool {
        push_lru(&mut self.processed_meta, session_id, PROCESSED_MAX)
    }

    /// 登记一个会话已做约束发现；已登记过返回 `false`。
    fn mark_constraints(&mut self, session_id: &str) -> bool {
        push_lru(&mut self.processed_constraints, session_id, PROCESSED_MAX)
    }

    /// 两个去重集是否互不干扰（自省 / 测试用）。
    pub fn sizes(&self) -> (usize, usize) {
        (self.processed_meta.len(), self.processed_constraints.len())
    }
}

/// FIFO 入队并按容量淘汰最旧；已存在返回 `false`。
fn push_lru(q: &mut VecDeque<String>, id: &str, max: usize) -> bool {
    if q.iter().any(|x| x == id) {
        return false;
    }
    q.push_back(id.to_string());
    while q.len() > max {
        q.pop_front();
    }
    true
}

/// improvements / auto_learned 的落点（`.memory/constraints/`）。
#[derive(Debug, Clone)]
pub struct MetaPaths {
    /// `.memory/constraints/`
    pub dir: PathBuf,
}

impl MetaPaths {
    /// 由记忆目录推导。
    pub fn for_memory_dir(memory_dir: &Path) -> Self {
        Self {
            dir: memory_dir.join("constraints"),
        }
    }

    /// `improvements.json`。
    pub fn improvements_file(&self) -> PathBuf {
        self.dir.join("improvements.json")
    }

    /// `auto_learned.json`。
    pub fn constraints_file(&self) -> PathBuf {
        self.dir.join("auto_learned.json")
    }
}

/// 读 improvements.json（缺失 / 坏 JSON → 空）。
pub fn load_improvements(paths: &MetaPaths) -> Vec<MetaImprovement> {
    let Ok(raw) = std::fs::read_to_string(paths.improvements_file()) else {
        return Vec::new();
    };
    let Ok(v) = serde_json::from_str::<Value>(&raw) else {
        return Vec::new();
    };
    v.as_array()
        .map(|arr| {
            arr.iter()
                .map(|r| MetaImprovement {
                    pattern_name: str_of(r, "pattern_name"),
                    pattern_description: str_of(r, "pattern_description"),
                    suggestion: str_of(r, "suggestion"),
                    severity: {
                        let s = str_of(r, "severity");
                        if s.is_empty() { "minor".to_string() } else { s }
                    },
                    source_session: str_of(r, "source_session"),
                    created_at: str_of(r, "created_at"),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 写 improvements.json。
pub fn save_improvements(
    paths: &MetaPaths,
    improvements: &[MetaImprovement],
) -> Result<(), LingmiaoError> {
    std::fs::create_dir_all(&paths.dir)
        .map_err(|e| LingmiaoError::memory(super::STORE, format!("mkdir constraints: {e}")))?;
    let arr: Vec<Value> = improvements
        .iter()
        .map(|i| {
            serde_json::json!({
                "pattern_name": i.pattern_name,
                "pattern_description": i.pattern_description,
                "suggestion": i.suggestion,
                "severity": i.severity,
                "source_session": i.source_session,
                "created_at": i.created_at,
            })
        })
        .collect();
    let body = serde_json::to_string_pretty(&arr).unwrap_or_else(|_| "[]".to_string());
    std::fs::write(paths.improvements_file(), body)
        .map_err(|e| LingmiaoError::memory(super::STORE, format!("write improvements: {e}")))
}

/// 读 auto_learned.json（缺失 / 坏 JSON → 空）。
pub fn load_constraints(paths: &MetaPaths) -> Vec<AutoConstraint> {
    let Ok(raw) = std::fs::read_to_string(paths.constraints_file()) else {
        return Vec::new();
    };
    let Ok(v) = serde_json::from_str::<Value>(&raw) else {
        return Vec::new();
    };
    v.as_array()
        .map(|arr| {
            arr.iter()
                .map(|r| AutoConstraint {
                    condition: str_of(r, "condition"),
                    prohibited_action: str_of(r, "prohibited_action"),
                    reason: str_of(r, "reason"),
                    source_session: str_of(r, "source_session"),
                    iteration: r.get("iteration").and_then(Value::as_u64).unwrap_or(0),
                    created_at: str_of(r, "created_at"),
                    // `active` 缺省为 true（原版 `r.get("active", True)`）。
                    active: r.get("active").and_then(Value::as_bool).unwrap_or(true),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 写 auto_learned.json。
pub fn save_constraints(
    paths: &MetaPaths,
    constraints: &[AutoConstraint],
) -> Result<(), LingmiaoError> {
    std::fs::create_dir_all(&paths.dir)
        .map_err(|e| LingmiaoError::memory(super::STORE, format!("mkdir constraints: {e}")))?;
    let arr: Vec<Value> = constraints
        .iter()
        .map(|c| {
            serde_json::json!({
                "condition": c.condition,
                "prohibited_action": c.prohibited_action,
                "reason": c.reason,
                "source_session": c.source_session,
                "iteration": c.iteration,
                "created_at": c.created_at,
                "active": c.active,
            })
        })
        .collect();
    let body = serde_json::to_string_pretty(&arr).unwrap_or_else(|_| "[]".to_string());
    std::fs::write(paths.constraints_file(), body)
        .map_err(|e| LingmiaoError::memory(super::STORE, format!("write constraints: {e}")))
}

fn str_of(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

/// 从 trajectory JSONL 读出某一种事件（`MAIN_END` / `AUDITOR_END`）。
pub fn trajectory_events(traj_path: &Path, event: &str) -> Vec<Value> {
    let Ok(text) = std::fs::read_to_string(traj_path) else {
        return Vec::new();
    };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v.get("event").and_then(Value::as_str) == Some(event))
        .collect()
}

/// MetaLoop（Task 3.1）：分析 trajectory → 更新 improvements.json。
///
/// 两条模式（与原版一致）：
/// * `stalled_iterations` — ≥3 轮 `continue:true` 且 `evidence` 为空 → critical
/// * `repeated_feedback` — 相邻两轮 feedback 前 120 字符相同 → major
pub fn run_meta_loop(
    state: &mut MetaState,
    paths: &MetaPaths,
    session_id: &str,
) -> Result<usize, LingmiaoError> {
    if !state.mark_meta(session_id) {
        return Ok(0);
    }
    let traj = paths
        .dir
        .parent()
        .map(|m| m.join("trajectories"))
        .unwrap_or_else(|| PathBuf::from("."))
        .join(format!("{session_id}.jsonl"));
    let auditor_ends = trajectory_events(&traj, "AUDITOR_END");
    if auditor_ends.is_empty() {
        return Ok(0);
    }
    let mut improvements = load_improvements(paths);

    // ── 模式 1：停摆轮次 ──
    let stalled = auditor_ends
        .iter()
        .filter(|e| {
            e.get("continue").and_then(Value::as_bool).unwrap_or(false)
                && e.get("evidence")
                    .and_then(Value::as_array)
                    .map(|a| a.is_empty())
                    .unwrap_or(true)
        })
        .count();
    if stalled >= 3 {
        improvements.push(MetaImprovement {
            pattern_name: "stalled_iterations".to_string(),
            pattern_description: format!(
                "Session {session_id}: {stalled} iterations with continue:true but no evidence of file changes. Auditor keeps requesting but Main produces no new artifacts."
            ),
            suggestion: "当 Main 连续 2 轮被 Auditor 要求 continue 但无新文件产出时，不要继续生成文字说明。主动用 grep/list_directory 确认现状，然后选择一个具体文件动手改。文字说明不算产出。".to_string(),
            severity: "critical".to_string(),
            source_session: session_id.to_string(),
            created_at: super::now_iso(),
        });
    }

    // ── 模式 2：重复反馈 ──
    let feedbacks: Vec<String> = auditor_ends
        .iter()
        .filter_map(|e| {
            e.get("feedback")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .filter(|s| !s.trim().is_empty())
        .collect();
    if feedbacks.len() >= 2 {
        for i in 0..feedbacks.len() - 1 {
            let a: String = feedbacks[i].chars().take(120).collect();
            let b: String = feedbacks[i + 1].chars().take(120).collect();
            if !a.is_empty() && a == b {
                improvements.push(MetaImprovement {
                    pattern_name: "repeated_feedback".to_string(),
                    pattern_description: format!(
                        "Session {session_id}: Auditor gave identical feedback in iterations {} and {}: '{}...'",
                        i + 1,
                        i + 2,
                        a.chars().take(80).collect::<String>()
                    ),
                    suggestion: "Auditor 重复反馈相同问题表明 Main 未有效响应。当收到 'Auditor 上轮反馈' 时，必须逐条读、逐条用工具确认、逐条改代码。不要采用 '本轮已全部修复' 的概括性回复——每项修复都要有 git diff 证据。".to_string(),
                    severity: "major".to_string(),
                    source_session: session_id.to_string(),
                    created_at: super::now_iso(),
                });
                break;
            }
        }
    }

    // ── 按 pattern_name 去重（取最新）+ 容量截断 ──
    let mut seen: HashSet<String> = HashSet::new();
    let mut unique: Vec<MetaImprovement> = Vec::new();
    for imp in improvements.into_iter().rev() {
        if seen.insert(imp.pattern_name.clone()) {
            unique.push(imp);
        }
    }
    unique.reverse();
    if unique.len() > MAX_IMPROVEMENTS {
        unique = unique.split_off(unique.len() - MAX_IMPROVEMENTS);
    }
    save_improvements(paths, &unique)?;
    Ok(unique.len())
}

/// 约束发现（Task 3.2 EPO-Safe）：从 Auditor feedback 关键词提取自学习约束。
pub fn discover_constraints(
    state: &mut MetaState,
    paths: &MetaPaths,
    session_id: &str,
) -> Result<usize, LingmiaoError> {
    if !state.mark_constraints(session_id) {
        return Ok(0);
    }
    let traj = paths
        .dir
        .parent()
        .map(|m| m.join("trajectories"))
        .unwrap_or_else(|| PathBuf::from("."))
        .join(format!("{session_id}.jsonl"));
    let auditor_ends = trajectory_events(&traj, "AUDITOR_END");
    if auditor_ends.is_empty() {
        return Ok(0);
    }
    let mut existing = load_constraints(paths);
    let before = existing.len();

    for ae in &auditor_ends {
        let fb = ae.get("feedback").and_then(Value::as_str).unwrap_or("");
        if fb.is_empty() {
            continue;
        }
        let fb_lower = fb.to_lowercase();
        let iteration = ae.get("iteration").and_then(Value::as_u64).unwrap_or(0);

        // 幻影修复
        if ["phantom", "不存在的问题", "幻觉修复", "凭空"]
            .iter()
            .any(|kw| fb.contains(kw))
        {
            existing.push(AutoConstraint::new(
                "Main Agent 声称修复了一个工具验证显示不存在的问题",
                "禁止声称 '已修复 X 问题' 而不提供 git diff 或文件变更证据。修复前必须先用工具确认问题真实存在。",
                format!("Session {session_id}: Auditor 检测到幻影修复"),
                session_id,
                iteration,
                super::now_iso(),
            ));
        }
        // 无实质产出
        if [
            "无实际产出",
            "无实质性",
            "no actual output",
            "no file change",
            "无文件改动",
        ]
        .iter()
        .any(|kw| fb_lower.contains(kw))
        {
            existing.push(AutoConstraint::new(
                "Main Agent 本轮未产生文件改动",
                "禁止在无文件改动时声称 '本轮完成' 或推进版本号。必须先产生 write_file/edit 实际代码改动，再声称完成。",
                format!("Session {session_id}: Auditor 发现 Main 声称完成但无文件改动"),
                session_id,
                iteration,
                super::now_iso(),
            ));
        }
        // 配置修改冒充产出
        if [
            "配置修改",
            "环境搭建",
            "不算产出",
            "config only",
            "not substantive",
        ]
        .iter()
        .any(|kw| fb_lower.contains(kw))
        {
            existing.push(AutoConstraint::new(
                "Main 仅产出配置修改（.json/.toml/.gitignore 等）",
                "禁止将配置修改、环境搭建、格式化、路径修正作为主打产出。这些是辅助操作——必须有实质代码改动配合。",
                format!("Session {session_id}: Auditor 判定产出仅为配置/环境变更"),
                session_id,
                iteration,
                super::now_iso(),
            ));
        }
    }

    if existing.len() > before {
        // 按 condition 前 60 字符去重（取最新）。
        let mut seen: HashSet<String> = HashSet::new();
        let mut unique: Vec<AutoConstraint> = Vec::new();
        for c in existing.into_iter().rev() {
            let key: String = c.condition.chars().take(CONSTRAINT_DEDUP_PREFIX).collect();
            if seen.insert(key) {
                unique.push(c);
            }
        }
        unique.reverse();
        save_constraints(paths, &unique)?;
        return Ok(unique.len());
    }
    Ok(before)
}

/// Main 消息里的「历史经验」块（原版 critical 最近 3 / major 最近 5）。
pub fn meta_improvements_block(paths: &MetaPaths) -> String {
    let all = load_improvements(paths);
    if all.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    let critical: Vec<&MetaImprovement> = all.iter().filter(|m| m.severity == "critical").collect();
    if !critical.is_empty() {
        out.push_str("\n\n# 🔴 历史经验（严重 — 必须遵守）\n");
        for m in critical.iter().rev().take(3).rev() {
            out.push_str(&format!("\n- **{}**: {}", m.pattern_name, m.suggestion));
        }
    }
    let major: Vec<&MetaImprovement> = all.iter().filter(|m| m.severity == "major").collect();
    if !major.is_empty() {
        out.push_str("\n\n# 🟡 历史经验（重要）\n");
        for m in major.iter().rev().take(5).rev() {
            out.push_str(&format!("\n- **{}**: {}", m.pattern_name, m.suggestion));
        }
    }
    out
}

/// Main 消息里的「自学习约束」块（active 最近 5）。
pub fn auto_constraints_block(paths: &MetaPaths) -> String {
    let active: Vec<AutoConstraint> = load_constraints(paths)
        .into_iter()
        .filter(|c| c.active)
        .collect();
    if active.is_empty() {
        return String::new();
    }
    let mut out = String::from("\n\n# ⛔ 自学习约束（AI 自主发现 — 禁止违反）\n");
    for c in active.iter().rev().take(5).rev() {
        out.push_str(&format!(
            "\n- 当「{}」时 → {}\n  （来源：会话 {}，原因：{}）",
            c.condition, c.prohibited_action, c.source_session, c.reason
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_paths(tag: &str) -> MetaPaths {
        let dir = std::env::temp_dir().join(format!(
            "lingmiao-auto-meta-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("trajectories")).unwrap();
        MetaPaths::for_memory_dir(&dir)
    }

    fn write_traj(paths: &MetaPaths, sid: &str, lines: &[Value]) {
        let f = paths
            .dir
            .parent()
            .unwrap()
            .join("trajectories")
            .join(format!("{sid}.jsonl"));
        let body = lines
            .iter()
            .map(|v| serde_json::to_string(v).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(f, body).unwrap();
    }

    #[test]
    fn the_two_dedup_sets_are_independent() {
        // 原版 v0.2.222 的回归：共用 set 会让约束发现静默全跳过。
        let mut s = MetaState::new();
        assert!(s.mark_meta("s1"));
        assert!(!s.mark_meta("s1"));
        // mark_meta 不影响 mark_constraints。
        assert!(
            s.mark_constraints("s1"),
            "constraint set must be independent"
        );
        assert_eq!(s.sizes(), (1, 1));
    }

    #[test]
    fn meta_loop_writes_critical_on_stalled_iterations() {
        let paths = tmp_paths("stall");
        let lines: Vec<Value> = (1..=3)
            .map(|i| {
                serde_json::json!({
                    "event": "AUDITOR_END", "iteration": i,
                    "continue": true, "feedback": "", "evidence": []
                })
            })
            .collect();
        write_traj(&paths, "s1", &lines);
        let mut state = MetaState::new();
        let n = run_meta_loop(&mut state, &paths, "s1").unwrap();
        assert_eq!(n, 1);
        let imps = load_improvements(&paths);
        assert_eq!(imps[0].pattern_name, "stalled_iterations");
        assert_eq!(imps[0].severity, "critical");
        std::fs::remove_dir_all(paths.dir.parent().unwrap()).ok();
    }

    #[test]
    fn constraint_discovery_extracts_phantom_fix_rule() {
        let paths = tmp_paths("phantom");
        write_traj(
            &paths,
            "s2",
            &[serde_json::json!({
                "event": "AUDITOR_END", "iteration": 2,
                "continue": true, "feedback": "检测到幻觉修复：此改动针对不存在的问题"
            })],
        );
        let mut state = MetaState::new();
        let n = discover_constraints(&mut state, &paths, "s2").unwrap();
        assert_eq!(n, 1);
        let cs = load_constraints(&paths);
        assert!(cs[0].active);
        assert!(cs[0].condition.contains("不存在的问题"));
        std::fs::remove_dir_all(paths.dir.parent().unwrap()).ok();
    }

    #[test]
    fn constraint_dedup_keeps_one_row_per_condition_prefix() {
        let paths = tmp_paths("dedup");
        let same = serde_json::json!({
            "event": "AUDITOR_END", "iteration": 1,
            "continue": true, "feedback": "无文件改动"
        });
        write_traj(&paths, "a", std::slice::from_ref(&same));
        write_traj(&paths, "b", std::slice::from_ref(&same));
        let mut state = MetaState::new();
        discover_constraints(&mut state, &paths, "a").unwrap();
        let n = discover_constraints(&mut state, &paths, "b").unwrap();
        assert_eq!(n, 1, "identical condition must not be duplicated");
        std::fs::remove_dir_all(paths.dir.parent().unwrap()).ok();
    }
}

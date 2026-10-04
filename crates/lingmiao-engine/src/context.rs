//! Strongly-typed per-turn context (Q7 decision A).
//!
//! The Python original threaded stage outputs through a string-keyed `dict`;
//! Q7 replaces that with a real struct so a mistyped field is a compile error
//! and each stage's typed output is explicit. The three M4 stages —
//! `组织上下文`, `工作阶段`, `沉淀阶段` — each take `&mut TurnContext` and fill
//! the field they own.

use std::sync::Arc;

use lingmiao_core::Config;
use lingmiao_memory::{Memory, Node};
use lingmiao_tools::ToolRegistry;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The 沉淀阶段 stage's consolidated output (Q9): observation / audit / quality
/// / task, parsed out of one LLM JSON reply.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SummaryReport {
    /// The raw JSON the model returned (after extraction / salvage).
    pub raw: Value,
    /// Conversation type (`qa` / `task` / `chat` / `file`).
    pub obs_type: String,
    /// One-line summary of the turn.
    pub summary: String,
    /// Number of observation items the model proposed.
    pub observations: u64,
    /// Tracked task status (`in_progress` / `completed` / `idle`).
    pub task_status: String,
    /// Suggested next steps for the tracked task.
    pub next_steps: String,
    /// Context-audit grade (`good` / `partial` / `poor`).
    pub audit_grade: String,
    /// Number of audit issues found.
    pub issues: u64,
    /// Content-quality grade (`good` / `minor问题` / `major问题`).
    pub quality_grade: String,
    /// Number of quality corrections proposed.
    pub corrections: u64,
    /// Non-empty when the summary stage itself faulted.
    pub error: String,
}

impl SummaryReport {
    /// Parse a consolidated reply (fields absent from `raw` become defaults).
    pub fn from_json(raw: &Value) -> Self {
        let arr_len = |v: &Value| v.as_array().map(|a| a.len() as u64).unwrap_or(0);
        Self {
            obs_type: str_at(raw, &["type"]),
            summary: str_at(raw, &["summary"]),
            observations: arr_len(&raw["items"]),
            task_status: str_at(raw, &["task", "taskStatus"]),
            next_steps: str_at(raw, &["task", "nextSteps"]),
            audit_grade: str_at(raw, &["audit", "grade"]),
            issues: arr_len(&raw["audit"]["issues"]),
            quality_grade: str_at(raw, &["quality", "grade"]),
            corrections: arr_len(&raw["quality"]["corrections"]),
            raw: raw.clone(),
            error: String::new(),
        }
    }

    /// A report carrying only an error (fallback when the stage faults).
    pub fn failed(error: impl Into<String>) -> Self {
        Self {
            error: error.into(),
            ..Default::default()
        }
    }

    /// The observation items the model proposed (`items[]`), normalised for
    /// insertion into #2 — the 原版 F-要点记录 contract:
    /// body-less items are dropped; a blank `kind` defaults to `fact`, a blank
    /// `topic` to the kind, a blank `name` to the topic. Owned values, so they
    /// can outlive the parsed `raw` JSON and feed a borrow-based batch.
    pub fn items(&self) -> Vec<SummaryItem> {
        let Some(arr) = self.raw.get("items").and_then(Value::as_array) else {
            return Vec::new();
        };
        arr.iter()
            .filter_map(|it| {
                let content = field(it, "content");
                if content.is_empty() {
                    return None;
                }
                let raw_kind = field(it, "kind");
                let kind = if raw_kind.is_empty() {
                    "fact".to_string()
                } else {
                    raw_kind
                };
                let raw_topic = field(it, "topic");
                let topic = if raw_topic.is_empty() {
                    kind.clone()
                } else {
                    raw_topic
                };
                let raw_name = field(it, "name");
                let name = if raw_name.is_empty() {
                    topic.clone()
                } else {
                    raw_name
                };
                Some(SummaryItem {
                    kind,
                    topic,
                    name,
                    content,
                    keywords: field(it, "keywords"),
                })
            })
            .collect()
    }
}

/// One 沉淀阶段 observation item (原版 F-要点记录's `items[]` entry), owned.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SummaryItem {
    /// Record kind (`fact` / `preference` / `decision` / `constraint` / …).
    pub kind: String,
    /// Grouping topic.
    pub topic: String,
    /// Short label.
    pub name: String,
    /// Body text.
    pub content: String,
    /// Comma-separated search keywords.
    pub keywords: String,
}

/// Trimmed string field of a JSON object (`""` when absent / not a string).
fn field(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

/// Walk a nested object path, returning the string value (or `""`).
fn str_at(v: &Value, path: &[&str]) -> String {
    let mut cur = v;
    for key in path {
        cur = &cur[*key];
    }
    cur.as_str().unwrap_or("").to_string()
}

/// Everything one turn's stages read and write.
///
/// Note what is **absent**：no conversation history. The engine is stateless
/// (原版对齐) — a turn's messages start from blank, and continuity is rebuilt each
/// turn from the #1 archive inside [`assemble`].
pub struct TurnContext {
    /// Stable id shared by this turn's archive row, #2 observations and the MG
    /// update (原版 threaded one `turn_id` through D/F/G/H/I). Generated once.
    pub turn_id: String,
    /// The raw user input for this turn.
    pub input: String,
    /// The chat-zone memory bundle, when wired.
    pub memory: Option<Arc<Memory>>,
    /// The tool registry the stages draw their whitelists from.
    pub registry: Arc<ToolRegistry>,
    /// Resolved configuration (prompts + stage whitelists + timeouts).
    pub cfg: Config,
    /// **A-嵌入检索** output: how many knowledge-graph candidates the
    /// deterministic retrieval handed the base reference（原版 `knowledge.search`
    /// → top-50）.
    pub a_candidates: u64,
    /// `组织上下文` output: the assembled six-section block, injected into
    /// 工作阶段's system prompt as `{prefix}`.
    pub b_context: Option<String>,
    /// §8.5 context bar: characters the **knowledge-graph** section of the
    /// assembled block contributed (`知识记忆`) — taken from the block itself,
    /// so it counts what was *injected*, not what was merely retrieved.
    pub b_knowledge_chars: u64,
    /// §8.5 context bar: characters the **observation timeline** section
    /// contributed (`历史观测`).
    pub b_history_chars: u64,
    /// §8.5 context bar: characters the **recent conversations** section
    /// contributed (`最近对话`) — the archive turns pulled in by
    /// `loadRecentTurns`.
    pub b_recent_turn_chars: u64,
    /// `工作阶段` output: the assistant's answer.
    pub c_answer: Option<String>,
    /// 沉淀阶段 output.
    pub summary: Option<SummaryReport>,
}

impl TurnContext {
    /// Build a fresh context for `input`（无历史 —— 见 [`TurnContext`] 说明）。
    pub fn new(
        input: impl Into<String>,
        memory: Option<Arc<Memory>>,
        registry: Arc<ToolRegistry>,
        cfg: Config,
    ) -> Self {
        Self {
            turn_id: lingmiao_memory::short_id("turn"),
            input: input.into(),
            memory,
            registry,
            cfg,
            a_candidates: 0,
            b_context: None,
            b_knowledge_chars: 0,
            b_history_chars: 0,
            b_recent_turn_chars: 0,
            c_answer: None,
            summary: None,
        }
    }
}

// ── 上下文装配器（原版 `core/context.py::ContextAssembler`） ──────────
//
// 原版把「本轮该给模型什么」拆成两半，且**都是纯函数**（无 LLM，只读 #1/#2/#3）：
//
//   组织上下文阶段先看一份**底座参考**（[`build_base`]）——约束 / KG / 观测 /
//   最近 3 轮 / 技能五段——据此挑出要加载的记录；模型回来一个 selection
//   （`loadObservations` / `loadNodes` / `loadRecentTurns`），[`assemble`] 再把
//   拼成工作阶段 system 的 `{prefix}`。
//
// 这正是「每轮从空白开始、连续性靠记忆检索」的实现：消息列表每轮只有本轮输入，
// 上一轮的内容**只能**经 [`assemble`] 里 `archive.recent(n)` 从归档库重新取回。

/// The six 约束类 kinds that always ride the context（原版 `_KIND_LABELS`），
/// with their user-facing label and one-line description.
///
/// 数据类（`fact` / `bug` / `code` / `feature` / `version` / `llm_stage` …）
/// **不**无差别注入 —— 它们由组织上下文阶段按需 `loadObservations` 加载。
pub const KIND_LABELS: &[(&str, &str, &str)] = &[
    ("constraint", "规范约束", "强制执行的行为规则"),
    ("correction", "错误教训", "从过往错误中学到的纠正措施"),
    ("decision", "历史决策", "用户和 AI 做出的关键决定"),
    ("preference", "用户偏好", "用户的习惯和倾向"),
    ("task", "任务状态", "当前进行中的任务及下一步"),
    ("audit", "审计结论", "上下文审计发现的问题"),
];

/// How many archive turns the **base reference** always carries（原版
/// `_build_base_turns`: `archive.recent(3)`）.
pub const BASE_TURNS: usize = 3;

/// The six-section block selected for this turn, plus its **real** per-section
/// character volumes（§8.5 上下文组成条用真计量，不用魔数系数）。
#[derive(Debug, Clone, Default)]
pub struct AssembledContext {
    /// The whole block, in section order — this is what goes into 工作阶段's
    /// system prompt as `{prefix}`.
    pub text: String,
    /// Characters the **knowledge-graph** section contributed (`知识记忆`).
    pub knowledge_chars: u64,
    /// Characters the **observation timeline** section contributed (`历史观测`).
    pub observation_chars: u64,
    /// Characters the **recent conversations** section contributed (`最近对话`).
    pub recent_turn_chars: u64,
    /// How many archive turns that section actually pulled in（`loadRecentTurns`
    /// 的真实结果，供 A→B 交接提示复述）。
    pub recent_turns: u64,
}

/// Section 1 — the 约束类 observations, label + description + body per kind.
fn constraints_section(mem: &Memory) -> String {
    let mut out = String::new();
    for (kind, label, desc) in KIND_LABELS {
        let Ok(items) = mem.observations.by_kind(kind, 50) else {
            continue;
        };
        if items.is_empty() {
            continue;
        }
        out.push_str(&format!("## {label}\n  [{desc}]\n"));
        for c in &items {
            out.push_str(&format!("  [{}] {}: {}\n", c.kind, c.name, c.content));
        }
    }
    out
}

/// Section 1 (base variant) — same kinds, the `build_base` formatting
/// （原版 `_build_base_constraints`）.
fn constraints_section_base(mem: &Memory) -> String {
    let mut out = String::new();
    for (kind, label, _desc) in KIND_LABELS {
        let Ok(items) = mem.observations.by_kind(kind, 50) else {
            continue;
        };
        if items.is_empty() {
            continue;
        }
        out.push_str(&format!("## {label} (all)\n"));
        for item in &items {
            out.push_str(&format!(
                "  {} | {}/{} | {}\n",
                item.kind, item.topic, item.name, item.content
            ));
        }
    }
    out
}

/// One knowledge-graph node as a context line.
fn node_line(n: &Node) -> String {
    let content = if n.content.is_empty() {
        &n.summary
    } else {
        &n.content
    };
    format!("  [{}] {}: {}\n", n.kind, n.name, content)
}

/// The failed tool calls of one archived turn, as short `⚠️` lines — the
/// 「上轮哪些工具踩了坑」continuity hint（原版 `_format_tool_failures`）。
///
/// Rust 的归档 `tool_calls` 是 `[{"name","arguments","result","error"}]`：只报
/// `error == true` 的调用，结果取**最后一行**（工具错误通常把「建议」放在末行），
/// 并截断到 200 字符。
fn format_tool_failures(tool_calls_raw: &str) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<Value>(tool_calls_raw) else {
        return Vec::new();
    };
    let Some(arr) = v.as_array() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for tc in arr {
        if tc.get("error").and_then(Value::as_bool) != Some(true) {
            continue;
        }
        let name = tc.get("name").and_then(Value::as_str).unwrap_or("?");
        let args = tc
            .get("arguments")
            .map(|a| a.to_string())
            .unwrap_or_else(|| "{}".to_string());
        let args_brief: String = if args.chars().count() > 120 {
            args.chars().take(117).collect::<String>() + "..."
        } else {
            args
        };
        let result = tc.get("result").and_then(Value::as_str).unwrap_or("");
        let brief = result.lines().rfind(|l| !l.trim().is_empty()).unwrap_or("");
        let brief: String = if brief.chars().count() > 200 {
            brief.chars().take(197).collect::<String>() + "..."
        } else {
            brief.to_string()
        };
        out.push(format!("  ⚠️ {name}({args_brief}) → {brief}"));
    }
    out
}

/// Section 4 — the recent-conversation block the 组织上下文 selection asked
/// for（原版 `_build_turns_section`: `archive.recent(loadRecentTurns)`）。
fn turns_section(mem: &Memory, n_turns: usize) -> (String, u64) {
    let Ok(recent) = mem.archive.recent(n_turns) else {
        return (String::new(), 0);
    };
    if recent.is_empty() {
        return (String::new(), 0);
    }
    let mut out = String::from("\n## Recent Conversations\n");
    for t in recent.iter().rev() {
        let at = t.at.get(..19).unwrap_or(&t.at);
        out.push_str(&format!("  [{at}] User: {}\n", t.user_msg));
        out.push_str(&format!("         AI: {}\n", t.assistant));
        for line in format_tool_failures(&t.tool_calls) {
            out.push_str(&line);
            out.push('\n');
        }
    }
    let n = recent.len() as u64;
    (out, n)
}

/// Section 5 — the 技能 documentation（原版 `_build_skills_section`:
/// `knowledge.search_keyword("document skill", limit)`）。
fn skills_section(mem: &Memory, limit: usize) -> String {
    let Ok(docs) = mem.knowledge.search_keyword("document skill", limit, "") else {
        return String::new();
    };
    if docs.is_empty() {
        return String::new();
    }
    let mut out = String::from("\n## Skills\n");
    for d in &docs {
        out.push_str(&format!("  {}: {}\n", d.name, d.summary));
    }
    out
}

/// Build the **base reference** 组织上下文 sees before searching（原版
/// `build_base`）—— constraints / KG top-10 / recent observations / recent 3
/// turns / skills —— so the stage can pick context with the institutional
/// constraints already in view.
pub fn build_base(mem: &Memory, candidates: &[(Node, f32)]) -> String {
    let mut parts = String::new();
    parts.push_str(&constraints_section_base(mem));
    if !candidates.is_empty() {
        parts.push_str("\n## Knowledge Graph (semantic top-10)\n");
        for (n, score) in candidates.iter().take(10) {
            let content = if n.content.is_empty() {
                &n.summary
            } else {
                &n.content
            };
            parts.push_str(&format!(
                "  node:{} | {} | {} | score={score:.3}\n    {content}\n",
                n.id, n.kind, n.name
            ));
        }
    }
    // Recent observations, deduped by topic, first 20（原版 `_build_base_observations`）.
    if let Ok(recent) = mem.observations.recent(50) {
        let mut seen = std::collections::HashSet::new();
        let deduped: Vec<_> = recent
            .into_iter()
            .filter(|r| seen.insert(r.topic.clone()))
            .collect();
        if !deduped.is_empty() {
            parts.push_str("\n## Observations (recent, deduped)\n");
            for o in deduped.iter().take(20) {
                parts.push_str(&format!(
                    "  {} | {}/{} | {}\n",
                    o.kind, o.topic, o.name, o.content
                ));
            }
        }
    }
    let (turns, _) = turns_section(mem, BASE_TURNS);
    parts.push_str(&turns);
    parts.push_str(&skills_section(mem, 10));
    parts
}

/// Assemble the six sections the selection asked for（原版 `assemble`）—— the
/// block that becomes 工作阶段's `{prefix}`, plus its real per-section volumes.
pub fn assemble(mem: &Memory, selection: &Value) -> AssembledContext {
    let mut out = AssembledContext::default();
    let mut text = constraints_section(mem);

    // Section 2 — the knowledge-graph nodes the stage chose to load.
    let mut kg = String::new();
    if let Some(ids) = selection.get("loadNodes").and_then(Value::as_array) {
        let mut head = false;
        for id in ids.iter().filter_map(Value::as_str) {
            let Ok(Some(n)) = mem.knowledge.get_node(id) else {
                continue;
            };
            if !head {
                kg.push_str("\n## Knowledge Graph (loaded nodes)\n");
                head = true;
            }
            kg.push_str(&node_line(&n));
        }
    }
    out.knowledge_chars = kg.chars().count() as u64;
    text.push_str(&kg);

    // Section 3 — the observation timeline the stage chose to load.
    if let Some(ids) = selection.get("loadObservations").and_then(Value::as_array) {
        let ids: Vec<String> = ids
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
        if let Ok(loaded) = mem.observations.get_by_ids(&ids)
            && !loaded.is_empty()
        {
            let mut obs = String::from("\n## Observations (timeline)\n");
            for o in &loaded {
                obs.push_str(&format!(
                    "  [{}] {}/{}: {}\n",
                    o.kind, o.topic, o.name, o.content
                ));
            }
            out.observation_chars = obs.chars().count() as u64;
            text.push_str(&obs);
        }
    }

    // Section 4 — continuity: the recent turns **from the archive**, never from
    // an in-memory buffer（每轮从空白开始，上一轮内容只在这里回来）。
    let n_turns = selection
        .get("loadRecentTurns")
        .and_then(Value::as_u64)
        .unwrap_or(BASE_TURNS as u64) as usize;
    let (turns, count) = turns_section(mem, n_turns);
    out.recent_turn_chars = turns.chars().count() as u64;
    out.recent_turns = count;
    text.push_str(&turns);

    text.push_str(&skills_section(mem, 20));
    out.text = text;
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingmiao_memory::{HashingEmbedder, Memory, Zone};
    use serde_json::json;
    use std::path::PathBuf;

    /// A temp chat-zone memory with a deterministic test embedder.
    fn temp_memory() -> (Memory, PathBuf) {
        let root =
            std::env::temp_dir().join(format!("lingmiao-ctx-{}", lingmiao_memory::short_id("t")));
        let paths = lingmiao_core::paths::Paths::at(&root);
        paths.ensure_dirs().unwrap();
        let mem = Memory::open(&paths, Zone::Chat, Some(Arc::new(HashingEmbedder::new())))
            .expect("open memory");
        (mem, root)
    }

    fn seed_observation(mem: &Memory, kind: &str, name: &str, content: &str) {
        let obs = lingmiao_memory::NewObservation {
            kind,
            topic: kind,
            name,
            content,
            keywords: "",
            turn_id: "turn-x",
            source: "test",
            stage: "test",
            topics: "[]",
        };
        mem.observations.insert(&obs).unwrap();
    }

    fn seed_turn(mem: &Memory, id: &str, user: &str, assistant: &str, tool_calls: &str) {
        let mut turn = lingmiao_memory::Turn::new(user, assistant);
        turn.id = id.to_string();
        turn.tool_calls = tool_calls.to_string();
        turn.chain_id = "chain-t".to_string();
        mem.archive.save(&turn).unwrap();
    }

    #[test]
    fn assemble_injects_the_selected_sections_with_real_volumes() {
        let (mem, root) = temp_memory();
        seed_observation(&mem, "constraint", "规则一", "必须说中文");
        let node_id = mem
            .knowledge
            .upsert_node(&lingmiao_memory::NewNode::new(
                "project",
                "lingmiao",
                "Rust 版",
                "灵妙的 Rust 实现",
            ))
            .unwrap();
        seed_turn(&mem, "turn-1", "上一轮提问", "上一轮回答", "[]");

        let assembled = assemble(
            &mem,
            &json!({
                "loadObservations": [],
                "loadNodes": [node_id],
                "loadRecentTurns": 3,
            }),
        );
        // 约束（规则一）与 KG 节点（lingmiao）都在块里。
        assert!(assembled.text.contains("必须说中文"), "{}", assembled.text);
        assert!(assembled.text.contains("lingmiao"), "{}", assembled.text);
        // 最近对话来自归档库 —— 这是无状态引擎唯一的连续性来源。
        assert!(assembled.text.contains("上一轮提问"), "{}", assembled.text);
        assert!(assembled.text.contains("上一轮回答"), "{}", assembled.text);
        assert_eq!(assembled.recent_turns, 1);
        assert!(assembled.knowledge_chars > 0);
        assert!(assembled.recent_turn_chars > 0);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn assemble_reports_zero_turns_when_the_archive_is_empty() {
        // 无状态引擎 + 空归档 = 真的没有连续性 —— 交接提示必须如实说 0，
        // 而不是复述模型请求的 `loadRecentTurns: 3`。
        let (mem, root) = temp_memory();
        let assembled = assemble(&mem, &json!({"loadRecentTurns": 3}));
        assert!(!assembled.text.contains("Recent Conversations"));
        assert_eq!(assembled.recent_turns, 0);
        assert_eq!(assembled.recent_turn_chars, 0);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn assemble_reports_failed_tool_calls_from_archived_turns() {
        // 原版 `_format_tool_failures`: 只报失败调用，且取结果的最后一行。
        let (mem, root) = temp_memory();
        seed_turn(
            &mem,
            "turn-f",
            "跑个命令",
            "好的",
            r#"[{"name":"bash","arguments":{"command":"x"},"result":"第一行\n建议: 换个写法","error":true},
                {"name":"read_file","arguments":{"path":"a"},"result":"ok","error":false}]"#,
        );
        let assembled = assemble(&mem, &json!({"loadRecentTurns": 1}));
        assert!(assembled.text.contains("⚠️ bash"), "{}", assembled.text);
        assert!(
            assembled.text.contains("建议: 换个写法"),
            "{}",
            assembled.text
        );
        // 成功调用不上报。
        assert!(
            !assembled.text.contains("⚠️ read_file"),
            "{}",
            assembled.text
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn build_base_carries_constraints_and_recent_turns() {
        let (mem, root) = temp_memory();
        seed_observation(&mem, "decision", "定案", "选了 B 方案");
        seed_turn(&mem, "turn-b", "底座提问", "底座回答", "[]");
        let base = build_base(&mem, &[]);
        assert!(base.contains("历史决策 (all)"), "{base}");
        assert!(base.contains("选了 B 方案"), "{base}");
        assert!(base.contains("底座提问"), "{base}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn parses_consolidated_summary_json() {
        let raw = json!({
            "type": "task",
            "summary": "did the thing",
            "items": [{"kind": "fact"}, {"kind": "decision"}],
            "audit": {"grade": "good", "issues": []},
            "quality": {"grade": "minor问题", "corrections": [{"type": "事实纠正"}]},
            "task": {"taskStatus": "in_progress", "nextSteps": "继续"}
        });
        let r = SummaryReport::from_json(&raw);
        assert_eq!(r.obs_type, "task");
        assert_eq!(r.observations, 2);
        assert_eq!(r.audit_grade, "good");
        assert_eq!(r.quality_grade, "minor问题");
        assert_eq!(r.corrections, 1);
        assert_eq!(r.task_status, "in_progress");
        assert_eq!(r.next_steps, "继续");
    }

    #[test]
    fn missing_fields_default() {
        let r = SummaryReport::from_json(&json!({}));
        assert_eq!(r.summary, "");
        assert_eq!(r.observations, 0);
        assert_eq!(r.audit_grade, "");
    }

    #[test]
    fn items_normalise_defaults_and_drop_body_less() {
        // F-要点记录 contract: body-less items dropped; blank kind→fact,
        // topic→kind, name→topic; trimmed fields.
        let raw = json!({
            "items": [
                {"kind": "decision", "topic": "t", "name": "n", "content": "keep me", "keywords": "k"},
                {"kind": "", "content": "no kind"},
                {"name": "only-name", "content": "no kind or topic"},
                {"kind": "fact", "content": "   "},   // whitespace-only body → dropped
                {"kind": "fact"},                       // no body → dropped
                "not-an-object"                          // malformed → dropped
            ]
        });
        let items = SummaryReport::from_json(&raw).items();
        assert_eq!(items.len(), 3, "{items:?}");
        assert_eq!(items[0].kind, "decision");
        assert_eq!(items[0].topic, "t");
        assert_eq!(items[0].name, "n");
        assert_eq!(items[0].content, "keep me");
        assert_eq!(items[0].keywords, "k");
        // blank kind → fact, blank topic → kind
        assert_eq!(items[1].kind, "fact");
        assert_eq!(items[1].topic, "fact");
        assert_eq!(items[1].name, "fact");
        // blank kind/topic, name given → kind=fact, topic=fact, name kept
        assert_eq!(items[2].kind, "fact");
        assert_eq!(items[2].name, "only-name");
    }

    #[test]
    fn items_absent_is_empty() {
        assert!(SummaryReport::from_json(&json!({})).items().is_empty());
        assert!(
            SummaryReport::from_json(&json!({"items": 3}))
                .items()
                .is_empty()
        );
    }
}

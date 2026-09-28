//! Strongly-typed per-turn context (Q7 decision A).
//!
//! The Python original threaded stage outputs through a string-keyed `dict`;
//! Q7 replaces that with a real struct so a mistyped field is a compile error
//! and each stage's typed output is explicit. The three M4 stages —
//! `组织上下文`, `工作阶段`, `沉淀阶段` — each take `&mut TurnContext` and fill
//! the field they own.

use std::sync::Arc;

use lingmiao_core::Config;
use lingmiao_llm::Message;
use lingmiao_memory::Memory;
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
pub struct TurnContext {
    /// Stable id shared by this turn's archive row, #2 observations and the MG
    /// update (原版 threaded one `turn_id` through D/F/G/H/I). Generated once.
    pub turn_id: String,
    /// The raw user input for this turn.
    pub input: String,
    /// Prior conversation history (snapshot taken before this turn).
    pub history: Vec<Message>,
    /// The chat-zone memory bundle, when wired.
    pub memory: Option<Arc<Memory>>,
    /// The tool registry the stages draw their whitelists from.
    pub registry: Arc<ToolRegistry>,
    /// Resolved configuration (prompts + stage whitelists + timeouts).
    pub cfg: Config,
    /// `组织上下文` output: the context block selected for this turn.
    pub b_context: Option<String>,
    /// §8.5 context bar: characters the B-stage retrieved from the **knowledge
    /// graph** (`search_knowledge`…), attributed by source rather than guessed.
    pub b_knowledge_chars: u64,
    /// §8.5 context bar: characters the B-stage retrieved from **observations /
    /// archive** (`search_observations`, `search_archive`, their `list_*`).
    pub b_history_chars: u64,
    /// `工作阶段` output: the assistant's answer.
    pub c_answer: Option<String>,
    /// 沉淀阶段 output.
    pub summary: Option<SummaryReport>,
}

impl TurnContext {
    /// Build a fresh context for `input`.
    pub fn new(
        input: impl Into<String>,
        history: Vec<Message>,
        memory: Option<Arc<Memory>>,
        registry: Arc<ToolRegistry>,
        cfg: Config,
    ) -> Self {
        Self {
            turn_id: lingmiao_memory::short_id("turn"),
            input: input.into(),
            history,
            memory,
            registry,
            cfg,
            b_context: None,
            b_knowledge_chars: 0,
            b_history_chars: 0,
            c_answer: None,
            summary: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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

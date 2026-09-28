//! `help` tool — 需求② §6.2.
//!
//! Exposes the embedded [`help.json`](lingmiao_core::config::embedded_help_topics)
//! document (19 topics) as a read-only tool the model can call to learn its own
//! configuration / features / tool usage:
//!
//! * no `topic` → list all topics (id + title);
//! * a known `topic` → that topic's Markdown body;
//! * an unknown `topic` → fuzzy keyword suggestions + the full topic list.
//!
//! ## Help RAG
//!
//! When an [`Embedder`] is injected (the running process's MiniLM backend — see
//! [`crate::full_registry`]), the tool also builds a **semantic index** over the
//! topics and falls back to vector retrieval when lexical matching finds
//! nothing: a natural-language query that shares no literal token with any
//! topic id / title / keyword (e.g. a paraphrase) still resolves to the closest
//! topic body by cosine similarity. The index and the query share *one*
//! embedder, so the comparison is always within a single vector space. With no
//! embedder (e.g. `default_registry` / unit tests) the tool stays purely
//! lexical — no model is loaded.
//!
//! It is registered in *every* stage whitelist (`stages.json`), so the model can
//! always consult it. Reuses the existing [`Tool`] trait + [`ToolRegistry`].

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use lingmiao_core::config::{HelpTopic, embedded_help_topics};
use lingmiao_memory::Embedder;
use lingmiao_memory::embed::cosine;
use serde::Deserialize;
use serde_json::Value;

use crate::tool::{Tool, ToolError, ToolOutput, ToolRegistry, json_schema};

/// Tool names this module contributes.
pub const TOOL_NAMES: [&str; 1] = ["help"];

/// Minimum cosine similarity for a semantic fallback hit (help RAG). Below this
/// the match is reported only as a near-miss suggestion, never as the answer.
const SEM_THRESHOLD: f32 = 0.30;

/// How many semantic near-misses to surface alongside the topic list.
const SEM_SUGGESTIONS: usize = 3;

#[derive(Deserialize, schemars::JsonSchema)]
struct HelpArgs {
    /// Topic id (e.g. `models`, `memory`, `commands`). When empty, lists all topics.
    #[serde(default)]
    topic: String,
}

/// The text indexed for one topic (id + title + keywords + body).
fn topic_text(id: &str, t: &HelpTopic) -> String {
    let mut s = String::with_capacity(t.body.len() + 64);
    s.push_str(id);
    s.push(' ');
    s.push_str(&t.title);
    s.push(' ');
    for k in &t.keywords {
        s.push_str(k);
        s.push(' ');
    }
    s.push_str(&t.body);
    s
}

/// The `help` tool.
pub struct HelpTool {
    topics: &'static HashMap<String, HelpTopic>,
    /// Query embedder for help RAG. `None` → lexical matching only.
    embedder: Option<Arc<dyn Embedder>>,
    /// Precomputed `(id, vector)` index over the topics (built once, sorted by id).
    index: Vec<(String, Vec<f32>)>,
}

impl Default for HelpTool {
    fn default() -> Self {
        Self::new()
    }
}

impl HelpTool {
    /// Build over the embedded help document, lexical only.
    pub fn new() -> Self {
        Self::with_embedder(None)
    }

    /// Build over the embedded help document, with an optional semantic index.
    ///
    /// When `embedder` is `Some`, every topic is embedded once here so a query
    /// can be cosine-ranked against them at call time (help RAG).
    pub fn with_embedder(embedder: Option<Arc<dyn Embedder>>) -> Self {
        let topics = embedded_help_topics();
        let index = match &embedder {
            Some(e) => {
                let mut v: Vec<(String, Vec<f32>)> = topics
                    .iter()
                    .map(|(id, t)| (id.clone(), e.embed(&topic_text(id, t))))
                    .collect();
                v.sort_by(|a, b| a.0.cmp(&b.0));
                v
            }
            None => Vec::new(),
        };
        Self {
            topics,
            embedder,
            index,
        }
    }

    fn render_all(&self) -> String {
        let mut ids: Vec<&String> = self.topics.keys().collect();
        ids.sort();
        let mut out = String::from("灵妙内置帮助 · 全部主题：\n");
        for id in ids {
            let t = &self.topics[id];
            out.push_str(&format!("- {id} — {}\n", t.title));
        }
        out.push_str("\n用 help 工具并指定 topic=<id> 查看正文。");
        out
    }

    fn body_of(&self, id: &str, t: &HelpTopic) -> String {
        format!("# {}  ({id})\n\n{}", t.title, t.body)
    }

    /// Case-insensitive exact id match.
    fn find_exact(&self, lower: &str) -> Option<(&String, &HelpTopic)> {
        self.topics
            .iter()
            .find(|(id, _)| id.to_ascii_lowercase() == lower)
    }

    /// Fuzzy matches: query vs id / title / keywords (both directions, ci).
    fn fuzzy(&self, query: &str) -> Vec<(&String, &HelpTopic)> {
        let q = query.to_lowercase();
        let mut hits: Vec<(&String, &HelpTopic)> = self
            .topics
            .iter()
            .filter(|(id, t)| {
                let id_l = id.to_lowercase();
                if id_l.contains(&q) || q.contains(&id_l) {
                    return true;
                }
                if t.title.to_lowercase().contains(&q) {
                    return true;
                }
                t.keywords.iter().any(|k| {
                    let k = k.to_lowercase();
                    k.contains(&q) || q.contains(&k)
                })
            })
            .collect();
        hits.sort_by(|a, b| a.0.cmp(b.0));
        hits
    }

    /// Cosine-rank every topic against `query` (highest first). Empty when no
    /// embedder was injected.
    fn semantic_rank(&self, query: &str) -> Vec<(String, f32)> {
        let (Some(e), false) = (&self.embedder, self.index.is_empty()) else {
            return Vec::new();
        };
        let qv = e.embed(query);
        let mut scored: Vec<(String, f32)> = self
            .index
            .iter()
            .map(|(id, v)| (id.clone(), cosine(&qv, v)))
            .collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored
    }
}

#[async_trait]
impl Tool for HelpTool {
    fn name(&self) -> &str {
        "help"
    }

    fn description(&self) -> &str {
        "查询灵妙内置帮助文档：了解自身配置、功能或工具用法。参数 topic 可选——不带 topic 列出全部主题；\
         带 topic 返回该主题正文；未知 topic 给出模糊匹配建议，并辅以语义检索（自然语言提问也能命中相关主题）。\
         需要了解自身配置/功能/工具用法时调用本工具，不要凭记忆猜测。"
    }

    fn parameters(&self) -> Value {
        json_schema::<HelpArgs>()
    }

    async fn execute(&self, arguments: Value) -> Result<ToolOutput, ToolError> {
        let a: HelpArgs = serde_json::from_value(arguments)
            .map_err(|e| ToolError::invalid("help", e.to_string()))?;
        let q = a.topic.trim();
        if q.is_empty() {
            return Ok(ToolOutput::ok(self.render_all()));
        }
        let lower = q.to_ascii_lowercase();
        if let Some((id, t)) = self.find_exact(&lower) {
            return Ok(ToolOutput::ok(self.body_of(id, t)));
        }
        let matches = self.fuzzy(&lower);
        if matches.len() == 1 {
            let (id, t) = matches[0];
            return Ok(ToolOutput::ok(self.body_of(id, t)));
        }
        if !matches.is_empty() {
            let mut out = format!("与 `{q}` 相关的主题：\n");
            for (id, t) in &matches {
                out.push_str(&format!("- {id} — {}\n", t.title));
            }
            out.push('\n');
            out.push_str(&self.render_all());
            return Ok(ToolOutput::ok(out));
        }

        // No lexical match → semantic fallback (help RAG over the topic bodies).
        let ranked = self.semantic_rank(q);
        if let Some((id, score)) = ranked.first() {
            if *score >= SEM_THRESHOLD {
                if let Some(t) = self.topics.get(id) {
                    let mut out = format!("（语义匹配 · 相关度 {score:.2}）\n\n");
                    out.push_str(&self.body_of(id, t));
                    return Ok(ToolOutput::ok(out));
                }
            }
        }

        // Nothing confident: report the miss, then any semantic near-misses.
        let mut out = format!("未找到与 `{q}` 匹配的主题。\n");
        let hints: Vec<(&String, f32)> = ranked
            .iter()
            .take(SEM_SUGGESTIONS)
            .filter(|(_, s)| *s > 0.0)
            .map(|(id, s)| (id, *s))
            .collect();
        if !hints.is_empty() {
            out.push_str("语义最相关：\n");
            for (id, s) in hints {
                if let Some(t) = self.topics.get(id) {
                    out.push_str(&format!("- {id}（{s:.2}）— {}\n", t.title));
                }
            }
        }
        out.push('\n');
        out.push_str(&self.render_all());
        Ok(ToolOutput::ok(out))
    }
}

/// Register the `help` tool (lexical only). Call [`register`] again with an
/// embedder to upgrade the same slot to help RAG — see [`crate::full_registry`].
pub fn register(registry: &mut ToolRegistry, embedder: Option<Arc<dyn Embedder>>) {
    registry.register(HelpTool::with_embedder(embedder));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn lists_all_topics_without_arg() {
        let tool = HelpTool::new();
        let out = tool.execute(serde_json::json!({})).await.unwrap();
        assert!(!out.is_error);
        for id in ["overview", "models", "memory", "for-ai"] {
            assert!(out.content.contains(id), "listing missing {id}");
        }
        assert!(out.content.contains("19") || out.content.contains("models"));
    }

    #[tokio::test]
    async fn exact_topic_returns_body() {
        let tool = HelpTool::new();
        let out = tool
            .execute(serde_json::json!({"topic": "models"}))
            .await
            .unwrap();
        assert!(out.content.contains("models.json"));
        assert!(out.content.contains("groups"));
    }

    #[tokio::test]
    async fn fuzzy_matches_chinese_keyword() {
        let tool = HelpTool::new();
        // "模型" is a keyword of the `models` topic.
        let out = tool
            .execute(serde_json::json!({"topic": "模型"}))
            .await
            .unwrap();
        assert!(out.content.contains("models"), "got: {}", out.content);
    }

    #[tokio::test]
    async fn unknown_topic_suggests_and_lists() {
        let tool = HelpTool::new();
        let out = tool
            .execute(serde_json::json!({"topic": "zzz-not-a-topic"}))
            .await
            .unwrap();
        assert!(out.content.contains("未找到"));
        assert!(out.content.contains("全部主题"));
    }

    /// A deterministic marker embedder for the RAG path. It only lights up the
    /// `models` topic vector (whose title carries `配置模型`), mapping a token
    /// that shares no substring with any topic id/title/keyword (`PX-9000`) onto
    /// it — so lexical matching provably fails and only the vector fallback can
    /// resolve the query.
    struct MarkerEmbedder;
    impl Embedder for MarkerEmbedder {
        fn embed(&self, text: &str) -> Vec<f32> {
            let mut v = vec![0.0f32, 0.0, 0.0];
            if text.contains("配置模型") || text == "PX-9000" {
                v[0] = 1.0;
            }
            if text.contains("记忆体系") || text == "MEM-1" {
                v[1] = 1.0;
            }
            v
        }
    }

    #[tokio::test]
    async fn semantic_fallback_resolves_lexically_unmatchable_query() {
        let tool = HelpTool::with_embedder(Some(Arc::new(MarkerEmbedder)));
        let out = tool
            .execute(serde_json::json!({"topic": "PX-9000"}))
            .await
            .unwrap();
        assert!(out.content.contains("语义匹配"), "got: {}", out.content);
        assert!(out.content.contains("配置模型"), "got: {}", out.content);
        assert!(out.content.contains("models.json"), "got: {}", out.content);
    }

    #[tokio::test]
    async fn semantic_fallback_absent_without_embedder() {
        let tool = HelpTool::new();
        let out = tool
            .execute(serde_json::json!({"topic": "PX-9000"}))
            .await
            .unwrap();
        assert!(out.content.contains("未找到"), "got: {}", out.content);
        assert!(!out.content.contains("语义匹配"), "got: {}", out.content);
    }

    #[tokio::test]
    async fn low_similarity_miss_only_lists_near_misses() {
        // A marker that lights up no topic vector → below threshold → miss.
        struct BlankEmbedder;
        impl Embedder for BlankEmbedder {
            fn embed(&self, _text: &str) -> Vec<f32> {
                vec![0.0f32, 0.0, 0.0]
            }
        }
        let tool = HelpTool::with_embedder(Some(Arc::new(BlankEmbedder)));
        let out = tool
            .execute(serde_json::json!({"topic": "totally-unrelated"}))
            .await
            .unwrap();
        assert!(out.content.contains("未找到"), "got: {}", out.content);
        assert!(!out.content.contains("语义匹配"), "got: {}", out.content);
    }

    #[tokio::test]
    async fn exact_id_still_wins_with_embedder() {
        let tool = HelpTool::with_embedder(Some(Arc::new(lingmiao_memory::HashingEmbedder::new())));
        let out = tool
            .execute(serde_json::json!({"topic": "memory"}))
            .await
            .unwrap();
        assert!(out.content.contains("记忆"), "got: {}", out.content);
        assert!(
            !out.content.contains("语义匹配"),
            "exact match short-circuits"
        );
    }

    #[test]
    fn parameters_are_object_schema() {
        let tool = HelpTool::new();
        let p = tool.parameters();
        assert_eq!(p["type"], "object");
        assert!(p["properties"]["topic"].is_object());
    }
}

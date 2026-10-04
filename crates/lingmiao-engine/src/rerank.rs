//! LLM memory re-ranker — ported from the Python original's
//! `_llm_rerank_candidates` (`tools/builtin/registration.py:17`).
//!
//! `search_memory` recalls a candidate set by hybrid semantic + keyword search;
//! this re-ranker asks the LLM to order that set by relevance (and drop the
//! irrelevant) before the tool truncates to its top-N. It lives in `lingmiao-engine`
//! because `lingmiao-tools` must **not** depend on `lingmiao-llm` — the tool only knows
//! the [`lingmiao_tools::Reranker`] trait, and the engine supplies this concrete
//! implementation.
//!
//! Failure is never fatal: any LLM / parse error is returned as `Err` and the
//! tool falls back to plain cosine (score) order — exactly the original's
//! `except → llm_error` path.

use std::collections::HashSet;

use async_trait::async_trait;
use lingmiao_llm::{ChatRequest, Client, Message};
use lingmiao_tools::{RerankCandidate, Reranker};
use serde_json::Value;

/// Sampling temperature for the re-rank call (Python parity: `0.01`).
const RERANK_TEMPERATURE: f32 = 0.01;
/// Output-token cap (Python parity: `max_tokens=500`).
const RERANK_MAX_TOKENS: u32 = 500;
/// Candidate snippet length fed to the LLM (Python parity: `[:150]`).
const SNIPPET_CHARS: usize = 150;

/// Hard budget for the whole re-rank round-trip (C 项, cli 2026-10-02).
///
/// The re-rank is a **nice-to-have** refinement: `search_memory` already has a
/// ranked candidate pool (RRF + layer quota) before the LLM sees it, and the
/// tool falls back to exactly that ordering whenever this call fails. Not
/// bounding this call therefore bought nothing and cost a lot — one provider
/// hiccup held a single `search_memory` for **103.9 s** (recall ~5 s + re-rank
/// ~99 s) on 2026-10-02, and because a batch's calls ran serially back then the
/// whole 组织上下文 stage took 180 s.
///
/// 12 s is comfortably above the normal case (the prompt is a few KB and the
/// reply is capped at 500 tokens) while keeping the worst case bounded. The
/// underlying transport already allows far more (`read_timeout` 90 s), so this
/// is a *refinement* budget, not a transport one.
const RERANK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(12);

/// The concrete [`Reranker`] backed by an [`lingmiao_llm::Client`].
pub struct LlmReranker {
    llm: Client,
}

impl LlmReranker {
    /// Wrap an LLM client (cloned from the engine's — one shared `ModelSpec`).
    pub fn new(llm: Client) -> Self {
        Self { llm }
    }
}

#[async_trait]
impl Reranker for LlmReranker {
    async fn rerank(
        &self,
        query: &str,
        candidates: &[RerankCandidate],
        top_k: usize,
    ) -> Result<Vec<usize>, String> {
        if candidates.is_empty() || top_k == 0 {
            return Ok(Vec::new());
        }
        let (system, user) = build_prompts(query, candidates, top_k);
        let mut req = ChatRequest::chat(system, vec![Message::user(user)]);
        req.temperature = RERANK_TEMPERATURE;
        req.max_tokens = Some(RERANK_MAX_TOKENS);

        // C 项: bound the refinement. A timeout becomes an ordinary `Err`, which
        // the tool already handles by falling back to the layer-quota + score
        // order — i.e. the search still returns good results, just not
        // LLM-ordered ones (see [`RERANK_TIMEOUT`]).
        let resp = match tokio::time::timeout(RERANK_TIMEOUT, self.llm.chat(req)).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => return Err(e.message().to_string()),
            Err(_) => {
                return Err(format!(
                    "rerank timed out after {}s",
                    RERANK_TIMEOUT.as_secs()
                ));
            }
        };
        // DeepSeek V4 Pro can spend all tokens on `reasoning_content` and leave
        // `content` empty — use the reasoning text as a JSON fallback (Python
        // parity).
        let mut raw = resp.content;
        if raw.trim().is_empty() {
            raw = resp.reasoning_content;
        }
        let ranked = parse_ranked(&raw)?;
        Ok(normalize_ranking(ranked, candidates, top_k))
    }
}

/// Build the re-ranker's `(system, user)` prompts (1:1 with the Python original).
fn build_prompts(query: &str, candidates: &[RerankCandidate], top_k: usize) -> (String, String) {
    let mut lines: Vec<String> = Vec::with_capacity(candidates.len());
    for (i, c) in candidates.iter().enumerate() {
        // Python: `content[:150].replace("\n", " ")`.
        let snippet: String = c
            .content
            .replace('\n', " ")
            .chars()
            .take(SNIPPET_CHARS)
            .collect();
        lines.push(format!(
            "[{i}] layer={} kind={} name={} score={:.3}\n    {snippet}",
            c.layer, c.kind, c.title, c.score
        ));
    }
    let candidate_text = lines.join("\n");

    let json_example = r#"{"ranked":[0,3,1],"reasoning":"按语义相关性和信息密度排序"}"#;
    let system = format!(
        "# 身份\n\n\
         你是记忆搜索精排器——对语义召回的候选集进行最终排序。\n\n\
         # 核心任务\n\n\
         根据用户查询对候选记忆排序：按相关性降序排列，去重，丢弃不相关项。\n\n\
         # 排序标准\n\n\
         1. 语义相关性：候选内容是否真正回答用户的问题（权重最高）\n\
         2. 信息密度：包含具体事实/决策/约束的候选优先于泛泛描述\n\
         3. 时效性：更新的记录优先，但语义相关性权重更高\n\
         4. 层优先级：约束(constraint)/决策(decision) > 事实(fact) > 对话记录(archive)\n\n\
         # 输出格式\n\n\
         CRITICAL: 只输出 JSON，不要任何其他文字。\n\
         格式示例: {json_example}\n\
         - ranked: 最多 {top_k} 个候选索引（可少于 {top_k} 如果不相关候选不足）\n\
         - reasoning: 简短说明排序依据（中文，20字以内）\n\
         - 不相关的候选直接丢弃，不要放入 ranked\n\n\
         # 约束\n\n\
         NEVER: 输出非 JSON 内容。NEVER: 包含超出 {top_k} 个候选。NEVER: 编造候选内容。"
    );
    let user = format!(
        "# 用户查询\n\n{query}\n\n\
         # 候选集（{} 条，来自 observations / knowledge / archive 三层语义召回）\n\n\
         {candidate_text}",
        candidates.len()
    );
    (system, user)
}

/// Extract the `ranked` index list from the model's reply, tolerating a JSON
/// body, a fenced block, or ```` ```json ```` fence (Python parity).
fn parse_ranked(raw: &str) -> Result<Vec<usize>, String> {
    let value = extract_json_object(raw)
        .ok_or_else(|| format!("LLM returned no JSON: {}", preview(raw)))?;
    let arr = value
        .get("ranked")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("LLM JSON has no `ranked` array: {}", preview(raw)))?;
    let ranked: Vec<usize> = arr
        .iter()
        .filter_map(|v| v.as_u64().map(|n| n as usize))
        .collect();
    if ranked.is_empty() {
        return Err(format!("LLM returned no ranked indices: {}", preview(raw)));
    }
    Ok(ranked)
}

/// Parse the outermost `{…}` object, else the first fenced block, else `None`.
fn extract_json_object(raw: &str) -> Option<Value> {
    let start = raw.find('{');
    let end = raw.rfind('}').map(|i| i + 1);
    if let (Some(s), Some(e)) = (start, end)
        && e > s
        && let Ok(v) = serde_json::from_str::<Value>(&raw[s..e])
    {
        return Some(v);
    }
    if let Some(block) = fenced_block(raw, "```json").or_else(|| fenced_block(raw, "```")) {
        return serde_json::from_str::<Value>(block.trim()).ok();
    }
    None
}

/// Contents of the first `<fence>…</fence>` block, or `None`.
fn fenced_block<'a>(raw: &'a str, fence: &str) -> Option<&'a str> {
    let open = raw.find(fence)?;
    let after = &raw[open + fence.len()..];
    let close = after.find("```")?;
    Some(&after[..close])
}

/// Map the LLM's ranked indices back to candidate slots, dropping out-of-range
/// and duplicate ids, then top up with the remaining (score-ordered) candidates
/// up to `top_k` (Python `_llm_rerank_candidates` safety net).
fn normalize_ranking(
    ranked: Vec<usize>,
    candidates: &[RerankCandidate],
    top_k: usize,
) -> Vec<usize> {
    let mut out: Vec<usize> = Vec::new();
    let mut used: HashSet<String> = HashSet::new();
    for idx in ranked {
        if idx < candidates.len() && used.insert(candidates[idx].id.clone()) {
            out.push(idx);
        }
    }
    if out.len() < top_k {
        for (i, c) in candidates.iter().enumerate() {
            if out.len() >= top_k {
                break;
            }
            if used.insert(c.id.clone()) {
                out.push(i);
            }
        }
    }
    out.truncate(top_k);
    out
}

/// Short one-line preview for an error message.
fn preview(raw: &str) -> String {
    raw.chars().take(120).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(id: &str, layer: &str, kind: &str, name: &str, score: f32) -> RerankCandidate {
        RerankCandidate {
            id: id.to_string(),
            layer: layer.to_string(),
            kind: kind.to_string(),
            title: name.to_string(),
            content: format!("content of {name}"),
            score,
        }
    }

    #[test]
    fn prompt_lists_candidates_with_layer_kind_name_score() {
        let cs = vec![
            cand("o1", "observations", "decision", "picked rust", 0.8),
            cand("k1", "knowledge", "technology", "rust", 0.7),
        ];
        let (system, user) = build_prompts("rust vs go", &cs, 10);
        assert!(system.contains("记忆搜索精排器"));
        assert!(system.contains("只输出 JSON"));
        assert!(system.contains("最多 10 个候选索引"));
        assert!(user.contains("rust vs go"));
        assert!(user.contains("# 候选集（2 条"));
        assert!(user.contains("[0] layer=observations kind=decision name=picked rust score=0.800"));
        assert!(user.contains("[1] layer=knowledge kind=technology name=rust score=0.700"));
    }

    #[test]
    fn parse_ranked_reads_body_json_and_fenced_block() {
        assert_eq!(
            parse_ranked(r#"{"ranked":[2,0,1],"reasoning":"x"}"#).unwrap(),
            vec![2, 0, 1]
        );
        // Prose around the JSON still parses (outermost object).
        assert_eq!(
            parse_ranked("think… {\"ranked\":[1]} done").unwrap(),
            vec![1]
        );
        // Fenced block fallback when the body is not valid JSON.
        let fenced = "note { broken\n```json\n{\"ranked\":[0,2]}\n```\n";
        assert_eq!(parse_ranked(fenced).unwrap(), vec![0, 2]);
    }

    #[test]
    fn parse_ranked_errors_on_missing_or_empty() {
        assert!(parse_ranked("just prose").is_err());
        assert!(parse_ranked(r#"{"ranked":[]}"#).is_err());
        assert!(parse_ranked(r#"{"reasoning":"no ranked key"}"#).is_err());
    }

    #[test]
    fn normalize_drops_out_of_range_and_dups_then_tops_up() {
        let cs = vec![
            cand("a", "observations", "fact", "a", 0.9),
            cand("b", "observations", "fact", "b", 0.8),
            cand("c", "knowledge", "fact", "c", 0.7),
            cand("d", "archive", "turn", "d", 0.3),
        ];
        // [2,2,9] → keep 2, drop the dup 2 and out-of-range 9; top up 0,1,3.
        assert_eq!(normalize_ranking(vec![2, 2, 9], &cs, 4), vec![2, 0, 1, 3]);
        // A short ranking is topped up to `top_k` from the remaining order.
        assert_eq!(normalize_ranking(vec![3], &cs, 3), vec![3, 0, 1]);
        // `top_k` caps the output.
        assert_eq!(normalize_ranking(vec![0, 1, 2, 3], &cs, 2), vec![0, 1]);
    }
}

//! LLM 调用失败分类与诊断面板 — 原版 `llm/error_classify.py` 的 Rust 落地.
//!
//! Provider 层抛出的失败被归类为面向用户可理解的类别，并可生成携带调用参数
//! 的诊断面板（用户可见 + 结构化日志）。
//!
//! 分类优先级（高 → 低）：
//!   413 / 明确的超限短语 → [`CTX_OVERFLOW`]   上下文超限（长程任务最常见）
//!   choices（空响应）      → [`EMPTY_RESPONSE`] 空响应（先于限流判断）
//!   429 / rate limit      → [`RATE_LIMIT`]     限流
//!   401 / 403 / auth      → [`AUTH`]           认证/权限失败
//!   网络类关键词           → [`NETWORK`]        网络错误
//!   5xx                   → [`UPSTREAM`]       服务端异常
//!   400                   → [`BAD_REQUEST`]    请求无效
//!   其余                   → [`UNKNOWN`]        未知错误
//!
//! Rust has no exceptions to introspect, so classification keys off an explicit
//! HTTP status (when available) plus the error text — see [`classify_llm_error`].
//!
//! Following M6.4「真源不抄」the category set, labels and advice are code
//! constants — one place, reviewed beside the classifier.

use serde_json::{Value, json};

use crate::client::Client;
use crate::message::Message;

/// 上下文超限（413 / context 关键词）。
pub const CTX_OVERFLOW: &str = "context_overflow";
/// 限流（429 / rate limit）。
pub const RATE_LIMIT: &str = "rate_limit";
/// 认证/权限失败（401/403 / auth）。
pub const AUTH: &str = "auth";
/// 网络错误（连接/超时/代理）。
pub const NETWORK: &str = "network";
/// 服务端异常（5xx）。
pub const UPSTREAM: &str = "upstream";
/// 请求无效（400）。
pub const BAD_REQUEST: &str = "bad_request";
/// 空响应（无 choices）。
pub const EMPTY_RESPONSE: &str = "empty_response";
/// 未知错误。
pub const UNKNOWN: &str = "unknown";

/// Every category, in priority order (mirrors the classifier's checks).
pub const CATEGORIES: [&str; 8] = [
    CTX_OVERFLOW,
    EMPTY_RESPONSE,
    RATE_LIMIT,
    AUTH,
    NETWORK,
    UPSTREAM,
    BAD_REQUEST,
    UNKNOWN,
];

/// Human-readable label per category.
pub fn category_label(category: &str) -> &'static str {
    match category {
        CTX_OVERFLOW => "上下文超限",
        RATE_LIMIT => "限流/频率限制",
        AUTH => "认证/权限失败",
        NETWORK => "网络错误",
        UPSTREAM => "服务端异常(5xx)",
        BAD_REQUEST => "请求无效(400)",
        EMPTY_RESPONSE => "空响应",
        _ => "未知错误",
    }
}

/// Actionable advice per category.
pub fn category_advice(category: &str) -> &'static str {
    match category {
        CTX_OVERFLOW => {
            "长程任务常见：上下文累积超限。建议新开对话/精简上下文，或缩短本轮工具结果。"
        }
        RATE_LIMIT => "请求过于频繁或触发配额。建议稍后重试，或降低并发。",
        AUTH => "API Key 无效、过期或权限不足。请检查 .env 或当前配置的 Key。",
        NETWORK => "连接失败/超时/代理异常。请检查网络与代理设置（socks/proxy）。",
        UPSTREAM => "服务商临时故障。建议稍后重试。",
        BAD_REQUEST => "请求参数非法（非上下文类）。请检查模型名/参数。",
        EMPTY_RESPONSE => "API 返回空结果，可能是服务端异常或限流。建议稍后重试。",
        _ => "未知异常，请把日志中的 llm_fatal 记录发回排查。",
    }
}

/// 一次 LLM 调用失败的分类结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmErrorInfo {
    /// Category id (one of [`CATEGORIES`]).
    pub category: &'static str,
    /// Human label.
    pub label: &'static str,
    /// HTTP status code, when known.
    pub status_code: Option<u16>,
    /// Truncated server-side error detail.
    pub detail: String,
    /// Actionable advice.
    pub advice: &'static str,
}

/// Truncate to `limit` characters, appending a cut note (Python parity).
fn truncate(s: &str, limit: usize) -> String {
    let n = s.chars().count();
    if n <= limit {
        return s.to_string();
    }
    let head: String = s.chars().take(limit).collect();
    format!("{head}...(截断 {} 字符)", n - limit)
}

/// 按 HTTP 状态码 + 错误文本对一次 LLM 失败分类。
///
/// `status` is the HTTP status when the failure came from a response; `None`
/// for transport-level failures and mid-stream errors.
pub fn classify_llm_error(status: Option<u16>, detail: &str) -> LlmErrorInfo {
    let lowered = detail.to_lowercase();
    let has = |needle: &str| lowered.contains(needle);

    // 1. 上下文超限 —— 长程任务最可疑（413 或**明确的**超限措辞）
    //
    // cli 2026-09-28: 旧判定 `has("context") && (length|token|exceed|maximum|limit
    // 任一)` 把任何提到 context/token 的 400 都套成「上下文超限」，实测组织上下文阶段
    // input=3117、沉淀阶段 input=12777 这种**量级不可能超窗口**的请求也被误标
    // （页面显示的 31.5M 也一样）。收紧为：仅 413、或 provider 明确报出
    // 「maximum context length / context length is N / too many tokens /
    // context_length_exceeded」等**带数量的超限短语**才算 CTX_OVERFLOW；
    // 其余提到 context 的 400 一律归 BAD_REQUEST（诚实分类）。
    let ctx_kw = has("maximum context length")
        || has("context length is")
        || has("context_length_exceeded")
        || has("reduce the length of the messages")
        || has("too many tokens")
        || (has("context") && (has("exceed") || has("overflow")));
    let category: &'static str = if status == Some(413) || ctx_kw {
        CTX_OVERFLOW
    }
    // 2. 空响应（无 choices）—— 须先于限流判断，因为其消息含 "rate limit" 字样。
    else if has("choices") {
        EMPTY_RESPONSE
    }
    // 3. 限流
    else if status == Some(429) || has("rate limit") || has("rate_limit") || has("requests per") {
        RATE_LIMIT
    }
    // 4. 认证 / 权限
    else if matches!(status, Some(401) | Some(403))
        || has("authentication")
        || has("unauthorized")
        || has("invalid api key")
        || has("permission")
    {
        AUTH
    }
    // 5. 网络错误（无状态码时按文本判断）
    else if status.is_none()
        && [
            "connect",
            "timeout",
            "proxy",
            "eof",
            "reset",
            "unreachable",
            "dns",
        ]
        .iter()
        .any(|k| has(k))
    {
        NETWORK
    }
    // 6. 上游 5xx
    else if matches!(status, Some(c) if (500..=599).contains(&c)) {
        UPSTREAM
    }
    // 7. 请求无效 400
    else if status == Some(400) || has("badrequest") {
        BAD_REQUEST
    } else {
        UNKNOWN
    };

    LlmErrorInfo {
        category,
        label: category_label(category),
        status_code: status,
        detail: truncate(detail, 400),
        advice: category_advice(category),
    }
}

/// 收集一次 LLM 调用的关键参数，用于失败诊断（日志 + 用户面板）。
///
/// 从 [`Client`] 上尽力提取 provider/model/base_url 及请求规模指标。`client`
/// 为 `None`（构造失败）时字段以 `?` 兜底，任何环境都能安全调用。
pub fn collect_llm_params(
    client: Option<&Client>,
    system: &str,
    messages: &[Message],
    tools: usize,
    stream: bool,
) -> Value {
    let system_chars = system.chars().count();
    let mut messages_chars = 0usize;
    for m in messages {
        if let Some(c) = &m.content {
            messages_chars += c.chars().count();
        }
        if !m.tool_calls.is_empty() {
            messages_chars += serde_json::to_string(&m.tool_calls)
                .map(|s| s.chars().count())
                .unwrap_or(0);
        }
    }
    let est_tokens = Client::estimate_tokens(system) + messages_chars / 3;

    let (provider, model, base_url) = match client {
        Some(c) => (
            c.group_id().to_string(),
            c.model().to_string(),
            c.spec().base_url.clone(),
        ),
        None => ("?".to_string(), "?".to_string(), "?".to_string()),
    };

    json!({
        "provider": provider,
        "model": model,
        "base_url": base_url,
        "system_chars": system_chars,
        "messages": messages.len(),
        "messages_chars": messages_chars,
        "est_tokens": est_tokens,
        "tools": tools,
        "thinking": true,
        "stream": stream,
    })
}

/// 生成面向用户的详细诊断面板（含分类 + 全部调用参数）。
pub fn format_error_panel(info: &LlmErrorInfo, params: &Value, raw: &str, header: &str) -> String {
    let mut lines = vec![format!("[{header} · {}]", info.label)];
    if !info.detail.is_empty() {
        lines.push(format!("原因: {}", info.detail));
    }
    lines.push("参数:".to_string());
    if let Value::Object(map) = params {
        for (k, v) in map {
            let sv = match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            lines.push(format!("  {k}: {sv}"));
        }
    }
    if let Some(code) = info.status_code {
        lines.push(format!("  status_code: {code}"));
    }
    if !raw.is_empty() && raw != info.detail {
        lines.push(format!("raw: {}", truncate(raw, 300)));
    }
    lines.push(format!("建议: {}", info.advice));
    lines.join("\n")
}

/// 最终回复用的简短摘要。
pub fn format_error_summary(info: &LlmErrorInfo) -> String {
    format!("（AI 服务调用失败 · {}）", info.label)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_priority_order() {
        // context overflow wins over everything.
        assert_eq!(
            classify_llm_error(Some(413), "payload too large").category,
            CTX_OVERFLOW
        );
        assert_eq!(
            classify_llm_error(Some(400), "maximum context length is 128k tokens").category,
            CTX_OVERFLOW
        );
        // empty response beats rate limit (the deepseek message quirk).
        assert_eq!(
            classify_llm_error(None, "possible API error or rate limit: no choices").category,
            EMPTY_RESPONSE
        );
        assert_eq!(
            classify_llm_error(Some(429), "slow down").category,
            RATE_LIMIT
        );
        assert_eq!(classify_llm_error(Some(401), "bad key").category, AUTH);
        assert_eq!(
            classify_llm_error(None, "connection reset by peer").category,
            NETWORK
        );
        assert_eq!(classify_llm_error(Some(503), "upstream").category, UPSTREAM);
        assert_eq!(
            classify_llm_error(Some(400), "invalid model").category,
            BAD_REQUEST
        );
        assert_eq!(classify_llm_error(None, "???").category, UNKNOWN);
    }

    #[test]
    fn context_word_alone_is_not_overflow() {
        // cli 2026-09-28: 早前判定把任何含 context 的 400 都套成「上下文超限」，
        // 实测 token 量级根本不可能超窗口（组织上下文阶段 input=3117 亦然）。只有 413
        // 或**明确的超限短语**才算 CTX_OVERFLOW，其余含 context 的 400 归
        // BAD_REQUEST —— 诚实分类，不制造误报。
        assert_eq!(
            classify_llm_error(
                Some(400),
                "Invalid parameter: context is required for this request"
            )
            .category,
            BAD_REQUEST
        );
        assert_eq!(
            classify_llm_error(Some(400), "unsupported context type").category,
            BAD_REQUEST
        );
        // …but a real, quantified overflow message still classifies as such.
        assert_eq!(
            classify_llm_error(
                Some(400),
                "This model's maximum context length is 65536 tokens"
            )
            .category,
            CTX_OVERFLOW
        );
        assert_eq!(
            classify_llm_error(Some(400), "context_length_exceeded").category,
            CTX_OVERFLOW
        );
    }

    #[test]
    fn labels_and_advice_cover_every_category() {
        for c in CATEGORIES {
            if c != UNKNOWN {
                assert_ne!(category_label(c), "未知错误", "label for {c}");
            }
            assert!(!category_advice(c).is_empty(), "advice for {c}");
        }
        assert_eq!(category_label(UNKNOWN), "未知错误");
    }

    #[test]
    fn detail_is_truncated() {
        let long = "x".repeat(500);
        let info = classify_llm_error(None, &long);
        assert!(info.detail.chars().count() < 500);
        assert!(info.detail.contains("截断"));
    }

    #[test]
    fn summary_and_panel_render() {
        let info = classify_llm_error(Some(429), "rate limit exceeded");
        assert_eq!(info.label, "限流/频率限制");
        assert!(format_error_summary(&info).contains("限流"));
        let params = json!({"model": "deepseek-v4", "messages": 3});
        let panel = format_error_panel(&info, &params, "rate limit exceeded", "AI 服务调用失败");
        assert!(panel.contains("AI 服务调用失败 · 限流/频率限制"), "{panel}");
        assert!(panel.contains("model: deepseek-v4"), "{panel}");
        assert!(panel.contains("建议:"), "{panel}");
    }
}

//! The **model** wait judge（F 项，cli 2026-10-05 拍板）.
//!
//! cli 的口径是「每 5 秒查询一次状态，然后把信息发给模型来决策判断」——判定
//! 停摆不由代码自行裁定，而是把 [`WaitState`] 喂给模型，由模型说「继续等 / 中断」。
//!
//! 除 ①「等模型吐字」那一处（cli 拍板接受例外：被等的就是模型自己，同一根电话线
//! 问不出去，由 [`lingmiao_core::polling::CodeStall`] 判）之外，其余等待点都用这个
//! [`ModelJudge`]。
//!
//! ## 三条纪律
//!
//! 1. **便宜**：只问一句、只要一个词，`max_tokens` 收紧、`temperature=0`，
//!    并且把 `tools` 全部关掉（决策调用绝不该触发工具）。
//! 2. **不阻塞主等待**：决策调用自带 `DECIDE_TIMEOUT`（默认 20s），超时/报错一律
//!    **保守判「继续」**——裁判自己都超时了，没有理由因此掐断被等的正事。
//! 3. **不吞错误**：异常只记 `tracing::warn`，绝不把裁判故障当成停摆证据。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use lingmiao_core::polling::{Verdict, WaitJudge, WaitState};

use crate::{ChatRequest, Client};

/// Default budget for one decision round-trip.
pub const DECIDE_TIMEOUT: Duration = Duration::from_secs(20);

/// Ask the model whether a silent wait should continue.
///
/// `client` is the stage's own client (so the decision travels the same API source
/// the stage uses). One client is created per stage in the engine, so this is not
/// a hot path — the judge is only consulted after `ask_after` of silence.
pub struct ModelJudge {
    client: Client,
    timeout: Duration,
}

impl ModelJudge {
    /// Judge backed by `client`.
    pub fn new(client: Client) -> Self {
        Self {
            client,
            timeout: DECIDE_TIMEOUT,
        }
    }

    /// Override the decision round-trip budget.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Ready-to-share handle.
    pub fn handle(client: Client) -> Arc<dyn WaitJudge> {
        Arc::new(Self::new(client))
    }
}

/// The verdict prompt. Deliberately terse: one question, one word back.
fn decide_prompt(state: &WaitState) -> String {
    format!(
        "系统正在等待一个外部动作完成，刚刚采集到它的状态：\n\n{}\n\n\
         请判断应当「继续等待」还是「中断等待」。\n\
         判断依据：若这是正常耗时（大文件读写、长命令、模型正在生成），继续等待；\
         若看起来已经卡死（长时间完全没有新数据、对端无响应），则中断。\n\
         只回答一个词：继续 或 中断。\n\
         如果选择中断，另起一行用一句话说明理由。",
        state.summary()
    )
}

/// Parse the one-word verdict out of a model reply.
///
/// Conservative by construction: anything that is not an explicit interrupt is
/// read as "keep waiting" — a garbled judge reply must never kill a live wait.
pub fn parse_verdict(reply: &str) -> Verdict {
    let head: &str = reply.lines().next().unwrap_or("");
    let normalized = head.replace(['，', ',', '。', '.', ' ', '\t'], "");
    if normalized.contains("中断") || normalized.contains("abort") {
        let reason = reply
            .lines()
            .skip(1)
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("模型判定已停摆")
            .trim_start_matches(['-', '*', ' '])
            .to_string();
        return Verdict::Interrupt(if reason.is_empty() {
            "模型判定已停摆".to_string()
        } else {
            reason
        });
    }
    Verdict::Continue
}

#[async_trait::async_trait]
impl WaitJudge for ModelJudge {
    async fn judge(&self, state: &WaitState) -> Verdict {
        let mut req = ChatRequest::chat(decide_prompt(state), Vec::new());
        // No tools, no thinking, deterministic, one word out.
        req.tools = None;
        req.tool_choice = "none".to_string();
        req.temperature = 0.0;
        req.max_tokens = Some(64);
        let fut = self.client.chat(req);
        match tokio::time::timeout(self.timeout, fut).await {
            Ok(Ok(resp)) => {
                let verdict = parse_verdict(&resp.content);
                // 每次真发问都留一条日志（含状态与结论）：等待是被谁、按什么理由
                // 结束的，事后可从日志直接对账，不必靠猜。
                tracing::info!(
                    what = %state.what,
                    class = state.class.as_str(),
                    elapsed_s = state.elapsed.as_secs(),
                    silent_s = state.silent.as_secs(),
                    verdict = match &verdict {
                        Verdict::Continue => "继续等".to_string(),
                        Verdict::Interrupt(r) => format!("中断：{r}"),
                    },
                    "wait judge ruling"
                );
                verdict
            }
            Ok(Err(e)) => {
                tracing::warn!(
                    "wait judge: decision call failed, continuing ({})",
                    e.message()
                );
                Verdict::Continue
            }
            Err(_) => {
                tracing::warn!(
                    "wait judge: decision call exceeded {}s, continuing",
                    self.timeout.as_secs()
                );
                Verdict::Continue
            }
        }
    }
}

/// Build a per-stage judge map from `stage -> resolved client`.
///
/// The engine holds one `Client` per routed stage; each gets its own judge so a
/// stage's decision travels the same provider the stage itself uses.
pub fn judges_by_stage<'a, I>(clients: I) -> HashMap<String, Arc<dyn WaitJudge>>
where
    I: IntoIterator<Item = (&'a String, &'a Client)>,
{
    clients
        .into_iter()
        .map(|(stage, client)| (stage.clone(), ModelJudge::handle(client.clone())))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Dialect;
    use lingmiao_core::polling::WaitClass;

    fn state() -> WaitState {
        WaitState {
            class: WaitClass::Command,
            what: "bash: cargo build".into(),
            elapsed: Duration::from_secs(120),
            silent: Duration::from_secs(45),
            sync: None,
        }
    }

    #[test]
    fn prompt_folds_in_a_synchronous_wait_when_one_is_in_flight() {
        // ⑧ sqlite's busy handler cannot ask a model itself; it publishes itself
        // and the async judge's prompt carries it — otherwise the model would be
        // told only about the thing it was nominally watching.
        let s = WaitState {
            sync: Some("sqlite 锁等待（observations.db）（已等 12s）".into()),
            ..state()
        };
        assert!(
            decide_prompt(&s).contains("sqlite 锁等待"),
            "{}",
            decide_prompt(&s)
        );
        assert!(s.summary().contains("同步等待"), "{}", s.summary());
    }

    #[test]
    fn prompt_carries_the_state_and_asks_for_one_word() {
        let p = decide_prompt(&state());
        assert!(p.contains("bash: cargo build"), "{p}");
        assert!(p.contains("外部命令"), "{p}");
        assert!(p.contains("120"), "elapsed is shown: {p}");
        assert!(p.contains("45"), "silence is shown: {p}");
        assert!(p.contains("继续 或 中断"), "{p}");
    }

    #[test]
    fn continue_is_the_default_for_everything_but_an_explicit_interrupt() {
        assert!(matches!(parse_verdict("继续"), Verdict::Continue));
        assert!(matches!(parse_verdict("continue"), Verdict::Continue));
        assert!(matches!(parse_verdict(""), Verdict::Continue));
        // Garbled / off-format replies never kill a live wait.
        assert!(matches!(
            parse_verdict("我不知道该怎么判断"),
            Verdict::Continue
        ));
        assert!(matches!(parse_verdict("``` json\n"), Verdict::Continue));
    }

    #[test]
    fn interrupt_carries_the_reason_line() {
        match parse_verdict("中断\n对端 45 秒无任何响应，疑似卡死") {
            Verdict::Interrupt(r) => assert!(r.contains("45"), "{r}"),
            Verdict::Continue => panic!("explicit interrupt must be honoured"),
        }
        // No reason line → a sensible default, never an empty string.
        match parse_verdict("中断") {
            Verdict::Interrupt(r) => assert_eq!(r, "模型判定已停摆"),
            Verdict::Continue => panic!("explicit interrupt must be honoured"),
        }
    }

    #[test]
    fn a_bad_client_never_turns_into_an_interrupt() {
        // The judge against an unreachable provider must answer "continue" —
        // a broken judge is not evidence that the waited-on work is stuck.
        let spec = crate::provider::ModelSpec::new(
            Dialect::OpenAi,
            "sk-test",
            "http://127.0.0.1:9",
            "no-such-model",
        );
        let client = Client::new(spec).unwrap();
        let judge = ModelJudge::new(client).with_timeout(Duration::from_millis(500));
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let verdict = rt.block_on(async { judge.judge(&state()).await });
        assert!(matches!(verdict, Verdict::Continue));
    }
}

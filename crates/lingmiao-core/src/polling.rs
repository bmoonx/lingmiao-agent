//! Unified wait-polling primitive（F 项，cli 2026-10-05 拍板）.
//!
//! 此前全工程 9 处等待点都是「一次等到死」的阻塞式超时（`read_timeout(90s)` /
//! `timeout(600s, …)` / `busy_timeout(10s)` …），等待期间没有任何反馈。cli 的口径是：
//!
//! > 每 5 秒查询一次状态，然后把信息发给模型来决策判断。
//!
//! 本模块就是这句话的**唯一实现**：一把钥匙，9 处共用。
//!
//! * [`polled`] 把任意 future 放进一个 5s 心跳里跑；每跳采集一次 [`WaitState`]
//!   （在等什么 / 等了多久 / 距上次进展多久），并交给 [`WaitJudge`] 决策。
//! * [`Progress`] 是「进展时钟」：被等的代码每收到一点新东西就 [`Progress::touch`]
//!   一次，`silent` 因而能反映「真的卡住了」而不是「本来就在等」。
//! * 决策**不是**每 5s 都问：只有 `silent ≥ ask_after`（默认 30s）才真发问——
//!   这是 cli 拍板时敲定的成本闸（等 3 分钟本会变成 36 次额外模型调用；加闸后
//!   正常快的情况一次都不问）。
//! * 判定为中断时，被等的 future 被**丢弃**（`kill_on_drop` / drop-in-cancel），
//!   调用方拿到 [`WaitAborted`] 并如实报错。
//!
//! ## 两类裁判
//!
//! * **模型裁判**（[`WaitJudge`] 的实现，在 `lingmiao-llm::decide`）：把状态喂给模型，
//!   由模型说 continue / abort。除 ① 以外 8 处都用它。
//! * **代码裁判**（[`CodeStall`]）：①「等模型吐字」这一处的被等对象**就是模型自己**
//!   ——卡住时再问它「还等不等」等于同一根电话线问不出去（cli 2026-10-05 拍板接受
//!   此处例外）。⑧ sqlite busy 亦为同步回调，无法 await，同走代码判定。

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::events::Event;

/// How often the wait state is sampled (cli: 「每 5 秒查询一次状态」).
pub const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// How long a wait may stay silent before the judge is actually consulted.
///
/// Cost gate (cli 拍板时确认): asking every 5s would turn a 3-minute wait into
/// 36 extra model calls. Nothing is asked while real progress is arriving.
pub const ASK_AFTER: Duration = Duration::from_secs(30);

/// Which class of thing is being waited on — the original 9 wait points fall
/// into four groups, and the class is part of what the model is told.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitClass {
    /// 等模型（① LLM 流、⑨ 阶段内单次往返）.
    Model,
    /// 等外部命令（⑤ bash、⑥ verify、⑦ ripgrep）.
    Command,
    /// 等网络服务（③ MCP attach、④ MCP call_tool）.
    Network,
    /// 等库 / 整阶段（⑧ sqlite busy、② 阶段总预算）.
    Store,
}

impl WaitClass {
    /// Stable wire label (also shown to the judge model).
    pub fn as_str(self) -> &'static str {
        match self {
            WaitClass::Model => "模型",
            WaitClass::Command => "外部命令",
            WaitClass::Network => "网络服务",
            WaitClass::Store => "数据库/阶段",
        }
    }
}

/// Poll cadence + the cost gate.
#[derive(Debug, Clone, Copy)]
pub struct PollPolicy {
    /// Heartbeat period (状态采集间隔).
    pub interval: Duration,
    /// Silence threshold before the judge is consulted (成本闸).
    pub ask_after: Duration,
}

impl Default for PollPolicy {
    fn default() -> Self {
        Self {
            interval: POLL_INTERVAL,
            ask_after: ASK_AFTER,
        }
    }
}

impl PollPolicy {
    /// The standard 5s / 30s policy.
    pub fn standard() -> Self {
        Self::default()
    }
}

/// A shared "progress clock" for one wait.
///
/// The waited-on code calls [`Progress::touch`] whenever it really made headway
/// (a stream delta arrived, a tool printed a line, …). `silent_for` is therefore
/// the honest "how long has nothing happened" — the number the judge reasons
/// about. An untouched `Progress` never advances, so a wait that was stuck from
/// the very start is recognisable instead of looking like progress.
#[derive(Debug)]
pub struct Progress {
    started: Instant,
    last: AtomicU64,
}

impl Progress {
    /// Start a clock now.
    pub fn new() -> Arc<Self> {
        let now = Instant::now();
        Arc::new(Self {
            started: now,
            last: AtomicU64::new(0),
        })
    }

    /// Record that something *did* happen.
    pub fn touch(&self) {
        self.last
            .store(self.started.elapsed().as_millis() as u64, Ordering::Relaxed);
    }

    /// Total time since the wait began.
    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// Time since the last [`Progress::touch`] (== elapsed when never touched).
    pub fn silent_for(&self) -> Duration {
        let touched = Duration::from_millis(self.last.load(Ordering::Relaxed));
        self.started.elapsed().saturating_sub(touched)
    }
}

/// One sampled snapshot, handed to the judge.
#[derive(Debug, Clone)]
pub struct WaitState {
    /// What class of thing is being waited on.
    pub class: WaitClass,
    /// Human-readable subject (e.g. `bash: cargo build`).
    pub what: String,
    /// How long the wait has run.
    pub elapsed: Duration,
    /// How long since the last sign of progress.
    pub silent: Duration,
    /// A **synchronous** wait in flight elsewhere in this process (⑧ sqlite's
    /// busy handler), when there is one.
    ///
    /// F 项: that wait physically cannot ask a model itself (a plain `fn`
    /// callback inside SQLite), so it only publishes itself. Folding it in here
    /// is what lets the *async* judge — which can ask a model — see that the
    /// database, not the thing it was nominally watching, is what is blocking.
    pub sync: Option<String>,
}

impl WaitState {
    /// One-line digest used in the abort message and the judge prompt.
    pub fn summary(&self) -> String {
        let mut s = format!(
            "{}（在等 {}）已等待 {:.1}s，距上次进展 {:.1}s",
            self.what,
            self.class.as_str(),
            self.elapsed.as_secs_f64(),
            self.silent.as_secs_f64(),
        );
        if let Some(sync) = &self.sync {
            s.push_str(&format!("；另有同步等待：{sync}"));
        }
        s
    }
}

/// The judge's ruling.
#[derive(Debug, Clone)]
pub enum Verdict {
    /// Keep waiting; the next heartbeat samples again.
    Continue,
    /// Stop waiting — drop the future and report `reason`.
    Interrupt(String),
}

/// Who decides whether a silent wait continues.
///
/// Async so a model-backed judge can be plugged in; see `lingmiao-llm::decide::ModelJudge`.
#[async_trait::async_trait]
pub trait WaitJudge: Send + Sync {
    /// Rule on one sampled state.
    async fn judge(&self, state: &WaitState) -> Verdict;
}

/// The **code** judge: interrupt once the silence exceeds `after`.
///
/// Used only where a model judge is physically impossible:
/// ① the wait is *for the model itself*（cli 2026-10-05 拍板接受的例外）, and
/// ⑧ sqlite's busy handler is a synchronous callback that cannot `await`.
pub struct CodeStall {
    /// Silence threshold.
    pub after: Duration,
}

impl CodeStall {
    /// Interrupt after `after` of no progress.
    pub fn new(after: Duration) -> Self {
        Self { after }
    }

    /// Ready-to-share handle.
    pub fn handle(after: Duration) -> Arc<dyn WaitJudge> {
        Arc::new(Self::new(after))
    }
}

#[async_trait::async_trait]
impl WaitJudge for CodeStall {
    async fn judge(&self, state: &WaitState) -> Verdict {
        if state.silent >= self.after {
            Verdict::Interrupt(format!(
                "连续 {:.0}s 无任何进展，判定停摆",
                state.silent.as_secs_f64()
            ))
        } else {
            Verdict::Continue
        }
    }
}

/// A wait the judge (or the code) cut short.
#[derive(Debug, Clone)]
pub struct WaitAborted {
    /// The subject that was being waited on.
    pub what: String,
    /// Why it was cut short.
    pub reason: String,
    /// How long it had been waiting.
    pub elapsed: Duration,
}

impl WaitAborted {
    /// One-line message for logs / tool results / `LingmiaoError`.
    pub fn message(&self) -> String {
        format!(
            "已中断等待 {}（已等 {:.1}s）：{}",
            self.what,
            self.elapsed.as_secs_f64(),
            self.reason
        )
    }
}

/// Run `fut` under the 5s poll loop, consulting `judge` on silent heartbeats.
///
/// * `judge == None` → no polling at all, `fut` runs to completion (tests and
///   judge-less call sites stay byte-for-byte identical).
/// * The judge is only consulted when `silent ≥ policy.ask_after` (cost gate).
/// * On [`Verdict::Interrupt`] the future is dropped immediately and
///   [`WaitAborted`] is returned — callers must surface it honestly rather than
///   silently reusing a half-finished result.
/// * **Every heartbeat is published** as [`Event::WaitPolled`] on the bus armed
///   by [`with_wait_bus`], so the wait is visible in the UI (cli 2026-10-05
///   「要进界面的，这是核心体验」) instead of living only in the log.
pub async fn polled<T, F>(
    policy: PollPolicy,
    class: WaitClass,
    what: impl Into<String>,
    progress: Arc<Progress>,
    judge: Option<Arc<dyn WaitJudge>>,
    fut: F,
) -> Result<T, WaitAborted>
where
    F: Future<Output = T>,
{
    let what = what.into();
    let Some(judge) = judge else {
        // No polling → nothing to report. Publishing here (an event per waited
        // call even when nobody is watching the wait) would be new noise for a
        // path that is supposed to be untouched.
        return Ok(fut.await);
    };
    let mut fut = std::pin::pin!(fut);
    let mut ticker = tokio::time::interval(policy.interval);
    // A slow consumer must not make the ticker fire in a burst.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // `interval` yields immediately on the first tick; consume it so the first
    // sample really is one period later.
    ticker.tick().await;
    loop {
        tokio::select! {
            out = &mut fut => {
                // The wait ended: one closing report, so the UI can replace its
                // live line with a settled "how long" line. Only for waits long
                // enough that a heartbeat had actually been shown — a 300 ms
                // tool call must not push a "0.3s" line into the transcript.
                let state = sample(class, what.clone(), &progress);
                if state.elapsed >= policy.interval {
                    report(class, &what, &state, PHASE_DONE, "");
                }
                return Ok(out);
            }
            _ = ticker.tick() => {
                let state = sample(class, what.clone(), &progress);
                if state.silent < policy.ask_after {
                    // Below the cost gate: collected, not asked. Reported only
                    // when this whole period produced **nothing** — a wait that
                    // is visibly streaming does not need a "still waiting" line
                    // every 5s, while one that has gone quiet does. (That is
                    // exactly the case the user cannot otherwise tell apart
                    // from a hang: cli 2026-10-05.)
                    if state.silent >= policy.interval {
                        report(class, &what, &state, PHASE_SAMPLING, "");
                    }
                    continue;
                }
                report(class, &what, &state, PHASE_ASKING, "");
                match judge.judge(&state).await {
                    Verdict::Continue => {
                        report(class, &what, &state, PHASE_CONTINUE, "");
                    }
                    Verdict::Interrupt(reason) => {
                        report(class, &what, &state, PHASE_INTERRUPT, &reason);
                        return Err(WaitAborted {
                            what,
                            reason,
                            elapsed: state.elapsed,
                        });
                    }
                }
            }
        }
    }
}

// ── wait-visibility phases (the `phase` field of [`Event::WaitPolled`]) ──
//
// Plain strings (not an enum) because they travel through serde to the log and
// the UI; the constants keep producer and consumer in lock-step.

/// A heartbeat where the cost gate held — sampled, not asked.
pub const PHASE_SAMPLING: &str = "sampling";
/// A heartbeat handed to the judge (the verdict is still pending).
pub const PHASE_ASKING: &str = "asking";
/// The judge ruled "keep waiting".
pub const PHASE_CONTINUE: &str = "continue";
/// The judge ruled "interrupt".
pub const PHASE_INTERRUPT: &str = "interrupt";
/// The waited-on work finished (or was dropped) — the wait is over.
pub const PHASE_DONE: &str = "done";

/// Push one [`Event::WaitPolled`] heartbeat, if a bus is armed for this context.
///
/// No bus (unit tests, library users) → nothing is emitted, exactly like the
/// judge: the wait stays invisible where there is nobody to see it.
///
/// Public because two of the nine sites (② / ⑨, in `stage_agent`) rule inline
/// rather than through [`polled`] — they call this directly so their heartbeats
/// look identical to the other seven in the UI.
pub fn report(class: WaitClass, what: &str, state: &WaitState, phase: &str, detail: &str) {
    let Some(bus) = current_wait_bus() else {
        return;
    };
    bus.push(Event::WaitPolled {
        class: class.as_str().to_string(),
        what: what.to_string(),
        elapsed_ms: state.elapsed.as_millis() as u64,
        silent_ms: state.silent.as_millis() as u64,
        phase: phase.to_string(),
        detail: detail.to_string(),
    });
}

// The wait judge in force for the **current execution context**.
//
// The engine arms this around `Engine::run_turn` with a model-backed judge
// (`lingmiao_llm::ModelJudge`); the tools that live in a lower crate read it
// from here instead of having a judge threaded through every constructor.
//
// `tokio::task_local!` (not a process-wide static) is deliberate: the judge is
// scoped to the turn's task tree, so a unit test that drives a tool directly
// sees **no judge at all** and therefore keeps the pre-polling behaviour —
// tests can never be silently re-routed through a model.
tokio::task_local! {
    static WAIT_JUDGE: Option<Arc<dyn WaitJudge>>;
    /// The bus the poll reports its heartbeats on (cli 2026-10-05「要进界面的」).
    /// Scoped with the judge for the same reason: a test that drives a tool
    /// directly publishes nothing.
    static WAIT_BUS: Option<Arc<crate::events::EventBus>>;
}

/// Run `fut` with `judge` armed for every wait inside it.
pub async fn with_judge<T, F>(judge: Option<Arc<dyn WaitJudge>>, fut: F) -> T
where
    F: Future<Output = T>,
{
    WAIT_JUDGE.scope(judge, fut).await
}

/// The judge armed for this execution context, if any.
pub fn current_judge() -> Option<Arc<dyn WaitJudge>> {
    WAIT_JUDGE.try_with(|j| j.clone()).ok().flatten()
}

/// Run `fut` with `bus` receiving every wait heartbeat inside it.
///
/// Kept separate from [`with_judge`] so a caller can make a wait *visible*
/// without changing who rules on it (`with_wait_scope` arms both).
pub async fn with_wait_bus<T, F>(bus: Option<Arc<crate::events::EventBus>>, fut: F) -> T
where
    F: Future<Output = T>,
{
    WAIT_BUS.scope(bus, fut).await
}

/// The bus wait heartbeats go to for this execution context, if any.
pub fn current_wait_bus() -> Option<Arc<crate::events::EventBus>> {
    WAIT_BUS.try_with(|b| b.clone()).ok().flatten()
}

/// Arm **both** the judge and the reporting bus for one turn — what the engine
/// actually calls (one scope, so a turn can never have a judge without its waits
/// being reportable, or vice versa).
pub async fn with_wait_scope<T, F>(
    judge: Option<Arc<dyn WaitJudge>>,
    bus: Option<Arc<crate::events::EventBus>>,
    fut: F,
) -> T
where
    F: Future<Output = T>,
{
    WAIT_JUDGE.scope(judge, WAIT_BUS.scope(bus, fut)).await
}

/// Effective policy: the 5s/30s defaults, overridable by env for field tuning.
///
/// * `LINGMIAO_POLL_INTERVAL_SECS` — heartbeat period.
/// * `LINGMIAO_POLL_ASK_AFTER_SECS` — silence floor before the judge is asked
///   (a wait site's own legacy limit always wins when it is larger).
///
/// Read once per call (cheap; waits are not hot loops).
pub fn global_policy() -> PollPolicy {
    // Names come from `brand::env` so a product rename carries them along (the
    // same reason no other module spells the prefix out).
    let secs = |suffix: &str, d: u64| {
        std::env::var(crate::brand::env(suffix))
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(d)
    };
    PollPolicy {
        interval: Duration::from_secs(secs("POLL_INTERVAL_SECS", 5)),
        ask_after: Duration::from_secs(secs("POLL_ASK_AFTER_SECS", 30)),
    }
}

/// [`polled`] with the context's judge and policy — the one-liner the wait sites
/// call.
pub async fn poll_global<T, F>(
    class: WaitClass,
    what: impl Into<String>,
    progress: Arc<Progress>,
    fut: F,
) -> Result<T, WaitAborted>
where
    F: Future<Output = T>,
{
    polled(global_policy(), class, what, progress, current_judge(), fut).await
}

/// Outcome of a [`guard`] wait.
#[derive(Debug)]
pub enum Guarded<T> {
    /// The waited-on future finished.
    Done(T),
    /// The legacy limit expired **with no judge armed** — the pre-polling
    /// behaviour, preserved exactly.
    TimedOut,
    /// The judge ruled the wait stalled and the future was dropped.
    Aborted(WaitAborted),
}

/// Wait for `fut` at a wait site, with an explicit judge, threshold and class.
///
/// The primitive the 9 sites build on. `ask_after` is how long the wait may stay
/// *silent* (no progress) before the judge is consulted; while progress keeps
/// arriving the judge is never asked, however long the wait runs — that is what
/// keeps a 3-minute `cargo build` from being mistaken for a hang.
pub async fn guard_at<T, F>(
    judge: Option<Arc<dyn WaitJudge>>,
    class: WaitClass,
    what: impl Into<String>,
    ask_after: Duration,
    progress: Arc<Progress>,
    fut: F,
) -> Guarded<T>
where
    F: Future<Output = T>,
{
    guard_capped(judge, class, what, ask_after, hard_cap(), progress, fut).await
}

/// [`guard_at`] with an **explicit** absolute ceiling.
///
/// `hard_cap = None` means "the judge has the last word" (cli 口径：由模型决策，
/// 不设人为上限). The engine passes a stage's remaining budget here, so the
/// pre-existing per-stage bound survives while every *silent* stretch inside it
/// is still put to the judge.
pub async fn guard_capped<T, F>(
    judge: Option<Arc<dyn WaitJudge>>,
    class: WaitClass,
    what: impl Into<String>,
    ask_after: Duration,
    hard_cap: Option<Duration>,
    progress: Arc<Progress>,
    fut: F,
) -> Guarded<T>
where
    F: Future<Output = T>,
{
    let what = what.into();
    let Some(judge) = judge else {
        // No judge in force → the caller's own limit is the whole wait. Callers
        // that need the legacy *timeout* semantics use [`guard`] instead.
        return match hard_cap {
            Some(cap) => match tokio::time::timeout(cap, fut).await {
                Ok(v) => Guarded::Done(v),
                Err(_) => Guarded::TimedOut,
            },
            None => Guarded::Done(fut.await),
        };
    };
    let mut policy = global_policy();
    policy.ask_after = ask_after.max(Duration::from_secs(1));
    let waited = polled(policy, class, what, progress, Some(judge), fut);
    match hard_cap {
        Some(cap) => match tokio::time::timeout(cap, waited).await {
            Ok(Ok(v)) => Guarded::Done(v),
            Ok(Err(a)) => Guarded::Aborted(a),
            Err(_) => Guarded::TimedOut,
        },
        None => match waited.await {
            Ok(v) => Guarded::Done(v),
            Err(a) => Guarded::Aborted(a),
        },
    }
}

/// Wait for `fut` at a wait site, keeping the old timeout semantics when no
/// judge is armed.
///
/// `legacy` is the site's existing limit. What it means depends on whether a
/// judge is in force for this execution context:
///
/// * **No judge** (unit tests, library users, `Engine::new`) →
///   `tokio::time::timeout(legacy, fut)`: **byte-for-byte the pre-polling
///   contract.** Old behaviour is never silently changed.
/// * **Judge armed** (inside [`with_judge`]) → the **silence gate** is
///   `min(legacy, 30s)`: once nothing at all has happened for that long, the
///   state is handed to the judge, which decides whether to keep waiting. A wait
///   that keeps producing progress is never questioned, however long it
///   legitimately takes —— 这正是 cli 要的效果（「太多 timeout 等待时间过长」），
///   长命令不再被硬掐。
///
/// An absolute ceiling can be set with `LINGMIAO_POLL_HARD_CAP_SECS`
/// (0/unset = none). Stages pass their own remaining budget explicitly through
/// [`guard_capped`], so a per-stage bound still holds inside a turn.
pub async fn guard<T, F>(
    class: WaitClass,
    what: impl Into<String>,
    legacy: Duration,
    progress: Arc<Progress>,
    fut: F,
) -> Guarded<T>
where
    F: Future<Output = T>,
{
    let what = what.into();
    match current_judge() {
        // Judged: the silence gate is the site's own limit or the global
        // cadence, whichever is sooner; the judge then has the last word (only
        // `LINGMIAO_POLL_HARD_CAP_SECS`, if set, bounds it).
        Some(judge) => {
            let gate = legacy
                .min(global_policy().ask_after)
                .max(Duration::from_secs(1));
            guard_capped(Some(judge), class, what, gate, hard_cap(), progress, fut).await
        }
        // Unjudged: exactly the pre-polling contract.
        None => match tokio::time::timeout(legacy, fut).await {
            Ok(v) => Guarded::Done(v),
            Err(_) => Guarded::TimedOut,
        },
    }
}

/// [`guard`] with an **explicit** judge (same semantics as [`guard`]: `legacy` is
/// the legacy timeout without a judge, and the silence gate with one).
pub async fn guard_with<T, F>(
    judge: Option<Arc<dyn WaitJudge>>,
    class: WaitClass,
    what: impl Into<String>,
    legacy: Duration,
    progress: Arc<Progress>,
    fut: F,
) -> Guarded<T>
where
    F: Future<Output = T>,
{
    let what = what.into();
    match judge {
        Some(judge) => {
            guard_capped(Some(judge), class, what, legacy, hard_cap(), progress, fut).await
        }
        None => match tokio::time::timeout(legacy, fut).await {
            Ok(v) => Guarded::Done(v),
            Err(_) => Guarded::TimedOut,
        },
    }
}

/// A **code**-judged wait: interrupt once the silence exceeds `after`.
///
/// Used where a model judge is physically impossible — ① the wait is *for the
/// model itself*（cli 2026-10-05 拍板接受的例外）, and ⑧ sqlite's busy handler is
/// a synchronous callback that cannot `await`. Behaviourally identical to
/// [`guard_with`] with a [`CodeStall`], named for readability at the call site.
pub async fn guard_code<T, F>(
    class: WaitClass,
    what: impl Into<String>,
    after: Duration,
    progress: Arc<Progress>,
    fut: F,
) -> Guarded<T>
where
    F: Future<Output = T>,
{
    // 代码裁判在 `after` 处必然判中断；`LINGMIAO_POLL_HARD_CAP_SECS`（若设）
    // 只可能更早结束。
    guard_capped(
        Some(CodeStall::handle(after)),
        class,
        what,
        after,
        hard_cap(),
        progress,
        fut,
    )
    .await
}

/// Optional absolute ceiling for judged waits (`LINGMIAO_POLL_HARD_CAP_SECS`).
///
/// Off by default: cli 的口径是「由模型决策」，不设人为上限。The knob exists
/// so an operator can bound an unattended run without touching the code.
pub fn hard_cap() -> Option<Duration> {
    std::env::var(crate::brand::env("POLL_HARD_CAP_SECS"))
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|n| *n > 0)
        .map(Duration::from_secs)
}

// ── the synchronous wait slot (⑧ sqlite busy) ──
//
// ⑧ is the one wait a *synchronous* callback performs (rusqlite's
// `busy_handler` takes a plain `fn(i32) -> bool`, so no async, no captured
// state). It therefore cannot consult a model itself — it can only report. It
// publishes what it is waiting for here, and the surrounding async poll (the
// stage's own wait) folds that line into what the model is told, so a DB lock
// is visible to the decision instead of being invisible. The abort itself is
// code-gated by [`CodeStall`] semantics: no progress for `after` → give up and
// surface the busy error honestly.

/// A wait being performed by synchronous code, published for the async poll.
#[derive(Debug, Clone)]
pub struct SyncWait {
    /// What the sync code is waiting for (e.g. `sqlite: 写 observations`).
    pub what: String,
    /// Time since the sync wait began.
    pub elapsed: Duration,
}

static SYNC_WAIT: std::sync::RwLock<Option<(String, Instant)>> = std::sync::RwLock::new(None);

/// Publish the start of a synchronous wait.
pub fn begin_sync_wait(what: impl Into<String>) {
    publish_sync_wait(what, Instant::now());
}

/// Publish a synchronous wait that began at `since`.
///
/// Used by a repeated callback (SQLite's busy handler fires many times for one
/// wait) that must not keep resetting the clock — the wait's start time has to
/// stay the first call's.
pub fn publish_sync_wait(what: impl Into<String>, since: Instant) {
    if let Ok(mut g) = SYNC_WAIT.write() {
        *g = Some((what.into(), since));
    }
}

/// Clear the synchronous wait slot.
pub fn end_sync_wait() {
    if let Ok(mut g) = SYNC_WAIT.write() {
        *g = None;
    }
}

/// The synchronous wait currently in progress, if any (class [`WaitClass::Store`]).
///
/// A published wait **expires on its own**: sync callers cannot always announce
/// that they finished (SQLite's handler simply stops being called once the lock
/// is granted), so a stale entry older than [`SYNC_WAIT_TTL`] is ignored rather
/// than reported as a live lock.
pub fn current_sync_wait() -> Option<SyncWait> {
    SYNC_WAIT
        .read()
        .ok()
        .and_then(|g| {
            g.as_ref().map(|(what, since)| SyncWait {
                what: what.clone(),
                elapsed: since.elapsed(),
            })
        })
        .filter(|w| w.elapsed < SYNC_WAIT_TTL)
}

/// How long a published synchronous wait stays visible without being refreshed.
pub const SYNC_WAIT_TTL: Duration = Duration::from_secs(20);

/// Sample the state once (used by the synchronous call sites that cannot await).
pub fn sample(class: WaitClass, what: impl Into<String>, progress: &Progress) -> WaitState {
    WaitState {
        class,
        what: what.into(),
        elapsed: progress.elapsed(),
        silent: progress.silent_for(),
        sync: current_sync_wait()
            .map(|w| format!("{}（已等 {:.0}s）", w.what, w.elapsed.as_secs_f64())),
    }
}

/// One-line human digest of a wait (for tool output / warnings).
pub fn describe(class: WaitClass, what: &str, progress: &Progress) -> String {
    sample(class, what, progress).summary()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A judge that records every state it saw and always continues.
    struct Spy {
        seen: Mutex<Vec<WaitState>>,
        verdict: Verdict,
    }

    #[async_trait::async_trait]
    impl WaitJudge for Spy {
        async fn judge(&self, state: &WaitState) -> Verdict {
            self.seen.lock().unwrap().push(state.clone());
            self.verdict.clone()
        }
    }

    fn fast() -> PollPolicy {
        PollPolicy {
            interval: Duration::from_millis(20),
            ask_after: Duration::from_millis(40),
        }
    }

    #[test]
    fn progress_reports_silence_until_touched() {
        let p = Progress::new();
        std::thread::sleep(Duration::from_millis(30));
        assert!(p.silent_for() >= Duration::from_millis(25));
        p.touch();
        assert!(p.silent_for() < Duration::from_millis(15));
        assert!(p.elapsed() >= Duration::from_millis(30));
    }

    #[tokio::test]
    async fn no_judge_means_no_polling() {
        let out = polled(
            fast(),
            WaitClass::Command,
            "echo",
            Progress::new(),
            None,
            async { 7 },
        )
        .await
        .expect("no judge → runs straight through");
        assert_eq!(out, 7);
    }

    #[tokio::test]
    async fn the_cost_gate_holds_until_silence_exceeds_ask_after() {
        let spy = Arc::new(Spy {
            seen: Mutex::new(Vec::new()),
            verdict: Verdict::Continue,
        });
        let p = Progress::new();
        // A wait that keeps making progress for 3 periods is never questioned.
        let keep_touching = {
            let p = p.clone();
            async move {
                for _ in 0..3 {
                    tokio::time::sleep(Duration::from_millis(15)).await;
                    p.touch();
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
                "done"
            }
        };
        let out = polled(
            fast(),
            WaitClass::Model,
            "slow stream",
            p,
            Some(spy.clone() as Arc<dyn WaitJudge>),
            keep_touching,
        )
        .await
        .expect("a busy wait is never interrupted");
        assert_eq!(out, "done");
        assert!(
            spy.seen.lock().unwrap().is_empty(),
            "the judge must not be consulted while progress keeps arriving"
        );
    }

    #[tokio::test]
    async fn a_silent_wait_is_judged_and_interrupted() {
        let spy = Arc::new(Spy {
            seen: Mutex::new(Vec::new()),
            verdict: Verdict::Interrupt("卡住了".into()),
        });
        let err = polled(
            fast(),
            WaitClass::Network,
            "mcp call_tool",
            Progress::new(),
            Some(spy.clone() as Arc<dyn WaitJudge>),
            async {
                tokio::time::sleep(Duration::from_secs(30)).await;
                "never"
            },
        )
        .await
        .expect_err("a silent wait is cut short");
        assert!(err.message().contains("卡住了"), "{}", err.message());
        assert!(err.message().contains("mcp call_tool"));
        let seen = spy.seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "one ruling, then the future is dropped");
        assert_eq!(seen[0].class, WaitClass::Network);
        assert!(seen[0].silent >= Duration::from_millis(35));
    }

    #[tokio::test]
    async fn interrupt_actually_drops_the_future() {
        // The waited future must not keep running after an interrupt.
        let side = Arc::new(AtomicU64::new(0));
        let flag = side.clone();
        let spy: Arc<dyn WaitJudge> = Arc::new(Spy {
            seen: Mutex::new(Vec::new()),
            verdict: Verdict::Interrupt("stop".into()),
        });
        let _ = polled(
            fast(),
            WaitClass::Command,
            "bash",
            Progress::new(),
            Some(spy),
            async move {
                let deadline = Instant::now() + Duration::from_secs(30);
                while Instant::now() < deadline {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    flag.fetch_add(1, Ordering::Relaxed);
                }
            },
        )
        .await;
        let after = side.load(Ordering::Relaxed);
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert_eq!(
            side.load(Ordering::Relaxed),
            after,
            "the future kept ticking after it was interrupted"
        );
    }

    #[tokio::test]
    async fn code_stall_fires_only_past_its_threshold() {
        let judge = CodeStall::new(Duration::from_millis(50));
        let fresh = WaitState {
            class: WaitClass::Model,
            what: "llm stream".into(),
            elapsed: Duration::from_millis(10),
            silent: Duration::from_millis(10),
            sync: None,
        };
        assert!(matches!(judge.judge(&fresh).await, Verdict::Continue));
        let stale = WaitState {
            silent: Duration::from_millis(60),
            ..fresh
        };
        match judge.judge(&stale).await {
            Verdict::Interrupt(r) => assert!(r.contains("停摆"), "{r}"),
            Verdict::Continue => panic!("a stalled wait must be interrupted"),
        }
    }

    #[tokio::test]
    async fn guard_without_a_judge_keeps_the_legacy_timeout_behaviour() {
        // Byte-for-byte the old contract: with nothing armed, `guard` IS
        // `tokio::time::timeout`. A site's own limit is neither stretched nor
        // shortened.
        let ok = guard(
            WaitClass::Command,
            "quick",
            Duration::from_secs(1),
            Progress::new(),
            async { 5 },
        )
        .await;
        assert!(matches!(ok, Guarded::Done(5)));

        let timed_out = guard(
            WaitClass::Command,
            "slow",
            Duration::from_millis(30),
            Progress::new(),
            async {
                tokio::time::sleep(Duration::from_secs(30)).await;
                7
            },
        )
        .await;
        assert!(matches!(timed_out, Guarded::TimedOut));
    }

    #[tokio::test]
    async fn guard_with_an_explicit_judge_aborts_a_stalled_wait() {
        // Used by ①, where the model cannot judge its own stream: the site hands
        // `guard_with` a `CodeStall`, so a long silence ends the wait by code.
        let judge = CodeStall::handle(Duration::from_millis(10));
        let out = guard_with(
            Some(judge),
            WaitClass::Model,
            "llm first token",
            Duration::from_millis(10),
            Progress::new(),
            async {
                tokio::time::sleep(Duration::from_secs(30)).await;
                "never"
            },
        )
        .await;
        match out {
            Guarded::Aborted(a) => {
                assert!(a.message().contains("llm first token"), "{}", a.message());
                assert!(a.message().contains("停摆"), "{}", a.message());
            }
            other => panic!("expected an abort, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn the_judge_is_scoped_to_the_task_tree() {
        // Unit tests / direct tool calls must see no judge (pre-polling
        // behaviour); only code inside `with_judge` is polled.
        assert!(current_judge().is_none(), "no judge outside the scope");
        let judge: Arc<dyn WaitJudge> = CodeStall::handle(Duration::from_millis(10));
        let inside = with_judge(Some(judge), async { current_judge().is_some() }).await;
        assert!(inside, "a judge is visible inside the scope");
        assert!(current_judge().is_none(), "and gone again afterwards");
    }

    #[test]
    fn wait_classes_have_stable_labels() {
        assert_eq!(WaitClass::Model.as_str(), "模型");
        assert_eq!(WaitClass::Command.as_str(), "外部命令");
        assert_eq!(WaitClass::Network.as_str(), "网络服务");
        assert_eq!(WaitClass::Store.as_str(), "数据库/阶段");
    }
}

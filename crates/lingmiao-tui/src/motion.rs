//! Time-driven chrome: the spinner-verb wheel, the idle tip rotation, and the
//! wall clock. Everything here is a *pure* function of an elapsed-millisecond
//! value plus, for the clock, `chrono` — so the motion is unit-testable
//! independently of the real wall clock, and `app.rs` only has to call it
//! (§14-P2 活动动词轮换 / §14-P3 tips 轮播).
//!
//! (Extracted from `app.rs` so the render layer stays about *drawing* and the
//! three pure motion helpers live together — see UX §14.)

use std::time::Duration;

/// The CC spinner-verb wheel (§14-P2): the gerunds CC rotates in its activity /
/// tail line (`◜ Pondering…` / `◜ Cooked for 4s · done 18:39`), captured from CC
/// 2.1.278 in page24. The activity line steps through one verb every 800ms so a
/// long turn visibly *moves* instead of staring at a frozen stage name, and the
/// turn tail reports the verb it finished on.
///
/// `Accompishing` was a transcription slip for **`Accomplishing`** — the words
/// are user-visible chrome, so they are checked against the CC binary
/// (self-learned constraint 「抄录外部用户可见字符串须逐词拼写核对」).
pub const VERBS: [&str; 44] = [
    "Accomplishing",
    "Actioning",
    "Baking",
    "Brewing",
    "Calculating",
    "Churning",
    "Clamping",
    "Cogitating",
    "Computing",
    "Concocting",
    "Considering",
    "Cooking",
    "Crafting",
    "Creating",
    "Crunching",
    "Deliberating",
    "Determining",
    "Doing",
    "Effecting",
    "Finagling",
    "Forging",
    "Formulating",
    "Generating",
    "Hatching",
    "Herding",
    "Honing",
    "Hustling",
    "Ideating",
    "Inferring",
    "Milling",
    "Minting",
    "Mulling",
    "Musing",
    "Noodling",
    "Perusing",
    "Pondering",
    "Processing",
    "Puttering",
    "Reticulating",
    "Ruminating",
    "Simmering",
    "Synthesizing",
    "Transmuting",
    "Wrangling",
];

/// How long one spinner verb stays on screen before the wheel steps on.
const VERB_STEP_MS: u64 = 800;

/// Pick the spinner verb for a given elapsed time — one step every 800ms, then
/// wrap around the wheel (§14-P2). Pure, so the rotation is unit-testable
/// independently of the (real) wall clock.
pub fn activity_verb(elapsed_ms: u64) -> &'static str {
    VERBS[(elapsed_ms / VERB_STEP_MS) as usize % VERBS.len()]
}

/// Wall-clock `HH:MM:SS` in the **local** timezone (never UTC) for the turn
/// tail's `done` stamp (§14-P2).
pub fn now_hms() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

/// Idle-state tips (§14-P3), rotated into the empty input box. CC rotates its own
/// tips this way (captured from CC 2.1.278: e.g. `Use /clear to start fresh when
/// switching topics and free up context`, `Hit Enter to queue up additional
/// messages while Claude is working.`, `Use /memory to view and manage Claude
/// memory`); these are the 灵妙 equivalents, every one naming a command or key
/// this TUI actually has.
pub const TIPS: [&str; 10] = [
    "输入 /help 查看全部命令",
    "PageUp / PageDown 上翻下翻对话历史",
    "Ctrl+R 搜索已提交的历史输入",
    "小白板可用 /board 查看全文",
    "Enter 发送 · Shift+Enter 或 Ctrl+J 换行",
    "↑ / ↓ 浏览已提交的历史输入",
    "Esc 取消进行中的回合",
    "/model 查看当前模型与配置来源",
    "/session 查看本次会话的 token 与阶段耗时",
    "/memory 查看记忆库统计",
];

/// How long one tip stays before rotating to the next (CC uses a cooldown of
/// this order — page24 records one).
pub const TIP_COOLDOWN: Duration = Duration::from_secs(8);

/// Which tip to show for a given elapsed time (§14-P3): one step every
/// [`TIP_COOLDOWN`], wrapping around [`TIPS`]. Pure, so the rotation and its
/// wrap-around are unit-testable.
pub fn tip_index(elapsed_ms: u64, n: usize) -> usize {
    if n == 0 {
        return 0;
    }
    (elapsed_ms / TIP_COOLDOWN.as_millis() as u64) as usize % n
}

/// The tip text for a given elapsed time (a thin wrapper over [`tip_index`]).
pub fn tip_at(elapsed_ms: u64) -> &'static str {
    TIPS[tip_index(elapsed_ms, TIPS.len())]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activity_verbs_rotate_with_elapsed() {
        // §14-P2: the wheel steps every 800ms, so a long turn visibly moves
        // instead of staring at a frozen stage name.
        assert_eq!(activity_verb(0), VERBS[0]);
        assert_ne!(activity_verb(0), activity_verb(800));
        assert_ne!(activity_verb(800), activity_verb(1600));
        // … and wraps back to the start after a full revolution.
        assert_eq!(activity_verb(800 * VERBS.len() as u64), VERBS[0]);
        // Every wheel entry is a real gerund (no gaps in the array).
        assert_eq!(VERBS.len(), 44, "the CC wheel has 44 verbs");
        assert!(VERBS.iter().all(|v| !v.is_empty()));
        // The transcription slip is gone: the wheel opens on the real word.
        assert_eq!(VERBS[0], "Accomplishing");
    }

    #[test]
    fn done_timestamp_is_local_wall_clock() {
        // §14-P2: `done HH:MM:SS` uses the *local* clock, never UTC — the shape
        // is `NN:NN:NN`.
        let hms = now_hms();
        let parts: Vec<&str> = hms.split(':').collect();
        assert_eq!(parts.len(), 3, "HH:MM:SS shape: {hms:?}");
        assert!(parts.iter().all(|p| p.len() == 2), "zero-padded: {hms:?}");
    }

    #[test]
    fn tips_rotate_with_cooldown() {
        // §14-P3: one tip per cooldown, then the next; the wheel wraps.
        let n = TIPS.len();
        assert_eq!(tip_index(0, n), 0);
        assert_ne!(tip_index(0, n), tip_index(8_000, n));
        assert_ne!(tip_index(8_000, n), tip_index(16_000, n));
        assert_eq!(tip_index(8_000 * n as u64, n), 0, "wraps around");
        // Degenerate input never panics.
        assert_eq!(tip_index(123, 0), 0);
        // Every tip is a real, non-empty string.
        assert!(TIPS.iter().all(|t| !t.is_empty()));
        // The opening tip advertises /help — the first thing a new user needs.
        assert!(tip_at(0).contains("/help"), "{}", tip_at(0));
    }
}

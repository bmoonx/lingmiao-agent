//! TUI application state + rendering — 需求① · §12 纯 CC 单栏.
//!
//! Design (docs/ux-design.md §12, 2026-09-20 拍板 ✅):
//! * **Single-column CC layout**: **no sidebar**. Vertical bands = the CC-style
//!   conversation main area, a multiline input box, and a 3-line **footer**
//!   (§12.3). There is **no** permanent header (cli 2026-09-24): the brand
//!   identity (品牌 · 版本 · 模型 · cwd) is printed once into the conversation at
//!   startup ([`App::push_startup_banner`], mirroring CC's launch block) and kept
//!   at the footer's **bottom-right**. The full-screen
//!   alternate buffer is kept and the conversation owns its own **paging**
//!   (`PageUp`/`PageDown`/`Home`/`End`) — see §11.
//! * **Sidebar six blocks → CC homes** (§12.2): status → footer L1;
//!   the 3-stage pipeline → inline `◜ {动词}… Ns · esc` + the CC-style turn
//!   tail `◜ {动词} for Ns · done HH:MM:SS` (§14-P2); session meters → footer L1;
//!   function navigation → **slash commands** (§12.4); the context bar → footer
//!   L2 (real tokens + composition, **no % / no window number**); the whiteboard
//!   content → a single grey L3 hint (cli 2026-09-21; `/board` for the full text).
//! * **Main area**: CC-style conversation — a `❯` prompt line for the user,
//!   `∴` thinking blocks (a grey header + the last few live lines by default;
//!   `ctrl+o` expands to the full reasoning — §12.7), `●` replies, `● name(args)` + `⎿ result`
//!   tool cards (with a `⠋ … · Ns` running state), and a CC-style
//!   `◜ {动词} for Ns · done HH:MM:SS` tail — plus a multi-line input box,
//!   in-app paging, and a resident shortcut row. The transcript is CC-shaped:
//!   **no** per-item pipeline badges and **no** tool-count chatter (cli
//!   2026-09-24: 「先按 CC 的样式做」) — assistant replies render as markdown
//!   (headings, lists, code) rather than raw source.
//! * **Theme**: terminal-native and **token-driven** — every colour comes from
//!   [`crate::theme`] (a layered palette: surface/border/rule/text/text_muted
//!   plus semantic success/warn/error, the running-activity signal, and the
//!   brand accent). No hard-coded
//!   background, colour is only a *signal* (state, thresholds), and widget code
//!   never writes a raw `Color::` literal (§14-P0).
//!
//! The state machine is pure and testable: [`App::on_key`] returns an
//! [`Action`] the run loop acts on, [`App::on_event`] folds bus events in, and
//! [`App::render`] draws the single-column frame. `App` never touches the
//! engine, the terminal, or the clock (it may read the wall clock for display
//! only).

use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use lingmiao_core::LingmiaoError;
use lingmiao_core::brand;
use lingmiao_core::events::Event;
use lingmiao_core::polling::{
    PHASE_ASKING, PHASE_CONTINUE, PHASE_DONE, PHASE_INTERRUPT, PHASE_SAMPLING,
};
use lingmiao_engine::{STAGE_B, STAGE_C, STAGE_SUMMARY, SummaryReport};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};

use crate::editor::Editor;
use crate::markdown::markdown_lines;
use crate::motion::{activity_verb, now_hms, tip_at};
use crate::session_input::SessionInput;
use crate::theme::{
    ACCENT, ACTIVITY_SPINNER, BORDER, DIFF_ADD, DIFF_CONTEXT, DIFF_REMOVE, ERROR,
    HANDOFF_FORMAT_MARK, HANDOFF_MARK, LOGO, RUNNING, SELECTION_BG, SELECTION_FG, SPINNER,
    STAGE_TAG, SUCCESS, TAIL_MARK, TEXT, TEXT_MUTED, THINKING_KW, TOOL_RESULT, WAIT_MARK, WARN,
};
use crate::wrap::{CLOSING, OPENING, wrap_cjk, wrap_cjk_keep};

// The spinner-verb wheel (§14-P2), the idle tip rotation (§14-P3) and the wall
// clock live in [`crate::motion`] — pure, unit-tested there. This module only
// calls them.

/// Slash-command palette (§12.2 ④): command name + Chinese description shown in
/// the completion menu. Function navigation moved off the sidebar onto
/// `/`-commands when the sidebar was撤 (§12.4).
const COMMANDS: [(&str, &str); 10] = [
    ("help", "显示帮助"),
    ("board", "查看小白板全文"),
    ("memory", "记忆库统计"),
    ("session", "本次会话信息"),
    ("tools", "可用工具组"),
    ("model", "当前模型"),
    ("auto", "自动模式（Main+Auditor 循环）"),
    ("round", "单轮审计（Main→Auditor）"),
    ("clear", "清空对话面板"),
    ("quit", "退出"),
];

/// What the run loop should do after a key press.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Nothing to do (text consumed by the editor or a self-contained command).
    None,
    /// Send this text through the engine pipeline.
    Submit(String),
    /// Abort the in-flight turn (Esc while streaming).
    Cancel,
    /// Cancel the in-flight turn and immediately send the front queued message
    /// (CC's `ctrl+x ctrl+s` "send now").
    SendQueuedNow,
    /// Forget conversation history — the run loop wipes the conversation panel.
    ///
    /// 原版对齐（cli 2026-10-05「按原版设计来」）: the command clears the **rendered
    /// panel only** — the engine is stateless now, and the #1 archive is never
    /// touched (原版 `/clear` = 「清空对话面板（历史仍在引擎归档中）」). The next
    /// 组织上下文 turn rebuilds continuity from the archive as always, so the
    /// panel clears without the model losing the thread.
    Clear,
    /// Put this text on the clipboard — the explicit copy shortcut (`ctrl+y` /
    /// `shift+insert`) applied to the live drag selection (cli 2026-09-27: the
    /// drag itself must **not** copy, the user presses a key).
    Copy(String),
    /// Read the OS clipboard and insert it into the input box — the explicit
    /// `ctrl+v` (cli 2026-09-28「默认支持 win 和 linux 的复制粘贴」). Carries no
    /// payload: reading the clipboard is I/O, so the **run loop** does it and
    /// hands the text back to [`App::paste`] — `App` itself stays pure.
    Paste,
    /// The loop queries the memory stores and reports stats (slash `/memory`).
    Stats,
    /// Start an autonomous session (`/auto <目标>`): the run loop builds the
    /// engine's autonomous loop and drives it on a background task, while the
    /// session's notices stream back over the same event bus.
    Auto(String),
    /// Run one Main→Auditor audit cycle (`/round <目标>`), synchronously.
    Round(String),
    /// Leave the app (a **second** `Ctrl+C`, `/quit`, `/exit`).
    Quit,
}

/// Whether a turn is currently in flight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
    Streaming,
}

/// Which bucket one line of a file mutation's unified diff belongs to — the
/// display side of `lingmiao_tools::diff::DiffKind`, parsed from the `kind` string in
/// [`Event::ToolCalled`]'s `diff` payload (`add` / `remove` / `context` / `hunk`).
///
/// cli 2026-09-28 (「参照 CC 实现代码改动时的红绿对比格式显示样式，包括写入的时候
/// 也是」): CC's `Edit` / `Write` cards render the patch with added lines green
/// and removed lines red; the TUI mirrors that (see [`push_tool_card`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiffKind {
    /// A `+` line (present only in the new text).
    Add,
    /// A `-` line (present only in the old text).
    Remove,
    /// An unchanged line carried along for readability.
    Context,
    /// The `@@ -a,b +c,d @@` hunk header.
    Hunk,
}

/// One display line of a file-mutation diff (the TUI-side mirror of
/// `lingmiao_tools::diff::DiffLine` — the app deliberately does not depend on the
/// tool crate, so the payload is parsed from the event).
#[derive(Debug, Clone, PartialEq, Eq)]
struct DiffLine {
    kind: DiffKind,
    text: String,
}

impl DiffLine {
    /// The one-column marker CC/git print in front of the line text.
    fn marker(&self) -> &'static str {
        match self.kind {
            DiffKind::Add => "+",
            DiffKind::Remove => "-",
            // A context line keeps a leading space (git's convention); the hunk
            // header carries its own `@@`, so it gets no marker.
            DiffKind::Context => " ",
            DiffKind::Hunk => "",
        }
    }

    /// The line's colour: green for additions, red for removals, muted chrome for
    /// context — exactly CC's `diffAdded` / `diffRemoved` / subtle mapping.
    fn color(&self) -> Color {
        match self.kind {
            DiffKind::Add => DIFF_ADD,
            DiffKind::Remove => DIFF_REMOVE,
            DiffKind::Context | DiffKind::Hunk => DIFF_CONTEXT,
        }
    }
}

/// Parse [`Event::ToolCalled`]'s `diff` payload (`[{"kind","text"}, …]`) into
/// display lines. Unknown kinds are dropped (forward-compatible), and an absent /
/// non-array payload yields an empty diff — every non-file tool.
fn parse_diff(diff: &serde_json::Value) -> Vec<DiffLine> {
    let Some(rows) = diff.as_array() else {
        return Vec::new();
    };
    rows.iter()
        .filter_map(|r| {
            let kind = match r.get("kind")?.as_str()? {
                "add" => DiffKind::Add,
                "remove" => DiffKind::Remove,
                "context" => DiffKind::Context,
                "hunk" => DiffKind::Hunk,
                _ => return None,
            };
            Some(DiffLine {
                kind,
                text: r.get("text")?.as_str()?.to_string(),
            })
        })
        .collect()
}

/// The outcome half of a finished tool call — duration, failure flag, and the
/// (already parsed) display diff. Grouped so [`App::push_tool_card`] keeps a
/// readable argument list (`clippy::too_many_arguments`).
struct ToolOutcome {
    /// Tool wall-clock duration.
    ms: f64,
    /// Whether the call failed (the card's dot / body turn red).
    error: bool,
    /// The red/green patch of a file mutation; empty for every other tool.
    diff: Vec<DiffLine>,
}

/// The `+N -M` stat CC prints on a file-mutation header, or an empty string when
/// the diff is empty (nothing changed).
fn diff_stat(lines: &[DiffLine]) -> String {
    let added = lines.iter().filter(|l| l.kind == DiffKind::Add).count();
    let removed = lines.iter().filter(|l| l.kind == DiffKind::Remove).count();
    match (added, removed) {
        (0, 0) => String::new(),
        (a, 0) => format!("+{a}"),
        (0, r) => format!("-{r}"),
        (a, r) => format!("+{a} -{r}"),
    }
}

/// Status of one pipeline stage within the current turn (§7 本轮处理).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum StageStatus {
    /// Not started yet — grey dot.
    #[default]
    Pending,
    /// In flight — spinner.
    Running,
    /// Finished OK — green tick.
    Done,
    /// Errored — red cross.
    Fault,
}

/// Per-stage progress: status + real elapsed time (from the stage event).
#[derive(Debug, Clone, Copy, Default)]
struct StageProgress {
    status: StageStatus,
    elapsed_ms: f64,
}

/// A tool call that is currently executing (CC/lingmiao running card: spinner +
/// args + live elapsed, before the result exists).
#[derive(Debug, Clone)]
struct RunningTool {
    /// The pipeline stage that issued the call — drives the stage tag (cli
    /// 2026-09-24 ⑦).
    stage: String,
    tool: String,
    args: serde_json::Value,
    started: Instant,
}

/// The **live wait heartbeat** of the F 项 poll — a mirror of the latest
/// [`Event::WaitPolled`], borrowed from the event (cli 2026-10-05「要进界面的，
/// 这是核心体验」).
///
/// Why it exists: the 5s poll (F 项) was *running* and deciding correctly, but
/// everything it learnt lived in `.cache/lingmiao/logs/` — nothing reached the
/// screen. A `bash` call that printed nothing for 60s therefore still looked
/// exactly like a hang, which is what cli reported («还是等了60多s»). This is the
/// missing display half: one line, refreshed in place every sample, saying *what*
/// is being waited on, *how long* it has run, *how long since anything happened*,
/// and *what the judge said*.
#[derive(Debug, Clone)]
struct WaitNotice {
    /// Waiting-on class (`外部命令` / `模型` / … — the engine's own label).
    class: String,
    /// What exactly is being waited on (`bash: cargo fmt --all --check`).
    what: String,
    /// How long the wait has run, in milliseconds.
    elapsed_ms: u64,
    /// How long since the last sign of progress (the number the judge rules on).
    silent_ms: u64,
    /// Latest phase (`sampling` / `asking` / `continue` / `interrupt`).
    phase: String,
    /// The judge's reason, on `interrupt`.
    detail: String,
    /// How many times the judge was actually consulted (a real «轮询了几次» count —
    /// each consult is one extra model call, so this is also the cost on screen).
    rulings: u32,
}

/// The **A→B handoff** as the transcript needs it — a plain mirror of
/// [`Event::ContextHandoff`], borrowed from the event (cli 2026-09-30).
struct HandoffNotice<'a> {
    /// Producing stage (`组织上下文`).
    from: &'a str,
    /// Receiving stage (`工作阶段`).
    to: &'a str,
    /// Observation records selected.
    observations: u64,
    /// Knowledge-graph nodes selected.
    nodes: u64,
    /// Prior conversation turns pulled in.
    recent_turns: u64,
    /// Characters of the injected block (`0` = nothing was injected).
    chars: u64,
    /// The literal prefix the receiver prepends.
    prefix: &'a str,
    /// Wire role of the injected message.
    role: &'a str,
    /// Where the injected message sits in the receiver's list.
    position: &'a str,
}

/// Fixed three-stage pipeline, in plain language (§7 去黑话).
const PROGRESS_STAGES: [&str; 3] = ["检索中", "作答中", "沉淀中"];

/// 自动模式两个角色的事件 `stage` 名 —— **直接引引擎侧的唯一信源**
/// `lingmiao_engine::autonomous::RoleKind::stage_name()`
/// （`crates/lingmiao-engine/src/autonomous/role.rs`），不在 TUI 重抄一遍字符串。
///
/// cli 2026-10-04（「各控件也没有显示阶段和角色」）：角色的事件 `stage` 以前落进
/// [`stage_tag_span`] 的 `""` 兜底分支 → 思考块 / 工具卡 / 正文**全都没有标签**；
/// 同一名字又落进 [`human_stage`] 的 `other => other`，于是左下角显示成
/// `Auto·Main · Auto·Main`（动词与阶段同一个词，既不是状态也不见角色）。这两个
/// 常量把角色名接进同一套渲染映射，两处一起修好；`const` 取引擎常量，引擎侧改名
/// 会在这里编译期跟着变，不会静默退化成空标签。
const AUTO_MAIN_STAGE: &str = lingmiao_engine::autonomous::RoleKind::Main.stage_name();
/// Auditor 角色（见 [`AUTO_MAIN_STAGE`]）。
const AUTO_AUDITOR_STAGE: &str = lingmiao_engine::autonomous::RoleKind::Auditor.stage_name();

/// 这个 `stage` 是不是自动模式角色（Main / Auditor）。
fn is_auto_stage(stage: &str) -> bool {
    stage == AUTO_MAIN_STAGE || stage == AUTO_AUDITOR_STAGE
}

/// Map a canonical stage name to its slot in [`PROGRESS_STAGES`].
///
/// 自动模式的角色跑的是**「工作阶段式」的工具循环**（`role.rs` 模块头差异②：角色
/// 内层不跑 A→B 装配，只跑工作循环），所以它落进同一格 —— 否则 `/session` 的每
/// 阶段计时对自动模式会永远停在 `检索中 — · 作答中 — · 沉淀中 —`。
fn stage_index(stage: &str) -> Option<usize> {
    match stage {
        "组织上下文" => Some(0),
        "工作阶段" => Some(1),
        "沉淀阶段" => Some(2),
        _ if is_auto_stage(stage) => Some(1),
        _ => None,
    }
}

/// One committed conversation item (§12.7 / §12.10). Almost everything is a
/// ready-made [`Line`]; the **thinking block** and the **tool output** stay
/// structured so `ctrl+o` can re-render them collapsed (a grey summary / a
/// `… 还有 N 行` hint) or expanded (the full reasoning / result) without losing
/// the transcript order.
enum Item {
    Line(Line<'static>),
    /// A **user turn** (§12.11): the reverse-video prompt bar — its own variant
    /// (not a bare [`Item::Line`]) so the transcript keeps the bar's own spans
    /// and the fold/layout code can treat it as a distinct block.
    UserTurn {
        lines: Vec<Line<'static>>,
    },
    Thinking {
        stage: String,
        text: String,
        secs: f64,
    },
    /// A tool card (§12.10 P1-a): the `● name(args)` header is kept whole, the
    /// `⎿ result` body is kept as the *already wrapped* lines so
    /// [`TOOL_OUTPUT_COLLAPSE_LINES`] folding can hide the tail — and `ctrl+o`
    /// reveal it again — without re-breaking the CJK wrapping (§12.9).
    ///
    /// `diff` is the red/green unified diff of a **file mutation** (`edit` /
    /// `write_file`), already wrapped + coloured; empty for every other tool
    /// (cli 2026-09-28「代码改动的红绿对比」).
    ToolOutput {
        header: Line<'static>,
        body: Vec<Line<'static>>,
        diff: Vec<Line<'static>>,
    },
}

/// When a tool result exceeds this many wrapped lines, only the first
/// [`TOOL_OUTPUT_HEAD_LINES`] show by default plus a `∴ … 还有 M 行 · ctrl+o 展开`
/// hint (§12.10).
///
/// cli 2026-09-24 (「先按 CC 的样式做」): CC shows a tool result as a single
/// `⎿ …` summary line, so a big `read_file`/`grep`/search result no longer floods
/// the transcript — the threshold is small and only the head line stays visible.
///
/// cli 2026-09-28 (「控件显示行数改为3」): both this threshold and the visible
/// head ([`TOOL_OUTPUT_HEAD_LINES`]) are **three**, kept in lockstep so a result
/// of exactly three lines never renders a bogus「还有 0 行」hint.
const TOOL_OUTPUT_COLLAPSE_LINES: usize = 3;

/// How many lines of a **folded** tool result stay on screen (§12.10). cli
/// 2026-09-24 (「工具调用返回值 … 3行」): **three** lines, so a short multi-line
/// result reads in place like CC's `⎿` block; `ctrl+o` reveals the rest.
///
/// cli 2026-09: briefly raised to four; cli 2026-09-28 restored **three** —
/// every non-thinking widget folds to three lines, with the threshold
/// ([`TOOL_OUTPUT_COLLAPSE_LINES`]) in lockstep.
const TOOL_OUTPUT_HEAD_LINES: usize = 3;

/// How many trailing lines of a **folded** reasoning block stay on screen
/// (§12.7).
///
/// cli 2026-09-21: a block collapsed to a single summary line read as *frozen* —
/// "看起来等待很焦急". The fold therefore keeps its **last** `N` wrapped lines
/// visible (grey, upright) so the user watches the model think in real time,
/// while `ctrl+o` still reveals the whole reasoning.
///
/// cli 2026-09-24: raised from 3 to **4** — one more live line of reasoning
/// stays on screen before the fold.
///
/// cli 2026-09: briefly raised to six; cli 2026-09-28 (「思考改为8」) settled on
/// **eight** — the reasoning window is the one widget that shows more than the
/// three-line tool fold, because a long think is exactly what makes the wait
/// feel long.
const THINKING_TAIL_LINES: usize = 8;

/// Lines the **mouse wheel** pages per notch (§11). cli 2026-09-21: with no mouse
/// capture the terminal turned the wheel into ↑/↓, which the input box swallowed
/// ("一滚动就是输入栏，出不去了").
pub const WHEEL_LINES: u16 = 3;

/// How long the `已复制 N 字符` confirmation stays on screen (cli 2026-09-27
///「鼠标拖动选中 UI 文字」) — long enough to read, short enough that it goes
/// away on its own.
///
/// It is drawn on the **footer line**, not over the conversation pane: the first
/// version painted it across the pane's bottom row, where it overpainted the
/// very selection the user had just made (cli 2026-09-27「左下角弹出的复制提示
/// 顶掉了选择」).
pub const COPY_NOTE_SECS: f64 = 2.0;

/// How long a **first** `Ctrl+C` stays armed before the app forgets it (cli
/// 2026-09-28「按一下 ctrl+c 我本来想复制直接就退出去了，这个组合键要按两次」).
///
/// CC's own double-press window (`function zO(t,c,o,s=D)` … `D=800`, claude.exe
/// 2.1.270): a second press within 800 ms *fires* the armed action (quit), a
/// later one just re-arms. 灵妙 copies the window so the muscle memory transfers.
///
/// Why the first press is not enough (and why it is not a *cancel* either): the
/// terminal cannot report `Ctrl+C` as "copy" — the app owns the selection (§24),
/// so `Ctrl+C` means "copy what is selected" when something is selected and
/// "quit" when it is not. One stray `Ctrl+C` while the user was *aiming* at a
/// terminal-style copy must not end the session, hence the arm + the on-screen
/// `再按一次 ⌃C 退出` hint (`Esc`/any other key disarms).
pub const EXIT_ARM_SECS: f64 = 0.8;

/// The transient hint shown after a first `Ctrl+C` (footer, left-aligned like the
/// `已复制 N 字符` confirmation).
pub const EXIT_ARM_HINT: &str = "再按一次 ⌃C 退出";

/// The placeholder stored when the whiteboard has nothing to show (missing /
/// empty `pages.json`). [`App::whiteboard_summary`] treats it as "no note" — the
/// ctx row's left slot stays blank rather than printing fake content.
pub const WHITEBOARD_EMPTY: &str = "（小白板暂无内容）";

/// Blank columns kept between the ctx row's **left** (whiteboard note) and
/// **right** (token figure) slots — cli 2026-09-28: 「留一点gap，不要完全顶着上下文
/// 那里」. The left slot is truncated to leave exactly this gutter.
pub const CTX_LINE_GAP: usize = 2;

/// A live drag selection in the conversation pane (cli 2026-09-27「鼠标拖动可以选中
/// UI 上的文字」).
///
/// Coordinates are **(absolute transcript line, display column)** — *not* screen
/// rows — so lines that stream in below during a drag cannot shift what is
/// highlighted. The transcript must implement this itself: the TUI captures the
/// mouse (§11 / §16.3), so the terminal's own drag-select never sees the drag.
///
/// Either mouse button starts a selection (cli 2026-09-27: 「右键拖动也要能选择」),
/// and finishing the drag **keeps** it — the copy itself is an explicit
/// keystroke (`ctrl+y` / `shift+insert`), so a release never overrides what the
/// user can still adjust or re-select.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Selection {
    /// Where the drag began.
    anchor: (u32, u16),
    /// Where the pointer is now.
    head: (u32, u16),
}

/// Maximum height (rows, **including** the border) of the input box. A buffer
/// that wraps to more visual rows than this scrolls to keep the cursor visible
/// (cli 2026-09-27「多行文本框超出看不到」).
const MAX_INPUT_HEIGHT: u16 = 9;

/// All renderable TUI state.
pub struct App {
    model: String,
    group: String,
    version: String,
    /// The input box's line editor — a char-index cursor + undo stack (§②). The
    /// buffer text is `editor.text`; the cursor is `editor.cursor`.
    editor: Editor,
    state: State,
    /// The pipeline stage currently running (empty when idle).
    stage: String,
    /// Live `工作阶段` answer text (streamed over the bus).
    streaming: String,
    /// Live reasoning text of the current turn (`∴` lines, streamed). Committed
    /// to `scrollback` before any answer or tool card lands.
    thinking: String,
    /// The pipeline stage that produced the current `thinking` buffer (badge).
    thinking_stage: String,
    /// Whether the reasoning fold (§12.7) is expanded. `false` is CC-style — a
    /// one-line grey summary; `ctrl+o` toggles it (default collapsed).
    thinking_expanded: bool,
    /// Whether folded **tool output** (§12.10 P1-a) is expanded. `false` folds a
    /// long `⎿ result` to its first [`TOOL_OUTPUT_COLLAPSE_LINES`] lines; the
    /// same `ctrl+o` toggles it alongside the reasoning fold.
    output_expanded: bool,
    /// When the current reasoning segment began — drives the summary's `Ns`.
    thinking_started: Instant,
    /// The pipeline stage that produced the current `streaming` answer — drives
    /// the reply's stage tag (cli 2026-09-24 ⑦).
    stream_stage: String,
    /// The tool call currently executing (`⠋ name(args) · Ns` running card).
    running_tool: Option<RunningTool>,
    /// The **live wait heartbeat** (F 项 轮询可见化, cli 2026-10-05「要进界面的」).
    ///
    /// Latest [`Event::WaitPolled`], or `None` when nothing is being polled. The
    /// poll samples every 5s; each sample overwrites this, and the renderer draws
    /// **one** line under the running card — so the wait visibly ticks instead of
    /// the transcript silently sitting still for a minute. Cleared when the wait
    /// reports `done` or the turn ends.
    wait_notice: Option<WaitNotice>,
    /// 自动模式的**当前轮次**（`0` = 不在自动模式里）。
    ///
    /// cli 2026-10-04（「自动模式需要显示轮次，也就是到第几轮了」）：自动模式是没有
    /// 硬轮数上限的循环，屏幕上只有随对话流滚走的「🤖 Main·第N轮 — 开始」。这一字段
    /// 由 [`Event::AutoNotice`] 的 `iteration` 驱动（引擎在 Main / Auditor 开始处带
    /// 上 `session.current_iteration`），让 TUI 把轮次钉在**持续可见**的位置：footer
    /// 左槽（`作答中 · Auto·Main · 第N轮`）与活动行。`begin_turn` 归零，所以普通回合
    /// 与自动模式结束都不会残留旧轮次。
    auto_round: u64,
    /// Tool calls in the *current* turn. Retained for a possible `· N 个工具`
    /// tail (cli 2026-09-24: 「先按 CC 的样式做」 — CC prints none, so it is
    /// currently unused).
    #[allow(dead_code)]
    turn_tools: u64,
    /// When the current turn started (for the tail line's real elapsed time).
    turn_started: Instant,
    /// When the app started — drives the idle tip rotation (§14-P3): the tip in
    /// the empty input box steps on every `TIP_COOLDOWN`.
    tip_started: Instant,
    error: Option<String>,
    /// Committed conversation items (user / assistant / tool cards / notices /
    /// collapsible thinking blocks §12.7).
    scrollback: Vec<Item>,
    /// Last known frame size.
    width: u16,
    height: u16,
    /// Set when the user asks to quit.
    should_quit: bool,
    // ── session meters (real) ──────────────────────────────────
    tokens_in: u64,
    tokens_out: u64,
    tool_calls: u64,
    elapsed_ms: f64,
    // ── header / footer / history (§12) ────────────────────────
    /// Working directory shown in the header (`~`-shortened).
    cwd: String,
    /// Whiteboard current-page lines (refreshed by the run loop); shown as the
    /// one-line footer L3 summary (§12.3).
    whiteboard: Vec<String>,
    /// Submitted-input history, oldest→newest (§12.4: ↑↓ browse it).
    history: Vec<String>,
    /// Index into [`Self::history`] while browsing with ↑↓, or `None` when
    /// editing the live draft.
    hist_cursor: Option<usize>,
    /// The live draft stashed while browsing history, restored on ↓ past newest.
    draft: String,
    /// Per-stage progress for the current/last turn (§7; also drives the inline
    /// `◜ {阶段} · Ns · esc` activity line and the header state).
    progress: [StageProgress; 3],
    /// Per-turn context composition (§8.5): `(name, chars)` rows, in order.
    ctx_sections: Vec<(String, u64)>,
    /// Sum of `ctx_sections` chars — the token-split denominator.
    ctx_total_chars: u64,
    /// The **current LLM call's** input size — the number at the pane's
    /// bottom-right.
    ///
    /// Provider-measured `usage.input_tokens` of the most recent LLM round-trip
    /// **of any stage** (`Event::LlmResponse`, pushed once per round-trip). cli
    /// 2026-09-28 (「你显示当前 llm 的输入 tokens 就行」): the figure must follow the
    /// call that just happened — 组织上下文's call, each 工作阶段 tool-loop round-trip,
    /// 流程's call — instead of the two stale behaviours it replaced: it used to
    /// update only when a **工作阶段 finished** (so all through 组织上下文 and the
    /// 工作阶段 stream the user read the *previous turn's* number) and it ignored
    /// every non-工作阶段
    /// stage. `input_tokens_last` on `StageResultReported` stays as a fallback for
    /// a stage whose transport never reported per-round usage.
    ctx_tokens: u64,
    // ── main-area paging & context focus (§8.3 / §11) ──────────
    /// Conversation scroll offset in lines *above* the newest line (0 = pinned
    /// to the bottom). `PageUp`/`PageDown`/`Home`/`End` move it. The alternate
    /// buffer has no native scrollback, so the app owns paging (§11).
    scroll: u16,
    /// Upper bound for [`Self::scroll`], recomputed every render.
    max_scroll: u16,
    /// Absolute line index of the viewport top while the user has scrolled up
    /// (`None` = follow the newest line). Pins the viewport so freshly-streamed
    /// lines appended below do not push it down (cli 2026-09-27「滚轮翻动后位置
    /// 保持」).
    anchor: Option<u16>,
    /// Scroll offset (visual rows) within the input box, so a buffer taller
    /// than the box keeps the cursor on screen (cli 2026-09-27「多行文本框」).
    input_scroll: u16,
    // ── slash-command completion palette (§12.2 ④) ─────────────
    /// Selected row in the completion palette that opens while the `/`-command
    /// word is being typed; clamped to the match list on read. The palette's
    /// *open* state is derived from the buffer (see [`Self::completion_matches`]),
    /// so it can never desync from what the user typed.
    menu_selected: usize,
    // ── message queue (cli 2026-09-24 ③, CC 「排队」) ─────────────
    /// Messages typed while a turn is in flight, in submission order. CC lets
    /// you keep typing mid-turn — Enter queues instead of being ignored — shows
    /// each as a pending `❯` bar, and drains the queue when the turn ends.
    queue: Vec<String>,
    /// Whether `ctrl+x` armed the "send now" chord (`ctrl+x ctrl+s`); cleared by
    /// any other key.
    send_now_armed: bool,
    // ── reverse history search (cli 2026-09-27, CC `ctrl+r`) ──────
    /// `ctrl+r` opens CC's **search prompts** overlay over the input box: type
    /// to filter submitted history (substring, case-insensitive), ↑/↓ pick, Enter
    /// drops the pick into the editor, Esc cancels. `false` = closed.
    search_open: bool,
    /// The live filter typed in the search overlay.
    search_query: String,
    /// Highlighted row in the filtered (newest-first) match list.
    search_selected: usize,
    // ── committed-transcript render cache (performance) ──────────
    /// The **committed** transcript, flattened under the current fold state and
    /// already pre-wrapped to the pane width ([`wrap_line`]) — i.e. exactly the
    /// screen lines for `scrollback[..cache_items]`.
    ///
    /// Rebuilt only when the transcript grows, the pane width changes, or a fold
    /// (`ctrl+o`) is toggled; otherwise it is extended **incrementally** with the
    /// newly pushed items. Without it every frame re-flattened *and* re-wrapped
    /// the entire history: a session with ~1 MB of committed reasoning cost
    /// ~500 ms/frame (and a wheel notch ~450 ms), because a folded thinking block
    /// wrapped its whole text just to show the last six rows.
    transcript_cache: Vec<Line<'static>>,
    /// How many `scrollback` items `transcript_cache` already covers.
    cache_items: usize,
    /// The pane width `transcript_cache` was wrapped at.
    cache_width: usize,
    /// The fold state (`thinking_expanded`, `output_expanded`) the cache was
    /// flattened under.
    cache_folds: (bool, bool),
    // ── in-app text selection (cli 2026-09-27「鼠标拖动选中 UI 文字」) ──
    /// The live drag selection, in **absolute transcript line** coordinates (so
    /// newly streamed lines below cannot shift it). `None` = nothing selected.
    /// The transcript must select itself because the TUI captures the mouse (§11
    /// / §16.3), so the terminal's own drag-select never fires.
    selection: Option<Selection>,
    /// The conversation pane's rect in the last frame — the region a drag may
    /// start in (a click on the input box / footer must not begin a selection).
    pane_area: Rect,
    /// Snapshot of the **visible** transcript rows as plain text, plus the
    /// absolute line index of the top row — the mapping a finished selection is
    /// resolved against (copying never re-renders the whole transcript).
    viewport_start: u32,
    viewport_lines: Vec<String>,
    /// A transient `已复制 N 字符` confirmation (message + when it was set); it
    /// self-expires after [`COPY_NOTE_SECS`]. Drawn on the **footer line** (the
    /// pane's bottom row is where a selection can end, so painting it there hid
    /// the selection — cli 2026-09-27「左下角弹出的复制提示顶掉了选择」).
    copy_note: Option<(String, Instant)>,
    // ── double-`Ctrl+C` exit (cli 2026-09-28) ─────────────────────
    /// When the **first** `Ctrl+C` armed the exit; a second press within
    /// [`EXIT_ARM_SECS`] quits, and any other key (or the deadline passing)
    /// disarms. `None` = not armed. Also drives the `再按一次 ⌃C 退出` footer
    /// hint, so the user is never surprised by a quit (cli 2026-09-28
    ///「按一下 ctrl+c 我本来想复制直接就退出去了…第一次按下后给提示」).
    exit_armed: Option<Instant>,
}

impl App {
    /// A fresh app bound to the active model.
    pub fn new(model: impl Into<String>, group: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            group: group.into(),
            version: lingmiao_core::VERSION.to_string(),
            editor: Editor::new(),
            state: State::Idle,
            stage: String::new(),
            streaming: String::new(),
            thinking: String::new(),
            thinking_stage: String::new(),
            thinking_expanded: false,
            output_expanded: false,
            thinking_started: Instant::now(),
            stream_stage: String::new(),
            running_tool: None,
            wait_notice: None,
            auto_round: 0,
            turn_tools: 0,
            turn_started: Instant::now(),
            tip_started: Instant::now(),
            error: None,
            scrollback: Vec::new(),
            width: 100,
            height: 40,
            should_quit: false,
            tokens_in: 0,
            tokens_out: 0,
            tool_calls: 0,
            elapsed_ms: 0.0,
            cwd: short_cwd(),
            whiteboard: vec![WHITEBOARD_EMPTY.to_string()],
            history: Vec::new(),
            hist_cursor: None,
            draft: String::new(),
            progress: [StageProgress::default(); 3],
            ctx_sections: Vec::new(),
            ctx_total_chars: 0,
            ctx_tokens: 0,
            scroll: 0,
            max_scroll: 0,
            anchor: None,
            input_scroll: 0,
            menu_selected: 0,
            queue: Vec::new(),
            send_now_armed: false,
            search_open: false,
            search_query: String::new(),
            search_selected: 0,
            transcript_cache: Vec::new(),
            cache_items: 0,
            cache_width: 0,
            cache_folds: (false, false),
            selection: None,
            pane_area: Rect::default(),
            viewport_start: 0,
            viewport_lines: Vec::new(),
            copy_note: None,
            exit_armed: None,
        }
    }
    /// Replace the whiteboard snapshot shown in the footer L3 summary (§12.3).
    pub fn set_whiteboard(&mut self, lines: Vec<String>) {
        self.whiteboard = if lines.is_empty() {
            vec![WHITEBOARD_EMPTY.to_string()]
        } else {
            lines
        };
    }

    /// Whether a turn is in flight (Enter is ignored; Esc cancels).
    pub fn is_busy(&self) -> bool {
        self.state == State::Streaming
    }

    /// Whether the run loop should exit.
    pub fn should_quit(&self) -> bool {
        self.should_quit
    }

    /// Take the lines queued for the conversation pane (leaves the queue empty).
    ///
    /// Foldable blocks are flattened according to the current fold state (§12.7
    /// reasoning / §12.10 tool output), so callers (loop / tests) see exactly
    /// what is on screen.
    pub fn take_scrollback(&mut self) -> Vec<Line<'static>> {
        let items = std::mem::take(&mut self.scrollback);
        self.transcript_lines(&items)
    }

    /// Wipe the rendered conversation panel (原版 `/clear` = UI only).
    ///
    /// Drops the committed transcript, its render cache, any live stream/thinking
    /// buffers and the paging/selection anchors — but **not** the engine's
    /// archive: the engine keeps no conversation state at all, so there is
    /// nothing there to clear either. The next turn's 组织上下文 still reads the
    /// archive back through `archive.recent(n)`, so clearing the panel never
    /// costs the model its memory.
    pub fn clear_transcript(&mut self) {
        self.scrollback.clear();
        self.transcript_cache.clear();
        self.cache_items = 0;
        self.streaming.clear();
        self.thinking.clear();
        self.thinking_stage.clear();
        self.stream_stage.clear();
        self.running_tool = None;
        self.wait_notice = None;
        self.error = None;
        self.selection = None;
        self.scroll = 0;
        self.anchor = None;
    }

    /// The committed transcript as renderable lines (foldable blocks flattened to
    /// the current fold state, §12.7 / §12.10; blocks already carry their
    /// one-blank-line separators, §12.14). The renderer uses the wrapped
    /// [`Self::transcript_cache`] instead (performance); this unwrapped view is
    /// kept for the tests that assert on content rather than layout.
    #[cfg(test)]
    fn history_lines(&self) -> Vec<Line<'static>> {
        self.transcript_lines(&self.scrollback)
    }

    /// Flatten a run of [`Item`]s to screen lines: each item under the current
    /// fold state. Blank separators between blocks are inserted **at push time**
    /// (§12.14, [`Self::blank_separator`]), so flattening is a straight map. The
    /// single place that decides transcript layout, so `take_scrollback` (tests /
    /// loop) and `render_conversation` never diverge.
    fn transcript_lines(&self, items: &[Item]) -> Vec<Line<'static>> {
        let width = self.width as usize;
        items
            .iter()
            .flat_map(|it| self.item_lines(it, width))
            .collect()
    }

    /// Flatten one [`Item`] to screen lines under the *current* fold state — the
    /// single place that decides what a folded thinking block (§12.7) or a folded
    /// tool output (§12.10) looks like.
    fn item_lines(&self, it: &Item, width: usize) -> Vec<Line<'static>> {
        match it {
            Item::Line(l) => vec![l.clone()],
            Item::UserTurn { lines } => lines.clone(),
            Item::Thinking { stage, text, secs } => {
                thinking_lines(stage, text, *secs, self.thinking_expanded, width)
            }
            Item::ToolOutput { header, body, diff } => {
                // cli 2026-09-28「代码改动的红绿对比」: a file mutation's diff is
                // **always** shown in full (before the `⎿` result body) — it is
                // the point of the card, and it is already capped at
                // `lingmiao_tools::diff::MAX_DIFF_LINES` at the source. The `⎿` result
                // body keeps its own 3-line fold (`ctrl+o`), unchanged.
                let mut out = vec![header.clone()];
                out.extend(diff.iter().cloned());
                let n = body.len();
                if !self.output_expanded && n > TOOL_OUTPUT_COLLAPSE_LINES {
                    out.extend(body[..TOOL_OUTPUT_HEAD_LINES].iter().cloned());
                    out.push(tool_fold_hint(n - TOOL_OUTPUT_HEAD_LINES));
                } else {
                    out.extend(body.iter().cloned());
                }
                out
            }
        }
    }

    /// Push a plain committed line into the transcript.
    fn push_line(&mut self, line: Line<'static>) {
        self.scrollback.push(Item::Line(line));
    }

    /// CC block separator (§12.14): the transcript puts exactly **one** blank line
    /// *between* blocks. A block is the user bar, a thinking block, a tool card, a
    /// reply, the tail, or a command-output group — CC separates every one of them
    /// with a single blank line (cli 2026-09-24 ③「控件间隔1行」; page41 :99 与 CC
    /// 同屏实拍逐块核对). No-op at the very start, so the transcript never opens
    /// with a leading gap. Each block calls it once, before its own first line.
    fn blank_separator(&mut self) {
        if !self.scrollback.is_empty() {
            self.scrollback.push(Item::Line(Line::from("")));
        }
    }

    /// Insert paste text into the editor (§6 痛点⑤, 零外部依赖).
    ///
    /// Both paste paths land here — the terminal's bracketed paste
    /// ([`TermEvent::Paste`]) and the run loop's [`Action::Paste`] clipboard read
    /// — so the line-ending contract lives in one place
    /// ([`crate::clipboard::normalize_paste`]: `\r\n`/`\r` → `\n`, trailing
    /// newlines dropped, so a paste never leaves a stray blank line or types a
    /// bare control character).
    ///
    /// **Mid-turn the text is no longer dropped** (cli 2026-09-29「输入框粘贴不好用」):
    /// while a turn runs the box edits the *queued* message (§「排队」), so the
    /// old refusal made `ctrl+v` a silent no-op exactly when the user was
    /// composing their next prompt — they retried, saw nothing, and concluded
    /// paste was broken. A paste is now an ordinary box edit, like typing;
    /// Enter still routes it to the queue.
    pub fn paste(&mut self, text: &str) {
        let normalised = crate::clipboard::normalize_paste(text);
        if normalised.is_empty() {
            return;
        }
        self.editor.insert_str(&normalised);
    }

    /// Handle a key press.
    pub fn on_key(&mut self, key: KeyEvent) -> Action {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        // Ctrl+C (cli 2026-09-28: 「我本来想复制直接就退出去了，这个组合键要按两次，
        // 第一次按下后给提示」):
        //   * a **live drag selection** → copy it and stop there. Nothing to copy
        //     is the *only* situation in which the press means "quit", so the
        //     terminal-style「复制我选中的东西」reflex can never end the session —
        //     not even a double-tap of it (a copy must never arm the exit either:
        //     two quick Ctrl+C aimed at "copy it again" would otherwise quit).
        //   * nothing selected → **arm** and show the `再按一次 ⌃C 退出` hint; a
        //     second press within [`EXIT_ARM_SECS`] quits (CC's own 800ms arm).
        //
        // Consequence (deliberate, documented in §28): quitting while a highlight
        // is still live needs one `Esc` first — dropping the selection is the
        // explicit "I am done selecting" signal. 灵妙 never silently trades a copy
        // for an exit.
        if ctrl && key.code == KeyCode::Char('c') {
            // The search overlay owns the keyboard while open, and CC binds
            // `ctrl+c` there to `historySearch:cancel` — so close it instead of
            // arming an exit the user did not mean.
            if self.search_open {
                self.close_search();
                return Action::None;
            }
            if let Some(text) = self.selection_text_opt() {
                return Action::Copy(text);
            }
            let armed = self
                .exit_armed
                .is_some_and(|at| at.elapsed().as_secs_f64() < EXIT_ARM_SECS);
            if armed {
                self.exit_armed = None;
                self.should_quit = true;
                return Action::Quit;
            }
            self.exit_armed = Some(Instant::now());
            return Action::None;
        }
        // Any other key disarms a pending exit: the second Ctrl+C has to be the
        // very next thing the user does (the arm is not sticky). The deadline in
        // [`EXIT_ARM_SECS`] expires it as well, so a hint never lingers.
        self.exit_armed = None;
        // Reverse history search (cli 2026-09-27, CC `ctrl+r: history:search`):
        // while the overlay is open it owns the keyboard — Esc cancels, Enter
        // accepts, typing filters, ↑/↓ pick. Any key press closes nothing else.
        if self.search_open {
            return self.search_key(key);
        }
        // `ctrl+r` opens it (pre-filled with the current draft, as CC passes the
        // input as `initialQuery`). Only when idle — mid-turn the box edits the
        // queue instead.
        if ctrl && key.code == KeyCode::Char('r') {
            if !self.is_busy() {
                self.search_query = self.editor.text.clone();
                self.search_selected = 0;
                self.search_open = true;
            }
            return Action::None;
        }
        // Ctrl+X Ctrl+S sends the front queued message now (cli 2026-09-24 ③,
        // CC 「ctrl+x ctrl+s to send now」): Ctrl+X arms, Ctrl+S fires; any other
        // key disarms the chord.
        if ctrl && key.code == KeyCode::Char('x') {
            self.send_now_armed = true;
            return Action::None;
        }
        if ctrl && key.code == KeyCode::Char('s') {
            let armed = self.send_now_armed;
            self.send_now_armed = false;
            if armed && !self.queue.is_empty() {
                return Action::SendQueuedNow;
            }
            return Action::None;
        }
        self.send_now_armed = false;
        // Ctrl+O toggles every fold — the reasoning blocks (§12.7) *and* the
        // collapsed tool output (§12.10 P1-a) — so one key reveals all hidden
        // detail. Works whether idle or mid-turn.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('o') {
            self.thinking_expanded = !self.thinking_expanded;
            self.output_expanded = !self.output_expanded;
            return Action::None;
        }
        // Copy the live mouse selection (cli 2026-09-27: 「不需要马上复制，让用户
        // 自己选择…用户如果需要自己按快捷键复制」). The drag only *selects*; the
        // clipboard write is this explicit keystroke — `ctrl+y` (tmux copy-mode's
        // copy key). Nothing selected → the key is inert.
        //
        // cli 2026-09-28: `shift+insert` moved to **paste** (it is the classic
        // X11/WSL paste chord) and `ctrl+c` now doubles as a copy when something
        // is selected, so `ctrl+y` stays the one unambiguous copy key.
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        if ctrl && key.code == KeyCode::Char('y') {
            if let Some(text) = self.selection_text_opt() {
                return Action::Copy(text);
            }
            return Action::None;
        }
        // Paste from the OS clipboard (cli 2026-09-28「默认支持 win 和 linux 的复制
        // 粘贴」): `ctrl+v` (and the `cmd+v`/Windows `alt+v` aliases CC maps per
        // platform) asks the run loop to read the clipboard and insert it. Most
        // terminals already deliver a *bracketed* paste as a `Paste` event, but a
        // bare `ctrl+v` carries no text, so the app must fetch it. Reading the
        // clipboard is I/O — the loop does it and calls [`App::paste`], keeping
        // this handler pure.
        let paste_key =
            (ctrl || key.modifiers.contains(KeyModifiers::SUPER)) && key.code == KeyCode::Char('v');
        if paste_key {
            return Action::Paste;
        }
        // `shift+insert` is the classic X11/WSL paste chord (and is not used for
        // copy any more — that stays `ctrl+y`).
        if shift && key.code == KeyCode::Insert {
            return Action::Paste;
        }
        // Esc cancels an in-flight turn (Q2/M5); otherwise it clears the input
        // (and leaves history browsing). It also drops a live selection first —
        // that is the escape hatch for a drag the user does not want.
        if key.code == KeyCode::Esc {
            if self.selection.is_some() {
                self.selection = None;
                return Action::None;
            }
            if self.is_busy() {
                return Action::Cancel;
            }
            self.editor.clear();
            self.hist_cursor = None;
            return Action::None;
        }
        // Slash-command completion palette (§12.2 ④): while the command *word*
        // is being typed — a single-line buffer beginning with `/` and no space
        // yet — ↑/↓ walk the matching commands and `Tab` fills the highlighted
        // one, mirroring CC's `/` palette. The palette is *derived* from the
        // buffer, so every other key simply falls through to editing.
        if !self.is_busy() {
            let matches = self.completion_matches();
            if !matches.is_empty() {
                let sel = self.menu_index(matches.len());
                match key.code {
                    KeyCode::Up => {
                        self.menu_selected = sel.saturating_sub(1);
                        return Action::None;
                    }
                    KeyCode::Down => {
                        self.menu_selected = (sel + 1).min(matches.len() - 1);
                        return Action::None;
                    }
                    KeyCode::Tab => {
                        let name = COMMANDS[matches[sel]].0;
                        self.editor.set_text(format!("/{name}"));
                        self.menu_selected = 0;
                        return Action::None;
                    }
                    _ => {}
                }
            }
        }
        // Shift/Alt+Enter insert a newline (Enter sends) — multi-line editing
        // (§6 ①). Alt+Enter covers terminals (e.g. plain xterm) that don't
        // report the SHIFT modifier on Enter. Handled before the sidebar-nav
        // branch so it always edits.
        if key.code == KeyCode::Enter
            && (key.modifiers.contains(KeyModifiers::SHIFT)
                || key.modifiers.contains(KeyModifiers::ALT))
        {
            if !self.is_busy() {
                self.editor.insert_char('\n');
            }
            return Action::None;
        }
        // Ctrl+J also inserts a newline — the one newline key that works on
        // terminals (e.g. plain xterm) that cannot report ⇧/⌥ on Enter. Arrives
        // as `Char('j')` + CONTROL (distinct from Enter, which sends).
        if key.code == KeyCode::Char('j') && key.modifiers.contains(KeyModifiers::CONTROL) {
            if !self.is_busy() {
                self.editor.insert_char('\n');
            }
            return Action::None;
        }
        // Page the conversation (§6 ② / §11): the alternate buffer has no
        // native scrollback, so the app owns paging. `Home`/`End` now move the
        // *cursor* within the current input line (§②), so paging to the ends is
        // bound to `Ctrl+Home` / `Ctrl+End`.
        match key.code {
            KeyCode::PageUp => {
                self.scroll_up(10);
                return Action::None;
            }
            KeyCode::PageDown => {
                self.scroll_down(10);
                return Action::None;
            }
            KeyCode::Home if ctrl => {
                self.scroll = self.max_scroll;
                self.anchor = Some(0);
                return Action::None;
            }
            KeyCode::End if ctrl => {
                self.scroll = 0;
                self.anchor = None;
                return Action::None;
            }
            _ => {}
        }
        // ↑/↓: within a multi-line buffer they move between lines (preserving
        // the column preference); a single-line draft browses submitted-input
        // history exactly as before — which is what CC does too (§②).
        if !self.is_busy() {
            match key.code {
                KeyCode::Up => {
                    if self.editor.text.contains('\n') {
                        self.editor.move_up();
                    } else {
                        self.browse_history(-1);
                    }
                    return Action::None;
                }
                KeyCode::Down => {
                    if self.editor.text.contains('\n') {
                        self.editor.move_down();
                    } else {
                        self.browse_history(1);
                    }
                    return Action::None;
                }
                _ => {}
            }
        }
        // Mid-turn ↑ with an empty box pulls the newest queued message back into
        // the editor for editing (cli 2026-09-24 ③, CC 「Press up to edit queued
        // messages」). Enter re-queues it while the turn is still running.
        if self.is_busy()
            && key.code == KeyCode::Up
            && self.editor.is_empty()
            && !self.queue.is_empty()
        {
            if let Some(msg) = self.queue.pop() {
                self.editor.set_text(msg);
            }
            return Action::None;
        }
        // Cursor movement + line-editing (§②). Alt/Ctrl+←/→ step by word; plain
        // Home/End go to the current line's start/end; Ctrl+A/E are the emacs
        // equivalents (CC supports both).
        match key.code {
            KeyCode::Left if ctrl || alt => {
                self.editor.move_word_left();
                return Action::None;
            }
            KeyCode::Right if ctrl || alt => {
                self.editor.move_word_right();
                return Action::None;
            }
            KeyCode::Left => {
                self.editor.move_left();
                return Action::None;
            }
            KeyCode::Right => {
                self.editor.move_right();
                return Action::None;
            }
            KeyCode::Home => {
                self.editor.move_home();
                return Action::None;
            }
            KeyCode::End => {
                self.editor.move_end();
                return Action::None;
            }
            _ => {}
        }
        // Ctrl+letter editing commands (§②): kill line / kill word / undo.
        if ctrl {
            match key.code {
                KeyCode::Char('a') => {
                    self.editor.move_home();
                    return Action::None;
                }
                KeyCode::Char('e') => {
                    self.editor.move_end();
                    return Action::None;
                }
                KeyCode::Char('u') => {
                    self.editor.kill_to_line_start();
                    return Action::None;
                }
                KeyCode::Char('k') => {
                    self.editor.kill_to_line_end();
                    return Action::None;
                }
                KeyCode::Char('w') => {
                    self.editor.delete_word_before();
                    return Action::None;
                }
                KeyCode::Char('z') => {
                    self.editor.undo();
                    return Action::None;
                }
                _ => {}
            }
        }
        match key.code {
            KeyCode::Enter => {
                // Mid-turn Enter queues the line (cli 2026-09-24 ③, CC 「Enter to
                // queue up additional messages while Claude is working」) instead
                // of dropping it — the run loop drains the queue when the turn
                // ends.
                if self.is_busy() {
                    let text = self.editor.text.trim().to_string();
                    if !text.is_empty() {
                        self.editor.clear();
                        self.hist_cursor = None;
                        self.queue.push(text);
                    }
                    return Action::None;
                }
                // While the completion palette is open, Enter runs the
                // *highlighted* command (CC behaviour: the palette is the input).
                // This also gives a bare `/` + Enter a sensible meaning (help)
                // instead of the old `未知命令：/`.
                let matches = self.completion_matches();
                if !matches.is_empty() {
                    let sel = self.menu_index(matches.len());
                    let text = format!("/{}", COMMANDS[matches[sel]].0);
                    self.editor.clear();
                    self.hist_cursor = None;
                    self.history.push(text.clone());
                    self.menu_selected = 0;
                    return self.slash(text.trim_start_matches('/'));
                }
                let text = self.editor.text.trim().to_string();
                if text.is_empty() {
                    return Action::None;
                }
                self.editor.clear();
                self.hist_cursor = None;
                self.history.push(text.clone());
                if let Some(cmd) = text.strip_prefix('/') {
                    return self.slash(cmd);
                }
                self.begin_turn(&text);
                Action::Submit(text)
            }
            KeyCode::Backspace => {
                self.editor.backspace();
                self.menu_selected = 0;
                Action::None
            }
            KeyCode::Delete => {
                self.editor.delete();
                self.menu_selected = 0;
                Action::None
            }
            KeyCode::Char(c) => {
                if !ctrl && !alt {
                    self.editor.insert_char(c);
                    self.menu_selected = 0;
                }
                Action::None
            }
            _ => Action::None,
        }
    }

    /// Indices into [`COMMANDS`] that match the buffer's command word — the
    /// completion palette's contents (§12.2 ④).
    ///
    /// The palette opens while the **command word** is being typed: a
    /// single-line buffer that begins with `/` and has **no space yet** (typing
    /// an argument closes it). Empty while a turn is in flight. Pure over the
    /// buffer, so both the key handler and the renderer agree on when it shows.
    fn completion_matches(&self) -> Vec<usize> {
        if self.is_busy() || self.editor.text.contains('\n') {
            return Vec::new();
        }
        let Some(prefix) = self.editor.text.strip_prefix('/') else {
            return Vec::new();
        };
        if prefix.contains(' ') {
            return Vec::new();
        }
        COMMANDS
            .iter()
            .enumerate()
            .filter(|(_, (name, _))| name.starts_with(prefix))
            .map(|(i, _)| i)
            .collect()
    }

    /// The selected row in the completion palette, clamped to `len` matches so a
    /// shrinking list (the user deleting characters) can never index out of it.
    fn menu_index(&self, len: usize) -> usize {
        self.menu_selected.min(len.saturating_sub(1))
    }

    /// Browse submitted-input history with ↑ (`delta = -1`) / ↓ (`+1`).
    ///
    /// Entering history stashes the live draft; stepping ↓ past the newest entry
    /// restores that draft. Returns `true` when it consumed the key.
    fn browse_history(&mut self, delta: i32) -> bool {
        let n = self.history.len();
        if n == 0 {
            return false;
        }
        match (self.hist_cursor, delta) {
            // Start at the newest entry, stashing the current draft.
            (None, -1) => {
                self.draft = self.editor.text.clone();
                self.hist_cursor = Some(n - 1);
            }
            (Some(i), -1) => self.hist_cursor = Some(i.saturating_sub(1)),
            (Some(i), 1) if i + 1 < n => self.hist_cursor = Some(i + 1),
            // ↓ past the newest → restore the draft and stop browsing.
            (Some(_), 1) => {
                self.hist_cursor = None;
                let draft = std::mem::take(&mut self.draft);
                self.editor.set_text(draft);
                return true;
            }
            _ => return false,
        }
        if let Some(i) = self.hist_cursor {
            self.editor.set_text(self.history[i].clone());
        }
        true
    }

    /// Indices into [`Self::history`] matching the current search filter, **newest
    /// first** (CC's history picker lists the most recent prompt on top). An empty
    /// filter matches everything. Case-insensitive substring match — the same
    /// "exact" pass CC's `HistorySearchDialog` runs before its fuzzy pass; we keep
    /// just the substring rule so the behaviour is simple and testable.
    fn search_matches(&self) -> Vec<usize> {
        let q = self.search_query.to_lowercase();
        (0..self.history.len())
            .rev()
            .filter(|&i| q.is_empty() || self.history[i].to_lowercase().contains(&q))
            .collect()
    }

    /// Handle a key while the reverse-history-search overlay is open (cli
    /// 2026-09-27, CC `ctrl+r`).
    fn search_key(&mut self, key: KeyEvent) -> Action {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => self.close_search(),
            KeyCode::Enter => {
                if let Some(text) = self.take_search_selection() {
                    self.editor.set_text(text);
                    self.hist_cursor = None;
                }
                self.close_search();
            }
            KeyCode::Up => {
                self.search_selected = self.search_selected.saturating_sub(1);
            }
            KeyCode::Down => {
                let n = self.search_matches().len();
                if self.search_selected + 1 < n {
                    self.search_selected += 1;
                }
            }
            KeyCode::Backspace => {
                self.search_query.pop();
                self.search_selected = 0;
            }
            KeyCode::Char(c) if !ctrl => {
                self.search_query.push(c);
                self.search_selected = 0;
            }
            _ => {}
        }
        Action::None
    }

    /// Close the search overlay and forget its transient filter/selection.
    fn close_search(&mut self) {
        self.search_open = false;
        self.search_query.clear();
        self.search_selected = 0;
    }

    /// The history entry under the current selection (clamped), for Enter to drop
    /// into the editor.
    fn take_search_selection(&self) -> Option<String> {
        let matches = self.search_matches();
        let sel = self.search_selected.min(matches.len().saturating_sub(1));
        matches.get(sel).map(|&i| self.history[i].clone())
    }

    /// Scroll the conversation `n` lines **up** (older), clamped to the top
    /// (§11). Driven by `PageUp` and the mouse wheel.
    ///
    /// The viewport top is pinned through [`Self::anchor`] so lines streamed in
    /// below do not push the view back down (cli 2026-09-27「滚轮翻动后位置保持」).
    pub fn scroll_up(&mut self, n: u16) {
        let top = self.anchor.unwrap_or(self.max_scroll).saturating_sub(n);
        self.anchor = Some(top);
        self.scroll = self.max_scroll.saturating_sub(top);
    }

    /// Scroll the conversation `n` lines **down** (newer), clamped at the bottom
    /// (§11). Driven by `PageDown` and the mouse wheel. Reaching the bottom
    /// releases the pin and resumes following the newest line.
    pub fn scroll_down(&mut self, n: u16) {
        let top = self.anchor.unwrap_or(self.max_scroll).saturating_add(n);
        if top >= self.max_scroll {
            self.anchor = None;
            self.scroll = 0;
        } else {
            self.anchor = Some(top);
            self.scroll = self.max_scroll.saturating_sub(top);
        }
    }

    /// Handle a mouse event (cli 2026-09-27「鼠标拖动可以选中 UI 上的文字」).
    ///
    /// The app owns every state change (it alone knows the pane layout and the
    /// transcript); mouse handling therefore has **no** outside effect any more —
    /// the old version returned a [`MouseAction::Copy`] on release, which both
    /// copied too eagerly and repainted a notice over the selection. A finished
    /// drag now just leaves the selection in place; the copy is an explicit
    /// keystroke ([`Self::on_key`]).
    ///
    /// The transcript must implement selection itself: the TUI captures the mouse
    /// (§11 / §16.3), so a terminal's own drag-select never sees the drag.
    pub fn on_mouse(&mut self, m: MouseEvent) {
        match m.kind {
            // Wheel → conversation paging (§11; cli 2026-09-21).
            MouseEventKind::ScrollUp => self.scroll_up(WHEEL_LINES),
            MouseEventKind::ScrollDown => self.scroll_down(WHEEL_LINES),
            // A press inside the pane starts (or restarts) a selection. **Left and
            // right both work** (cli 2026-09-27: 「右键拖动也要能选择」 — a right
            // drag in a mouse-capturing TUI has no other meaning anyway). A press
            // outside the pane (input box / footer) drops any existing selection.
            MouseEventKind::Down(MouseButton::Left | MouseButton::Right) => {
                self.selection = self.pane_pos(m.column, m.row).map(|pos| Selection {
                    anchor: pos,
                    head: pos,
                });
            }
            // Dragging extends it; a drag outside the pane clamps to the edge
            // (the pointer may leave the pane mid-drag, which must not cancel it).
            MouseEventKind::Drag(MouseButton::Left | MouseButton::Right) => {
                if let Some(pos) = self.pane_pos_clamped(m.column, m.row) {
                    if let Some(sel) = self.selection.as_mut() {
                        sel.head = pos;
                    }
                }
            }
            // Release **keeps** the selection (cli 2026-09-27: 「不需要马上复制，
            // 让用户自己选择，也不要顶掉」) — only a press+release that never
            // moved (a bare click) clears it, so a stray click cannot leave a
            // highlight behind. The actual copy waits for `ctrl+y`.
            MouseEventKind::Up(MouseButton::Left | MouseButton::Right) => {
                if let Some(sel) = self.selection {
                    if sel.anchor == sel.head {
                        self.selection = None;
                    }
                }
            }
            _ => {}
        }
    }

    /// The text of the live selection, or `None` when there is nothing to copy
    /// (no selection, an empty drag, or a selection covering only blank space).
    /// Used by the explicit copy shortcut and by the fold-state tests.
    pub(crate) fn selection_text_opt(&self) -> Option<String> {
        let text = self.selection_text(self.selection?);
        if text.trim().is_empty() {
            None
        } else {
            Some(text)
        }
    }

    /// The transcript coordinate under a screen cell, or `None` when the cell is
    /// outside the conversation pane (so a click on the input box or footer never
    /// begins a selection).
    fn pane_pos(&self, col: u16, row: u16) -> Option<(u32, u16)> {
        let pane = self.pane_area;
        if self.viewport_lines.is_empty() || pane.width == 0 || pane.height == 0 {
            return None;
        }
        if col < pane.x || col >= pane.right() || row < pane.y || row >= pane.bottom() {
            return None;
        }
        // Rows below the last rendered line (a short transcript) clamp to it,
        // never past it — otherwise a click in the empty space would select a
        // line that does not exist.
        let last = self.viewport_start + self.viewport_lines.len().saturating_sub(1) as u32;
        let line = (self.viewport_start + (row - pane.y) as u32).min(last);
        Some((line, col - pane.x))
    }

    /// Like [`Self::pane_pos`] but clamped into the pane — used while dragging,
    /// where the pointer may wander off the pane and the selection must keep
    /// extending to the nearest edge instead of freezing.
    fn pane_pos_clamped(&self, col: u16, row: u16) -> Option<(u32, u16)> {
        let pane = self.pane_area;
        if self.viewport_lines.is_empty() || pane.width == 0 || pane.height == 0 {
            return None;
        }
        let col = col.clamp(pane.x, pane.right().saturating_sub(1));
        let row = row.clamp(pane.y, pane.bottom().saturating_sub(1));
        let last = self.viewport_start + self.viewport_lines.len().saturating_sub(1) as u32;
        let line = (self.viewport_start + (row - pane.y) as u32).min(last);
        Some((line, col - pane.x))
    }

    /// The text covered by a selection, in reading order.
    ///
    /// Both ends are clamped into the visible window (a selection only ever spans
    /// what was on screen when it was made), each line's trailing padding is
    /// dropped, and lines are joined with `\n` — the shape a paste expects.
    fn selection_text(&self, sel: Selection) -> String {
        let (start_line, lines) = (self.viewport_start, &self.viewport_lines);
        if lines.is_empty() {
            return String::new();
        }
        let last = start_line + lines.len() as u32 - 1;
        let (mut a, mut b) = (sel.anchor, sel.head);
        if (a.0, a.1) > (b.0, b.1) {
            std::mem::swap(&mut a, &mut b);
        }
        let a_line = a.0.clamp(start_line, last);
        let b_line = b.0.clamp(start_line, last);
        let mut out: Vec<String> = Vec::new();
        for line in a_line..=b_line {
            let text = &lines[(line - start_line) as usize];
            let chars: Vec<char> = text.chars().collect();
            let from = if line == a_line {
                display_col_to_char_idx(text, a.1)
            } else {
                0
            };
            let to = if line == b_line {
                display_col_to_char_idx(text, b.1)
            } else {
                chars.len()
            };
            let from = from.min(chars.len());
            let to = to.max(from).min(chars.len());
            out.push(chars[from..to].iter().collect::<String>());
        }
        out.iter()
            .map(|l| l.trim_end())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Dispatch a `/`-command (without the leading slash) — §12.4: the sidebar
    /// function navigation became `/`-commands.
    fn slash(&mut self, cmd: &str) -> Action {
        let (name, rest) = cmd.split_once(' ').unwrap_or((cmd, ""));
        match name {
            "help" | "?" => {
                self.push_help();
                Action::None
            }
            "board" | "whiteboard" | "wb" => {
                let mut out = vec!["小白板（当前页）：".to_string()];
                out.extend(self.whiteboard.iter().cloned());
                let refs: Vec<&str> = out.iter().map(String::as_str).collect();
                self.push_info(&refs);
                Action::None
            }
            "memory" => Action::Stats,
            "auto" => {
                if rest.trim().is_empty() {
                    self.push_error("用法：/auto <目标>（例如 /auto 给 utils 加单元测试）");
                    Action::None
                } else {
                    Action::Auto(rest.trim().to_string())
                }
            }
            "round" => {
                if rest.trim().is_empty() {
                    self.push_error("用法：/round <目标>（单轮审计：Main 产出 → Auditor 核验）");
                    Action::None
                } else {
                    Action::Round(rest.trim().to_string())
                }
            }
            "session" => {
                self.push_info(&[
                    &format!(
                        "会话：{}↓ tokens · {}↑ tokens · 🔧 {} 次工具 · 用时 {:.1}s",
                        fmt_tokens(self.tokens_in),
                        fmt_tokens(self.tokens_out),
                        self.tool_calls,
                        self.elapsed_ms / 1000.0
                    ),
                    &format!("本轮阶段：{}", self.stage_timings()),
                ]);
                Action::None
            }
            "tools" => {
                self.push_info(&[
                    "可用工具组：",
                    "  file  read_file/write_file/edit/glob/grep/list_directory/copy_file/bash",
                    "  memory search_observations/search_knowledge/search_archive/update_*",
                    "  whiteboard whiteboard_* · business business_db_* · help",
                ]);
                Action::None
            }
            "model" | "config" => {
                self.push_info(&[
                    &format!("当前模型：{}/{}", self.group, self.model),
                    "改配置：编辑程序同级或当前目录的 config.json —— models 放模型目录，stages 给每个阶段配模型（见 /help 的 models 主题）",
                ]);
                Action::None
            }
            "clear" => {
                self.clear_transcript();
                self.push_info(&["· 对话面板已清空（历史仍在引擎归档中，新消息从这里开始）"]);
                Action::Clear
            }
            "quit" | "exit" | "q" => {
                self.should_quit = true;
                Action::Quit
            }
            other => {
                self.push_error(&format!("未知命令：/{other} —— 试试 /help"));
                Action::None
            }
        }
    }

    fn push_help(&mut self) {
        // One block → one `push_info` call, so the help text gets a single
        // one-blank-line prefix rather than one per line (§12.14).
        let mut lines: Vec<String> = vec!["commands:".to_string()];
        for (name, desc) in COMMANDS {
            lines.push(format!("  /{name:<9} {desc}"));
        }
        lines.extend([
            "keys: Enter 发送 · ⇧Enter/⌃J 换行 · ←→ 移光标 · Home/End 行首尾 · ⌃A/⌃E 行首尾"
                .to_string(),
            "      ⌃←/⌃→ 按词移动 · ⌃U/⌃K 删到行首/行尾 · ⌃W 删前一词 · ⌃Z 撤销".to_string(),
            "      ↑↓ 历史(单行)/行间(多行) · ⌃R 搜索历史 · ⌃Y 复制选中 · ⌃V 粘贴 · PgUp/PgDn 翻页 · ⌃Home/⌃End 顶/底 · ⌃O 展开折叠 · Esc 取消".to_string(),
            "      ⌃C 无选中时连按两次退出（第一次给提示）；有拖选时 ⌃C 只复制（先 Esc 丢选择再按两次才退出）"
                .to_string(),
            "mouse: 滚轮翻页 · 左/右键拖动选中对话文字，再按 ⌃Y 复制（有选中时 ⌃C 亦可复制）".to_string(),
        ]);
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        self.push_info(&refs);
    }

    /// Apply an engine event (streamed over the bus).
    pub fn on_event(&mut self, event: &Event) {
        match event {
            Event::StageStarted { stage, .. } => {
                self.state = State::Streaming;
                self.stage = stage.clone();
                if let Some(i) = stage_index(stage) {
                    self.progress[i] = StageProgress {
                        status: StageStatus::Running,
                        elapsed_ms: 0.0,
                    };
                }
            }
            Event::LlmDelta { stage, kind, text } => {
                // The conversation shows every pipeline stage's *prose*, each
                // tagged with its stage (§4): B streams retrieval reasoning, C
                // streams the reply. 沉淀阶段 streams machine JSON — not prose —
                // so its deltas stay out of the transcript.
                if self.state == State::Streaming && stage != STAGE_SUMMARY {
                    match kind.as_str() {
                        // Only C's `content` is the user-facing reply; B's
                        // `content` is the injected context block (an internal
                        // artifact, surfaced via its tool cards instead).
                        // The reply text. 工作阶段's `content` is the user-facing
                        // answer; 自动模式 (cli 2026-10-04「各控件也没有显示阶段和
                        // 角色」— Main/Auditor 的**正文整段不显示**) streams its
                        // reply under `Auto·Main` / `Auto·Auditor`, which `== STAGE_C`
                        // never matched, so the role's answer produced no `●` block
                        // at all. Both are the answer stream; 组织上下文's `content`
                        // stays out (it is the injected context block, surfaced via
                        // its tool cards — see the comment above).
                        "content" if stage == STAGE_C || is_auto_stage(stage) => {
                            // The answer starts → flush any pending reasoning
                            // first, so `∴` lines stay above the `●` reply.
                            self.commit_thinking();
                            self.stream_stage = stage.clone();
                            self.streaming.push_str(text);
                        }
                        "reasoning" => {
                            // A new segment starts when the buffer was empty (the
                            // previous one was committed) — stamp its start so the
                            // folded summary can report the duration (§12.7).
                            if self.thinking.is_empty() {
                                self.thinking_started = Instant::now();
                            }
                            self.thinking_stage = stage.clone();
                            self.thinking.push_str(text);
                        }
                        _ => {}
                    }
                }
            }
            // Tool cards are shown for the *user-facing* stages (组织上下文 + 工作阶段)
            // so each call is attributable to its stage (§4). 沉淀阶段's tools are
            // internal bookkeeping writes; they stay out of the transcript.
            Event::ToolStarted {
                stage, tool, args, ..
            } if stage != STAGE_SUMMARY => {
                // A tool is about to run: flush any reasoning *and* whatever text
                // the model streamed before deciding to call it (its "preamble"),
                // so the next tool-loop iteration starts from a clean buffer.
                //
                // The C-stage tool loop re-invokes the model after every tool
                // result; without this flush every iteration appended into the
                // same `streaming` buffer, so the final reply echoed the earlier
                // preambles and reasoning (T14 回复重复错乱).
                self.commit_thinking();
                self.flush_answer();
                // A new call starts → the previous wait's heartbeat no longer
                // describes anything on screen (cli 2026-10-05, F 项 可见化).
                self.wait_notice = None;
                self.running_tool = Some(RunningTool {
                    stage: stage.clone(),
                    tool: tool.clone(),
                    args: args.clone(),
                    started: Instant::now(),
                });
            }
            Event::ToolCalled {
                stage,
                tool,
                args,
                result_preview,
                ms,
                error,
                diff,
                ..
            } if stage != STAGE_SUMMARY => {
                self.tool_calls += 1;
                self.turn_tools += 1;
                self.running_tool = None;
                // The wait that belonged to this card is over; its settled
                // duration is on the card itself (cli 2026-10-05).
                self.wait_notice = None;
                // cli 2026-09-28 (「参照 CC 实现代码改动时的红绿对比格式显示样式，
                // 包括写入的时候也是」): a file mutation's display-only diff rides the
                // event; the card paints it green/red (CC's `Edit` / `Write` cards).
                let diff_lines = parse_diff(diff);
                self.push_tool_card(
                    stage,
                    tool,
                    args,
                    result_preview,
                    &ToolOutcome {
                        ms: *ms,
                        error: *error,
                        diff: diff_lines,
                    },
                );
            }
            // **Every** LLM round-trip reports its own provider-measured usage
            // (cli 2026-09-28「上下文显示还是不对，你显示当前 llm 的输入 tokens 就行」).
            // The bottom-right figure follows **the call that just happened** —
            // 组织上下文, each 工作阶段 round-trip, 沉淀阶段 — instead of waiting for
            // the stage to finish (which showed the previous turn's number all
            // through 组织上下文 and the 工作阶段 stream) or tracking 工作阶段 alone.
            Event::LlmResponse { usage, .. } => {
                if usage.input_tokens > 0 {
                    self.ctx_tokens = usage.input_tokens;
                }
            }
            Event::StageResultReported {
                stage,
                ok,
                tokens,
                elapsed_ms,
                fault_detail,
                ..
            } => {
                // §7 本轮处理: record the real per-stage outcome + timing.
                if let Some(i) = stage_index(stage) {
                    self.progress[i] = StageProgress {
                        status: if *ok {
                            StageStatus::Done
                        } else {
                            StageStatus::Fault
                        },
                        elapsed_ms: *elapsed_ms,
                    };
                }
                // Real token accounting from the provider usage.
                if let Some(inp) = tokens.get("input_tokens").and_then(|v| v.as_u64()) {
                    self.tokens_in += inp;
                    // §8.5 fallback: the stage's own last round-trip, used only
                    // when no per-round-trip `llm_response` arrived (older
                    // payloads / a stage whose transport never reported usage).
                    // The normal path is the `Event::LlmResponse` arm above,
                    // which keeps the figure live *during* the turn.
                    if stage.as_str() == STAGE_C {
                        self.ctx_tokens = tokens
                            .get("input_tokens_last")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(inp);
                    }
                }
                if let Some(out) = tokens.get("output_tokens").and_then(|v| v.as_u64()) {
                    self.tokens_out += out;
                }
                // NB: the session's `⏱` total is *not* a per-stage max (that used
                // to make it equal the slowest single stage — T3). It accumulates
                // whole-turn durations in `on_turn_done`/`on_cancel`.
                if !fault_detail.is_empty() {
                    self.error = Some(fault_detail.clone());
                }
            }
            Event::SummaryReported { error, .. } if !error.is_empty() => {
                self.error = Some(error.clone());
            }
            // cli 2026-09-30 — the A→B **handoff notice**: a one-line, muted
            // transcript entry stating what 组织上下文 selected and the exact shape
            // the block takes inside 工作阶段's context (from the engine, so the
            // wording can never drift from the real injection).
            Event::ContextHandoff {
                from,
                to,
                observations,
                nodes,
                recent_turns,
                chars,
                prefix,
                role,
                position,
            } => {
                self.push_handoff(&HandoffNotice {
                    from,
                    to,
                    observations: *observations,
                    nodes: *nodes,
                    recent_turns: *recent_turns,
                    chars: *chars,
                    prefix,
                    role,
                    position,
                });
            }
            Event::ContextUsage {
                sections,
                total_chars,
                ..
            } => {
                // §8.5: real per-section character counts of the injection.
                self.ctx_sections = sections
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|s| {
                                let name = s.get("name")?.as_str()?.to_string();
                                let chars = s.get("chars").and_then(|c| c.as_u64()).unwrap_or(0);
                                Some((name, chars))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                self.ctx_total_chars = *total_chars;
            }
            // **The wait heartbeat** (cli 2026-10-05「要进界面的，这是核心体验」).
            // The F 项 poll samples every 5s; each sample lands here and is drawn
            // as one live line under the running card, so a long silent command
            // visibly *ticks* (已等 / 静默 / 裁判结论) instead of the transcript
            // sitting motionless for a minute — which is exactly what made cli
            // report «调用 bash 的时候没有轮询».
            Event::WaitPolled {
                class,
                what,
                elapsed_ms,
                silent_ms,
                phase,
                detail,
            } => {
                if phase == PHASE_DONE {
                    // The wait is over; the tool card that follows carries the
                    // settled duration, so the live line goes away.
                    self.wait_notice = None;
                } else {
                    // `rulings` counts the **consultations** (one `asking` per
                    // ruling): each is an extra model call, so showing the count
                    // keeps the poll's cost visible next to its benefit.
                    let mut rulings = self.wait_notice.as_ref().map(|w| w.rulings).unwrap_or(0);
                    if phase == PHASE_ASKING {
                        rulings += 1;
                    }
                    self.wait_notice = Some(WaitNotice {
                        class: class.clone(),
                        what: what.clone(),
                        elapsed_ms: *elapsed_ms,
                        silent_ms: *silent_ms,
                        phase: phase.clone(),
                        detail: detail.clone(),
                        rulings,
                    });
                }
            }
            // **自动模式进度**（cli 2026-10-04）: 原版把 `AutonomousLoop._emit()`
            // 的每一条（`▶ 启动` / `🤖 Main·第N轮 — 开始` / `🔍 Auditor·…` / `✅ 完成`）
            // 推进对话流；Rust 侧经 `Event::AutoNotice` 走同一条总线、同一个事件循环。
            // `tag` 决定渲染层次：`error` → 红字错误行；其余 → 灰色提示行（chrome），
            // 与引擎自带的阶段提示同层（TUI 配色规则：要读的信息用白、可略过的用灰）。
            //
            // cli 2026-10-04（「自动模式需要显示轮次，也就是到第几轮了」）：`iteration`
            // 非零时更新 [`Self::auto_round`]，把轮次钉在 footer 左槽 + 活动行上
            // （提示本身仍照旧进对话流）。
            Event::AutoNotice {
                tag,
                text,
                iteration,
            } => {
                if *iteration > 0 {
                    self.auto_round = *iteration;
                }
                match tag.as_str() {
                    "error" => self.push_error(text),
                    _ => self.push_info(&[text.as_str()]),
                }
            }
            _ => {}
        }
    }

    /// Finish the in-flight turn.
    pub fn on_turn_done(&mut self, result: Result<SummaryReport, LingmiaoError>) {
        self.state = State::Idle;
        self.stage.clear();
        self.running_tool = None;
        self.wait_notice = None;
        // §8 session total (T3): sum whole-turn wall-clock durations — *not* the
        // slowest single stage (which is what a per-stage max produced).
        self.elapsed_ms += self.turn_started.elapsed().as_millis() as f64;
        self.commit_thinking();
        self.flush_answer();
        match result {
            Ok(report) => {
                if report.error.is_empty() {
                    // Robustness (cli 2026-09-27): a stage may have reported a
                    // fault during the turn while the pipeline still finished
                    // `Ok` — the 2026-09-27 bug was a C-stage HTTP 400 that left
                    // `self.error` set but ended the turn on a bare `◜ done`, so
                    // the user saw no answer and no reason. Surface **any**
                    // recorded error here, so no fault is ever swallowed.
                    if let Some(err) = self.error.clone() {
                        self.push_error_detail(&err);
                    }
                    // CC-style tail: `◜ {动词} for 4.2s · done 18:39` (cli
                    // 2026-09-24: the `· N 个工具` / `· 总结 type=… audit=…`
                    // chatter was pure noise — CC prints neither).
                    self.push_tail(self.turn_started.elapsed().as_secs_f64());
                } else {
                    self.error = Some(report.error.clone());
                    self.push_error(&format!("沉淀阶段失败：{}", report.error));
                }
            }
            Err(e) => {
                self.error = Some(e.to_string());
                self.push_error_detail(&format!("回合失败：{e}"));
            }
        }
    }

    /// 打开一个**自动模式回合**（`/auto` / `/round`）：把目标写成用户提问条，
    /// 进入 Streaming 态，复用同一套阶段进度与滚动复位。
    ///
    /// 与原版一致：目标本身就是这一轮的输入，Main / Auditor 的进度经
    /// [`Event::AutoNotice`] 陆续流进来；回合结束时由 run loop 调
    /// [`App::end_auto_turn`] 收尾。
    pub fn begin_auto_turn(&mut self, goal: &str) {
        self.begin_turn(goal);
    }

    /// 收尾自动模式回合（run loop 在 `/auto` 或 `/round` 的任务结束时调用）。
    ///
    /// 走 [`App::on_turn_done`] 的**成功**分支语义（`state` 落回 Idle、提交折叠中
    /// 的思考与流式正文、记一轮耗时），但**不打印**回合尾行与错误提示 —— 自动
    /// 模式的终局话由引擎经 `Event::AutoNotice` 播报。
    ///
    /// 不能复用 `on_cancel()`：那会额外打印「已取消进行中的回合」，把一次正常
    /// 完成说成取消（实测 `/round` 成功后尾巴上多出这一条）。
    pub fn end_auto_turn(&mut self) {
        self.state = State::Idle;
        self.stage.clear();
        self.running_tool = None;
        self.wait_notice = None;
        // 轮次随会话结束清零（cli 2026-10-04）：`stage` 清空后 footer 已读不到它，
        // 但留着旧数字只是等着在某个未来分支里被误读。
        self.auto_round = 0;
        self.elapsed_ms += self.turn_started.elapsed().as_millis() as f64;
        self.commit_thinking();
        self.flush_answer();
    }

    /// Reset every per-turn buffer and open a fresh turn with `text` as the user
    /// bar (shared by a normal Enter submit and a queued-message dispatch).
    fn begin_turn(&mut self, text: &str) {
        self.error = None;
        self.streaming.clear();
        self.thinking.clear();
        self.thinking_stage.clear();
        self.thinking_started = Instant::now();
        self.stream_stage.clear();
        self.running_tool = None;
        self.wait_notice = None;
        // 轮次归零：`begin_turn` 是「新回合开始」的唯一入口，普通回合与自动模式收尾
        // 后重发的排队消息都走它 —— 旧轮次绝不能残留（cli 2026-10-04）。
        self.auto_round = 0;
        self.turn_tools = 0;
        self.turn_started = Instant::now();
        self.state = State::Streaming;
        self.progress = [StageProgress::default(); 3];
        self.scroll = 0; // jump back to the newest line on submit
        self.anchor = None;
        self.input_scroll = 0;
        self.push_turn(true, text);
    }

    /// Pop the front queued message and start it as a fresh turn (cli 2026-09-24
    /// ③, CC 「排队」). Returns the text for the run loop to hand to the engine,
    /// or `None` when the queue is empty. Called after every turn ends / is
    /// cancelled, so the queue drains one message per turn (CC behaviour).
    pub fn dequeue_turn(&mut self) -> Option<String> {
        if self.queue.is_empty() {
            return None;
        }
        let text = self.queue.remove(0);
        self.begin_turn(&text);
        Some(text)
    }

    /// Queue a line **without** starting it — used when Enter arrives while an
    /// automatic-mode session runs (cli 2026-10-04「插入只能是在不影响原内容的
    /// 前提下，而不是清空了继续」).
    ///
    /// Enter already routes to the queue whenever `is_busy()` (`begin_auto_turn`
    /// puts the app in `Streaming`), so this is the run loop's defensive twin of
    /// that path: it guards the case where a submit is dispatched while an
    /// `auto_task` / `round_task` is alive, where `begin_turn` would clear the
    /// live think/answer buffers out from under the running role.
    pub fn enqueue_turn(&mut self, text: String) {
        if text.trim().is_empty() {
            return;
        }
        self.queue.push(text);
    }

    /// Number of messages waiting in the queue (tests / callers).
    pub fn queued_len(&self) -> usize {
        self.queue.len()
    }

    /// Everything the user has typed but **not** submitted yet — the queued
    /// messages plus the input-box draft — for persistence across a crash
    /// (cli 2026-09-28「刚才队列里的提示词还能看到吗？进程突然被杀死了」).
    ///
    /// The draft is read straight from the input box: the only other place a
    /// draft can hide is [`Self::draft`], stashed while ↑ browses history — and
    /// that browsing is idle-only while the queue only fills mid-turn, so the two
    /// never overlap (and a snapshot never has to pick between them).
    ///
    /// This is deliberately **not** a session record: only un-submitted input is
    /// captured, because a submitted line already lives in the transcript and in
    /// `turns.user_msg`.
    pub fn unsubmitted(&self) -> SessionInput {
        SessionInput {
            queue: self.queue.clone(),
            draft: self.editor.text.clone(),
        }
    }

    /// Restore a snapshot taken by [`Self::unsubmitted`] (startup recovery). The
    /// queued bars come back in order and the draft lands in the input box; the
    /// caller announces both, because silently re-filling the box after a crash
    /// is exactly the kind of unexplained state a user cannot reason about.
    pub fn restore_unsubmitted(&mut self, snapshot: &SessionInput) {
        if !snapshot.draft.trim().is_empty() {
            self.editor.set_text(snapshot.draft.clone());
        }
        for msg in &snapshot.queue {
            self.queue.push(msg.clone());
        }
    }

    /// Abort the in-flight turn **without** the「已取消」notice — used by
    /// `ctrl+x ctrl+s` (send the queued message now), where the turn is replaced
    /// rather than cancelled.
    pub fn on_interrupt(&mut self) {
        self.state = State::Idle;
        self.stage.clear();
        self.running_tool = None;
        self.wait_notice = None;
        self.elapsed_ms += self.turn_started.elapsed().as_millis() as f64;
        self.commit_thinking();
        self.flush_answer();
    }

    /// Called by the run loop after it aborts the in-flight task (Esc).
    pub fn on_cancel(&mut self) {
        self.on_interrupt();
        self.push_info(&["· 已取消进行中的回合"]);
    }

    /// Drain command output (from the run loop) into scrollback. One call = one
    /// **block**, so it gets the one-blank-line prefix (§12.14).
    pub fn push_info(&mut self, lines: &[&str]) {
        if lines.is_empty() {
            return;
        }
        self.blank_separator();
        for l in lines {
            self.push_line(Line::from(Span::styled(
                (*l).to_string(),
                Style::default().fg(TEXT_MUTED),
            )));
        }
    }

    /// Print a memory-store statistics block (from the run loop).
    pub fn push_stats(&mut self, archive: u64, observations: u64, nodes: u64, edges: u64) {
        self.push_info(&[
            "memory (chat zone):",
            &format!("  #1 archive      {archive} turns"),
            &format!("  #2 observations {observations}"),
            &format!("  #3 knowledge    {nodes} nodes / {edges} edges"),
        ]);
    }

    /// Print a red error line into scrollback (its own block, §12.14).
    pub fn push_error(&mut self, msg: &str) {
        self.blank_separator();
        self.push_line(Line::from(Span::styled(
            format!("  ✖ {msg}"),
            Style::default().fg(ERROR),
        )));
    }

    /// Print a **multi-line** red error block into scrollback (its own block,
    /// §12.14). The first line carries the `✖` marker; continuation lines are
    /// indented under it. Used for a stage fault's diagnostic panel so its full
    /// detail is visible (cli 2026-09-27: errors must never be swallowed).
    pub fn push_error_detail(&mut self, msg: &str) {
        let lines: Vec<&str> = msg.lines().filter(|l| !l.trim().is_empty()).collect();
        if lines.is_empty() {
            return;
        }
        self.blank_separator();
        for (i, l) in lines.iter().enumerate() {
            let text = if i == 0 {
                format!("  ✖ {l}")
            } else {
                format!("    {l}")
            };
            self.push_line(Line::from(Span::styled(text, Style::default().fg(ERROR))));
        }
    }

    /// A dim notice.
    pub fn push_notice(&mut self, msg: &str) {
        self.push_info(&[&format!("· {msg}")]);
    }

    /// The **A→B handoff notice** (cli 2026-09-30): one short transcript entry
    /// saying what `组织上下文` picked and the exact shape the block takes inside
    /// `工作阶段`'s context.
    ///
    /// Everything printed here comes from [`Event::ContextHandoff`], which the
    /// engine fills from its own injection path (`CONTEXT_INJECT_PREFIX` /
    /// `CONTEXT_INJECT_ROLE` / `CONTEXT_INJECT_POSITION` and the stage's parsed
    /// `load…` JSON) — the UI never restates the contract from memory, so the
    /// text cannot drift from what actually went on the wire.
    ///
    /// 2026-10-05 B 项: the block is injected into the **system** prompt (原版
    /// `context.py:335`), so the format line now reads 「以「## 上下文」前缀作为
    /// system 提示注入，位于 工作阶段 system 尾部」 — the previous user-message
    /// wording described the dual-injection that was removed.
    ///
    /// cli 2026-09-30 (「A阶段结束后显示那段话用白色」): both lines draw the
    /// **primary foreground** ([`TEXT`], white) rather than the muted chrome grey
    /// ([`TEXT_MUTED`], #AAAAAA) the first cut used. The notice marks the one
    /// moment the turn changes hands between stages, so it is *read*, not
    /// skimmed — the same reason the thinking fold's **title** is white while its
    /// body stays grey (see [`thinking_lines`]).
    fn push_handoff(&mut self, n: &HandoffNotice) {
        let mut picked: Vec<String> = Vec::new();
        if n.observations > 0 {
            picked.push(format!("{} 条观测", n.observations));
        }
        if n.nodes > 0 {
            picked.push(format!("{} 个知识节点", n.nodes));
        }
        if n.recent_turns > 0 {
            picked.push(format!("{} 轮对话", n.recent_turns));
        }
        // The stage's own reasoning streams in live (`∴` lines); the notice
        // belongs *below* it, so flush the in-flight block first — otherwise the
        // still-open buffer would render after the committed notice.
        self.commit_thinking();
        self.blank_separator();
        let head = if n.chars == 0 && picked.is_empty() {
            // Nothing was selected: say so plainly rather than claim a handoff of
            // an empty block (the injection is skipped in that case).
            format!("{HANDOFF_MARK} {} › {} · 未选中上下文", n.from, n.to)
        } else {
            let what = if picked.is_empty() {
                "已选中上下文".to_string()
            } else {
                format!("已选中 {}", picked.join(" + "))
            };
            format!(
                "{HANDOFF_MARK} {} › {} · {what} · 共 {} 字",
                n.from, n.to, n.chars
            )
        };
        // cli 2026-09-30 (「A阶段结束后显示那段话用白色」): the handoff notice is
        // drawn in the **primary** foreground ([`TEXT`] = white), not chrome grey
        // — it is the one line that says "组织上下文 handed X chars to 工作阶段",
        // and cli wants it read, not skimmed. The `▸` / `▹` marks ride the same
        // ink; the thinking fold already follows this rule for its **title**.
        self.push_line(Line::from(Span::styled(head, Style::default().fg(TEXT))));
        // The **format** half: how it enters 工作阶段's context, in the receiver's
        // own terms (prefix / role / position all come off the event).
        if n.chars > 0 {
            self.push_line(Line::from(Span::styled(
                format!(
                    "  {HANDOFF_FORMAT_MARK} 以「{}」前缀作为 {} 提示注入，位于 {}",
                    n.prefix, n.role, n.position
                ),
                Style::default().fg(TEXT),
            )));
        }
    }

    /// Show the transient `已复制 N 字符` confirmation on the footer row (the run
    /// loop calls this after a successful clipboard write). Kept as a setter so
    /// the copy click path lives with the clipboard in `lib.rs` while the state
    /// stays in `App`.
    pub fn set_copy_note(&mut self, msg: String) {
        self.copy_note = Some((msg, Instant::now()));
    }

    /// Flush the live reasoning buffer into the transcript as a thinking block
    /// (§12.7).
    ///
    /// Called before the answer, before a tool card, and at turn end — so the
    /// block always sits *above* whatever follows it, exactly like CC. The block
    /// is stored *structured* (not pre-rendered) so `ctrl+o` can toggle it later:
    /// collapsed = one grey summary line, expanded = the full reasoning.
    fn commit_thinking(&mut self) {
        let text = std::mem::take(&mut self.thinking).trim().to_string();
        if text.is_empty() {
            return;
        }
        let stage = self.thinking_stage.clone();
        let secs = self.thinking_started.elapsed().as_secs_f64();
        self.blank_separator();
        self.scrollback.push(Item::Thinking { stage, text, secs });
    }

    /// CC-style tool card: `● 中文名(主参数)` + `⎿ result`.
    ///
    /// The dot is **green** on success ([`SUCCESS`]) and red ([`ERROR`]) only on
    /// failure — cli 2026-09-24 实测 CC 2.1.278 的工具卡圆圈本就是纯绿
    /// RGB(0,205,0)（page41 :99 同屏取色），故修回上一轮 cdb04e6 把它收成白色
    /// （[`TEXT`]）的过头：CC 的「克制」是**每种控件各有语义色**，而非通盘单色。
    /// 工具名加粗、主参数用正文色。结果 `⎿` 用**正文亮色**（[`TEXT`]）—— CC 的工具
    /// 结果（如 Bash 的 stdout 块）本就是最亮的前景色，是被读取的**主体数据**；
    /// fe14e4b 把它和「思考」（真·次要灰）一起压暗，反而抹平了层次（§12.15）。
    fn push_tool_card(
        &mut self,
        stage: &str,
        tool: &str,
        args: &serde_json::Value,
        preview: &str,
        outcome: &ToolOutcome,
    ) {
        let (ms, error) = (outcome.ms, outcome.error);
        let diff_lines: &[DiffLine] = &outcome.diff;
        let dot = if error { ERROR } else { SUCCESS };
        let mut header = vec![Span::styled(
            "● ",
            Style::default().fg(dot).add_modifier(Modifier::BOLD),
        )];
        // cli 2026-09-24 ⑦: every item carries its stage tag (low-saturation).
        if let Some(tag) = stage_tag_span(stage) {
            header.push(tag);
        }
        // cli 2026-09-24 ⑥: the tool name is shown in **English** (its real API
        // name), like CC's `Read` / `Bash` — not the old Chinese label.
        header.push(Span::styled(
            tool.to_string(),
            Style::default().add_modifier(Modifier::BOLD),
        ));
        // cli 2026-09-24 (「如果没有参数也要有()，体现出函数调用」): always render
        // the call parens — a bare `()` reads as a *function call*
        // (`memory_kinds()`) rather than a plain label.
        let arg = tool_primary_arg(tool, args);
        header.push(Span::styled(format!("({arg})"), Style::default().fg(TEXT)));
        // cli 2026-09-28 (「参照 CC 实现代码改动时的红绿对比格式」): a file mutation's
        // header carries CC's `+N -M` diff stat — the same figures CC prints beside
        // an `Edit` / `Write`, in its own `+`/`-` colours.
        let stat = diff_stat(diff_lines);
        if !stat.is_empty() {
            // A space separates the call parens from the stat (`edit(f.rs) +1 -1`),
            // matching CC's `Edit(file) +1 -1` header shape.
            header.push(Span::styled(" ", Style::default().fg(TEXT_MUTED)));
            for (i, part) in stat.split(' ').enumerate() {
                if i > 0 {
                    header.push(Span::styled(" ", Style::default().fg(TEXT_MUTED)));
                }
                let color = if part.starts_with('+') {
                    DIFF_ADD
                } else {
                    DIFF_REMOVE
                };
                header.push(Span::styled(part.to_string(), Style::default().fg(color)));
            }
        }
        header.push(Span::styled(
            format!("  {}", fmt_ms(ms)),
            Style::default().fg(TEXT_MUTED),
        ));

        // cli 2026-09-24 (「工具调用返回值显示为比思考稍微亮一点点的灰色」): the
        // result body is a *middle* grey ([`TOOL_RESULT`], ANSI 37) — one notch
        // brighter than the reasoning (`TEXT_MUTED`, ANSI 90) yet calmer than the
        // primary answer (`TEXT`). §12.15 had made it the brightest foreground;
        // cli pulled it back so a raw tool dump reads as data, not as the reply.
        // Failures stay red.
        let color = if error { ERROR } else { TOOL_RESULT };
        let wrap = (self.width as usize).saturating_sub(6).max(8);
        let mut previews = wrap_cjk(preview.trim(), wrap);
        if previews.is_empty() {
            previews.push(String::new());
        }
        let mut body = Vec::with_capacity(previews.len());
        for (i, seg) in previews.into_iter().enumerate() {
            let lead = if i == 0 { "  ⎿ " } else { "    " };
            body.push(Line::from(Span::styled(
                format!("{lead}{seg}"),
                Style::default().fg(color),
            )));
        }
        // The red/green patch (cli 2026-09-28「代码改动的红绿对比」): one `+`/`-`/
        // `context` line per changed line, wrapped by the same CJK-aware ruler and
        // coloured from the `theme::DIFF_*` tokens so the widget layer still writes
        // no raw colour. A `@@` hunk header renders dim, like git/CC.
        let diff: Vec<Line> = diff_lines
            .iter()
            .flat_map(|l| {
                let marker = l.marker();
                let text = format!("  {marker}{}", l.text);
                let style = Style::default().fg(l.color());
                wrap_cjk(&text, wrap)
                    .into_iter()
                    .enumerate()
                    .map(move |(i, seg)| {
                        // Continuation rows of a wrapped diff line indent under the
                        // marker so the column stays readable.
                        if i == 0 {
                            Line::from(Span::styled(seg, style))
                        } else {
                            Line::from(Span::styled(format!("   {seg}"), style))
                        }
                    })
            })
            .collect();
        // Store the card *structured* (§12.10) so a long result folds to its
        // first `N` lines + a `ctrl+o` hint, and the same `ctrl+o` reveals the
        // rest — without losing the transcript order or re-wrapping the CJK.
        self.blank_separator();
        self.scrollback.push(Item::ToolOutput {
            header: Line::from(header),
            body,
            diff,
        });
    }

    /// Append the user's prompt as a CC-style **reverse-video bar**.
    ///
    /// CC paints each user turn as an inverse bar (the whole line inverted); we
    /// do the same — padded to the pane width so it reads as a solid band, with
    /// the `❯` prompt kept in the accent colour.
    ///
    /// (`user == false` falls through to [`Self::push_answer`], kept for
    /// call-site symmetry with the tests.)
    fn push_turn(&mut self, user: bool, text: &str) {
        if user {
            let lines = user_bar_lines(text, (self.width as usize).saturating_sub(2));
            if lines.is_empty() {
                return;
            }
            // Keep the bar as its own item; it opens a *block*, so it gets the
            // one-blank-line prefix like every other block (§12.14).
            self.blank_separator();
            self.scrollback.push(Item::UserTurn { lines });
            return;
        }
        self.push_answer(text);
    }

    /// Commit the live answer buffer (if any) as a `●` reply block, leaving it
    /// empty. Called at every assistant-message boundary (tool start / turn end
    /// / cancel) so successive tool-loop iterations never concatenate.
    fn flush_answer(&mut self) {
        let text = std::mem::take(&mut self.streaming);
        let text = text.trim().to_string();
        if !text.is_empty() {
            self.push_answer(&text);
        }
    }

    /// Append an assistant reply: a `● ` bullet on the first line, the remaining
    /// lines rendered as markdown (T12 富文本 — bold / italic / headings / lists
    /// / inline & fenced code / tables, via `tui-markdown`).
    fn push_answer(&mut self, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        let mut md = markdown_lines(text);
        if let Some(first) = md.first_mut() {
            let taken = std::mem::take(first);
            let mut spans = vec![Span::styled(
                "● ",
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            )];
            // cli 2026-09-24 ⑦: the reply carries its stage tag (low-saturation).
            if let Some(tag) = stage_tag_span(&self.stream_stage) {
                spans.push(tag);
            }
            spans.extend(taken.spans);
            *first = Line::from(spans);
        }
        // The reply is its own block → one blank line before it (§12.14).
        self.blank_separator();
        for l in md {
            self.push_line(l);
        }
        // No trailing blank here: the *next* block adds its own separator.
    }

    /// CC-style turn tail (§14-P2): `◜ {动词} for Ns · done HH:MM:SS` (dim). The
    /// verb is the one the wheel landed on at completion, and the stamp is local
    /// wall-clock — matching CC's shape (`{mark} Cooked for 4s · done 18:39`).
    ///
    /// cli 2026-09-24 (「先按 CC 的样式做」): the `· N 个工具` chatter is gone (CC
    /// prints none); the count is still tracked in [`Self::turn_tools`] in case a
    /// 灵妙-specific touch is re-decided later.
    ///
    /// cli 2026-09-28 (「去掉里边 claude 那几个花/星星的 emoji」): the leading mark
    /// is [`TAIL_MARK`] (`◜`) — the activity arc's home frame, whose font coverage
    /// and 1-column advance are verified (see [`crate::theme::ACTIVITY_SPINNER`]) —
    /// instead of the old `✻`, which the terminal had to substitute from a
    /// proportional fallback font.
    fn push_tail(&mut self, secs: f64) {
        let verb = activity_verb((secs * 1000.0) as u64);
        self.blank_separator();
        // cli 2026-09-24 (「活动行要跟屏幕最左侧对齐，不要跟二级缩进对齐」): the
        // tail starts flush at the left margin like CC — **no** leading indent (it
        // used to carry a 2-space lead that lined it up with the indented
        // reasoning body).
        self.push_line(Line::from(Span::styled(
            format!("{TAIL_MARK} {verb} for {secs:.1}s · done {}", now_hms()),
            Style::default().fg(TEXT_MUTED),
        )));
    }

    // ── rendering ─────────────────────────────────────────────────

    /// Per-stage real timing for `/session`: `检索记忆 0.3s · 正在作答 1.2s · …`
    /// (§7 本轮处理 —— the sidebar block became a command).
    fn stage_timings(&self) -> String {
        PROGRESS_STAGES
            .iter()
            .zip(self.progress.iter())
            .map(|(label, p)| match p.status {
                StageStatus::Pending => format!("{label} —"),
                _ => format!("{label} {:.1}s", p.elapsed_ms / 1000.0),
            })
            .collect::<Vec<_>>()
            .join(" · ")
    }

    /// The **status label** for the activity line (cli 2026-09-24「活动行那里放
    /// 状态」): the current pipeline stage in plain language — this is what the
    /// footer's status row used to show before it was removed.
    fn activity_status(&self) -> String {
        if self.stage.is_empty() {
            "作答中".to_string()
        } else if is_auto_stage(&self.stage) {
            // 自动模式（cli 2026-10-04「自动模式要显示角色和阶段」）: the activity
            // line names **the role** (`Auto·Main` / `Auto·Auditor`) instead of the
            // plain verb, so a glance at the busy line tells Main's work from the
            // Auditor's — the whole point of running two roles.
            self.stage.clone()
        } else {
            human_stage(&self.stage).to_string()
        }
    }

    /// Draw the full single-column frame (§12.1): conversation+input / 3-line
    /// footer.
    pub fn render(&mut self, frame: &mut Frame) {
        let area = frame.area();
        self.width = area.width;
        self.height = area.height;
        // Expire a stale `Ctrl+C` arm here (the run loop repaints ~10×/s even
        // when no key arrives), so the `再按一次 ⌃C 退出` hint always goes away on
        // its own — the deadline ([`EXIT_ARM_SECS`]) is enforced on a timer, not
        // only on the next key press.
        self.expire_exit_arm();
        // CC has **no** persistent header (cli 2026-09-24: 「header 部分是 CC 没有
        // 的，它只有启动的时候在对话历史里打印」): the brand identity is printed
        // once, at launch, into the conversation (see
        // [`Self::push_startup_banner`]) and kept ever-present at the footer's
        // **bottom-right** (see [`Self::render_footer`]). The frame is therefore
        // simply the conversation over the input box, over the footer — no header
        // band, no hairline rule.
        // The footer is a single identity line now (cli 2026-09-24: the status row
        // was dropped and the context breakdown moved above the input box).
        let footer_h = 1;
        let [body, footer] =
            Layout::vertical([Constraint::Min(3), Constraint::Length(footer_h)]).areas(area);
        self.render_body(frame, body);
        self.render_footer(frame, footer);
    }

    /// Print the CC-style brand block **once**, at startup, into the conversation
    /// history (cli 2026-09-24: 「header 部分是 CC 没有的，它只有启动的时候在对话
    /// 历史里打印，咱们也保持一致」).
    ///
    /// CC paints a logo sprite beside a two-line identity block
    /// (`Claude Code vX` / `model · billing` / `cwd`) at launch and then lets it
    /// scroll away; 灵妙 does the same instead of reserving a permanent top band.
    /// The identity stays reachable at the footer's bottom-right — see
    /// [`Self::render_footer`]. cli 2026-09-28 (「把 logo 的颜色改成跟输入框提示
    /// 这里一样的颜色」): the mark wears the same grey as the input-box
    /// placeholder ([`TEXT_MUTED`], via [`LOGO`]) and the name stays plain
    /// [`TEXT`] — so the banner is chrome, not a coloured brand splash.
    pub fn push_startup_banner(&mut self) {
        let mark = Style::default().fg(LOGO);
        let sep = Span::styled(" · ", Style::default().fg(TEXT_MUTED));
        self.push_line(Line::from(vec![
            Span::styled(" ▟▀▙  ", mark),
            Span::styled(
                format!("{} {}", brand::NAME, brand::SLUG),
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  v{}", self.version),
                Style::default().fg(TEXT_MUTED),
            ),
        ]));
        self.push_line(Line::from(vec![
            Span::styled(" ▜▄▛  ", mark),
            Span::styled(self.model.clone(), Style::default().fg(TEXT)),
            sep,
            Span::styled(self.cwd.clone(), Style::default().fg(TEXT_MUTED)),
        ]));
    }

    /// Main band: the CC conversation pane over the bordered input box (§12.1,
    /// 对话区无边框 —— the prompt itself is framed, like CC).
    fn render_body(&mut self, frame: &mut Frame, area: Rect) {
        // The input box grows with the number of *visual* (wrapped) editor rows,
        // not the raw newline count, so a long line that wraps still gets room
        // (cli 2026-09-27「多行文本框不折行」). Content width = area width − 2
        // border − 2 prompt indent. Clamped to a sane band so it can never eat
        // the conversation pane.
        let content_w = (area.width as usize).saturating_sub(4).max(1);
        let visual_rows = self.editor_visual_line_count(content_w);
        let input_h = (visual_rows as u16 + 2).clamp(3, MAX_INPUT_HEIGHT);
        // cli 2026-09-24 (「绿色的上下文条不要了…放到对话栏右下角原来显示上下文
        // 窗口的位置…控制在20字符内」): the green full-width composition band is
        // gone. A short, muted, **right-aligned** total now sits just above the
        // input box — the same spot CC prints `N tokens` (its pane's
        // bottom-right) — collapsing before the first injection.
        let ctx_h = self.ctx_line_height();
        let [conv, ctx, input] = Layout::vertical([
            Constraint::Min(3),
            Constraint::Length(ctx_h),
            Constraint::Length(input_h),
        ])
        .areas(area);
        self.render_conversation(frame, conv);
        self.render_ctx_line(frame, ctx);
        // §12.2 ④: the slash-command palette floats just above the input box.
        self.render_completion(frame, area, input);
        // The reverse-history-search overlay (ctrl+r) shares that slot.
        self.render_search(frame, area, input);
        self.render_input(frame, input);
    }

    /// Height of the **context line** above the input box (cli 2026-09-24): one
    /// row once there is anything to show, `0` before the first injection — so it
    /// collapses and the conversation keeps the row.
    fn ctx_line_height(&self) -> u16 {
        if self.ctx_tokens == 0
            && self.ctx_sections.is_empty()
            && self.whiteboard_summary().is_empty()
        {
            0
        } else {
            1
        }
    }

    /// The whiteboard's current page as a **single line** for the ctx row's left
    /// slot (cli 2026-09-28: 「小白板显示在与上下文数字同一行，左对齐，留一点
    /// gap，不要完全顶着上下文那里」).
    ///
    /// `self.whiteboard` is the page as read from disk: line 0 is the
    /// `[N] 标题` head, the rest are content lines. The summary is that head plus,
    /// when present, the page's **first non-empty content line** — the
    /// "what am I on about" glance the ctx row now carries. Empty when the
    /// whiteboard has no content (never the placeholder text — a blank left slot
    /// is quieter than a fake note), so the row can collapse before the first
    /// turn when there is also no token figure.
    fn whiteboard_summary(&self) -> String {
        let head = self.whiteboard.first().map(String::as_str).unwrap_or("");
        if head.is_empty() || head == WHITEBOARD_EMPTY {
            return String::new();
        }
        let mut out = head.to_string();
        if let Some(content) = self
            .whiteboard
            .iter()
            .skip(1)
            .map(|l| l.trim())
            .find(|l| !l.is_empty() && !Self::whiteboard_line_is_echo(l, head))
        {
            out.push_str(" · ");
            out.push_str(content);
        }
        out
    }

    /// Is this whiteboard content line only an **echo** of the page head?
    ///
    /// cli 2026-09-28 (「原来的小白板内容太长了显示不出来」): the ctx row carries the
    /// page as a **single line**, so a body line that merely repeats the head
    /// wastes the whole slot. Two shapes turn up in the real pages — a
    /// `--- Page N: <title> ---` divider the agent copied back from a
    /// `whiteboard_read` result, and a line equal to the title — and neither
    /// tells the user anything new, so the summary skips to the next real line
    /// (see [`Self::whiteboard_summary`]).
    fn whiteboard_line_is_echo(line: &str, head: &str) -> bool {
        if let Some(inner) = line.strip_prefix("---").and_then(|l| l.strip_suffix("---")) {
            let inner = inner.trim().trim_start_matches("Page ").trim();
            let echoed = inner.split_once(':').map_or(inner, |(_, t)| t).trim();
            return echoed.is_empty() || Self::whiteboard_echo_matches(echoed, head);
        }
        Self::whiteboard_echo_matches(line, head)
    }

    /// Does `line` repeat the page head (`[N] 标题`), ignoring the `[N]` prefix
    /// and any `（）` decoration the reader adds?
    fn whiteboard_echo_matches(line: &str, head: &str) -> bool {
        fn bare(s: &str) -> String {
            let s = s.trim();
            let s = match s.strip_prefix('[').and_then(|r| r.split_once(']')) {
                Some((_, rest)) => rest.trim(),
                None => s,
            };
            s.trim_matches(|c: char| "()（）".contains(c)).to_string()
        }
        let (a, b) = (bare(line), bare(head));
        !a.is_empty() && a == b
    }

    /// Columns the ctx row's **left** slot may use, leaving
    /// [`CTX_LINE_GAP`] blank columns before the right-aligned token number so
    /// the note never butts up against it (cli 2026-09-28「留一点gap，不要完全
    /// 顶着上下文那里」). Pure, so the gutter is unit-testable.
    fn ctx_left_budget(pad_width: u16, number_width: usize) -> usize {
        (pad_width as usize).saturating_sub(number_width + CTX_LINE_GAP)
    }

    /// Draw the **ctx row** above the input box: the whiteboard's current page
    /// left-aligned, and the short context total right-aligned (cli 2026-09-24:
    /// 「…控制在20字符内…放到对话栏右下角」 — the spot CC prints `N tokens`).
    ///
    /// cli 2026-09-28: the previously empty left half now carries the whiteboard
    /// summary, and it is truncated to [`Self::ctx_left_budget`] so a
    /// [`CTX_LINE_GAP`]-column gutter always separates it from the number. Both
    /// slots are muted grey; the number reports the provider-measured token count
    /// and nothing more (no window, no threshold — §8.4 keeps colour a *signal*).
    fn render_ctx_line(&self, frame: &mut Frame, area: Rect) {
        if area.height == 0 {
            return;
        }
        let text = self.ctx_summary();
        let wb = self.whiteboard_summary();
        if text.is_empty() && wb.is_empty() {
            return;
        }
        let pad = Rect {
            x: area.x + 1,
            y: area.y,
            width: area.width.saturating_sub(2),
            height: 1,
        };
        // Left slot: the whiteboard note, truncated to leave the gutter.
        let budget = Self::ctx_left_budget(pad.width, display_width(&text));
        if !wb.is_empty() && budget >= 4 {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    truncate(&wb, budget),
                    Style::default().fg(TEXT_MUTED),
                )))
                .alignment(Alignment::Left),
                pad,
            );
        }
        // Right slot: the token figure (unchanged semantics — §30).
        if !text.is_empty() {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    text,
                    Style::default().fg(self.ctx_line_color()),
                )))
                .alignment(Alignment::Right),
                pad,
            );
        }
    }

    /// The slash-command completion palette (§12.2 ④): a small rounded list of
    /// the commands matching the buffer, floated just above the input box. The
    /// highlighted row is the one ↑/↓ moved to, which `Tab` (or `Enter`) accepts.
    ///
    /// Pure presentation over [`Self::completion_matches`] — the palette's *open*
    /// state is the buffer's, so it can never lag what the user typed. It never
    /// overlaps the input box; when the band above it is too short to hold a
    /// framed box it is simply not drawn (the keys still work).
    fn render_completion(&self, frame: &mut Frame, body: Rect, input: Rect) {
        let matches = self.completion_matches();
        if matches.is_empty() {
            return;
        }
        // Rows available above the input box, within the body band.
        let avail = input.y.saturating_sub(body.y);
        if avail < 3 {
            return;
        }
        let h = (matches.len() as u16 + 2).min(avail);
        let w = input.width.min(48);
        let rect = Rect {
            x: input.x,
            y: input.y - h,
            width: w,
            height: h,
        };
        // Clear so the palette reads as a floating panel over the transcript.
        frame.render_widget(Clear, rect);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(BORDER));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        if inner.height == 0 || inner.width == 0 {
            return;
        }
        let sel = self.menu_index(matches.len());
        // Scroll the window so the selected row stays visible on a short pane.
        let visible = inner.height as usize;
        let first = if sel >= visible { sel + 1 - visible } else { 0 };
        let mut lines: Vec<Line> = Vec::new();
        for (row, &ci) in matches.iter().enumerate().skip(first).take(visible) {
            let (name, desc) = COMMANDS[ci];
            if row == sel {
                let style = Style::default()
                    .fg(SELECTION_FG)
                    .bg(SELECTION_BG)
                    .add_modifier(Modifier::BOLD);
                let mut s = format!("/{name:<9} {desc}");
                let used = display_width(&s) as u16;
                if used < inner.width {
                    s.push_str(&" ".repeat((inner.width - used) as usize));
                }
                lines.push(Line::from(Span::styled(s, style)));
            } else {
                lines.push(Line::from(vec![
                    Span::styled(format!("/{name:<9}"), Style::default().fg(ACCENT)),
                    Span::styled(format!(" {desc}"), Style::default().fg(TEXT_MUTED)),
                ]));
            }
        }
        frame.render_widget(Paragraph::new(lines), inner);
    }

    /// The reverse **history search** overlay (cli 2026-09-27, CC `ctrl+r`): a
    /// floating rounded panel just above the input box, the typed filter on its
    /// head line and the matching submitted prompts below (newest first), the
    /// highlighted row painted inverse. Reuses the completion palette's slot and
    /// framing so the two overlays read as the same family.
    fn render_search(&self, frame: &mut Frame, body: Rect, input: Rect) {
        if !self.search_open {
            return;
        }
        let matches = self.search_matches();
        let avail = input.y.saturating_sub(body.y);
        if avail < 3 {
            return;
        }
        // One filter line + up to N match rows + 2 border rows.
        let h = (matches.len() as u16 + 3).min(avail).max(3);
        let w = input.width.min(64);
        let rect = Rect {
            x: input.x,
            y: input.y - h,
            width: w,
            height: h,
        };
        frame.render_widget(Clear, rect);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(BORDER));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        if inner.height == 0 || inner.width == 0 {
            return;
        }
        let mut lines: Vec<Line> = Vec::new();
        // Head: the live filter, then a hint.
        lines.push(Line::from(vec![
            Span::styled(
                "❯ ",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(self.search_query.clone(), Style::default().fg(TEXT)),
            Span::styled(
                "  搜索历史 · ↑↓ 选择 · Enter 填入 · Esc 取消",
                Style::default().fg(TEXT_MUTED),
            ),
        ]));
        if matches.is_empty() {
            lines.push(Line::from(Span::styled(
                "  （无匹配）",
                Style::default().fg(TEXT_MUTED),
            )));
        } else {
            let visible = (inner.height as usize).saturating_sub(1);
            let sel = self.search_selected.min(matches.len() - 1);
            let first = if sel >= visible { sel + 1 - visible } else { 0 };
            for (row, &i) in matches.iter().enumerate().skip(first).take(visible) {
                // One screen line per entry: fold embedded newlines.
                let one = self.history[i].replace('\n', "⏎");
                if row == sel {
                    let style = Style::default()
                        .fg(SELECTION_FG)
                        .bg(SELECTION_BG)
                        .add_modifier(Modifier::BOLD);
                    let mut s = truncate(&one, inner.width as usize);
                    let used = display_width(&s) as u16;
                    if used < inner.width {
                        s.push_str(&" ".repeat((inner.width - used) as usize));
                    }
                    lines.push(Line::from(Span::styled(s, style)));
                } else {
                    lines.push(Line::from(Span::styled(
                        truncate(&one, inner.width as usize),
                        Style::default().fg(TEXT),
                    )));
                }
            }
        }
        frame.render_widget(Paragraph::new(lines), inner);
    }

    /// The CC-style conversation pane — no border in the single-column layout
    /// (§12.1).
    fn render_conversation(&mut self, frame: &mut Frame, area: Rect) {
        // 1-column left inset for breathing room (no border to inset against).
        let pad = Rect {
            x: area.x + 1,
            y: area.y,
            width: area.width.saturating_sub(2),
            height: area.height,
        };
        let inner_w = (pad.width as usize).max(8);
        // §12.9: pre-wrap every line ourselves (CJK 禁则 + hanging list indent)
        // before paging, then render without ratatui's character-granular wrap so
        // it cannot re-break — and undo — what we did.
        //
        // Performance (cli 2026-09-27「翻页很卡」): the **committed** transcript is
        // flattened + wrapped **once** and cached (see
        // [`Self::ensure_transcript_cache`]); only the in-flight blocks below are
        // rebuilt on each frame, and only the visible window is cloned. The old
        // code re-flattened *and* re-wrapped the whole history every frame — a
        // folded thinking block wrapped its entire reasoning just to show the last
        // six rows — which cost ~500 ms/frame (and ~450 ms per wheel notch) once a
        // session carried ~1 MB of committed reasoning.
        // One width for **both** the committed cache and the live blocks: the live
        // blocks use `inner_w` (which floors at 8 so the card header never wraps to
        // a character a line), and a mismatch would make the committed and
        // in-flight halves of the same card disagree on wrapping — exactly the
        //「一条一个词、竖着排」garble seen on a very narrow pane (cli 2026-09-28 ②).
        let wrap_width = inner_w;
        self.ensure_transcript_cache(wrap_width);
        let committed_len = self.transcript_cache.len();
        let mut tail: Vec<Line> = Vec::new();
        // Whether anything (committed or already-built live block) precedes the
        // next block — drives the one-blank-line separator (§12.14).
        let mut any = committed_len > 0;
        // Live reasoning (the in-flight segment, not yet committed) — folded the
        // same way as committed blocks (§12.7). Each live block gets the same
        // one-blank-line separator a committed block would (§12.14), so the
        // in-flight transcript reads identically to the settled one.
        if !self.thinking.trim().is_empty() {
            if any {
                tail.push(Line::from(""));
            }
            tail.extend(thinking_lines(
                &self.thinking_stage,
                &self.thinking,
                self.thinking_started.elapsed().as_secs_f64(),
                self.thinking_expanded,
                inner_w,
            ));
            any = true;
        }
        // Live running tool card (`⠋ name(args) · Ns`).
        if let Some(rt) = &self.running_tool {
            if any {
                tail.push(Line::from(""));
            }
            any = true;
            let spin = SPINNER[(rt.started.elapsed().as_millis() / 100) as usize % SPINNER.len()];
            let arg = tool_primary_arg(&rt.tool, &rt.args);
            let mut spans = vec![Span::styled(
                format!("{spin} "),
                Style::default().fg(RUNNING).add_modifier(Modifier::BOLD),
            )];
            // cli 2026-09-24 ⑦: the running card carries the stage tag too.
            if let Some(tag) = stage_tag_span(&rt.stage) {
                spans.push(tag);
            }
            // cli 2026-09-24 ⑥: English tool name (its real API name), CC-style —
            // not the old Chinese label.
            spans.push(Span::styled(
                rt.tool.clone(),
                Style::default().add_modifier(Modifier::BOLD),
            ));
            if !arg.is_empty() {
                spans.push(Span::styled(format!("({arg})"), Style::default().fg(TEXT)));
            }
            spans.push(Span::styled(
                format!("  {:.0}s", rt.started.elapsed().as_secs_f64()),
                Style::default().fg(TEXT_MUTED),
            ));
            tail.push(Line::from(spans));
        }
        // Live **wait heartbeat** (cli 2026-10-05「要进界面的，这是核心体验」) —
        // one line, rewritten in place each 5s sample, so a silent command
        // visibly ticks instead of the pane sitting still for a minute.
        if let Some(line) = self.wait_notice_line() {
            if any {
                tail.push(Line::from(""));
            }
            any = true;
            tail.push(line);
        }
        // Live answer (`● …`) — rendered as markdown so the reply looks the same
        // while it streams as it does once committed (T12).
        if !self.streaming.trim().is_empty() {
            if any {
                tail.push(Line::from(""));
            }
            any = true;
            for (i, line) in markdown_lines(self.streaming.trim())
                .into_iter()
                .enumerate()
            {
                if i == 0 {
                    let mut spans = vec![Span::styled(
                        "● ",
                        Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
                    )];
                    // cli 2026-09-24 ⑦ (「正文输出都要有阶段标志」): the *live*
                    // reply carries the same stage tag its committed form does.
                    // This path was left tag-less when §12.13 stripped the badges,
                    // but ⑦ reversed that — keep the two render paths in step.
                    if let Some(tag) = stage_tag_span(&self.stream_stage) {
                        spans.push(tag);
                    }
                    spans.extend(line.spans);
                    tail.push(Line::from(spans));
                } else {
                    tail.push(line);
                }
            }
        }
        // Pending queued messages (cli 2026-09-24 ③, CC 「排队」): each shows as
        // the same reverse-video `❯` bar a submitted turn gets, with the newest
        // carrying the `ctrl+x ctrl+s 立即发送` hint — so a mid-turn message is
        // visibly *waiting* rather than silently buffered.
        for (i, msg) in self.queue.iter().enumerate() {
            if any {
                tail.push(Line::from(""));
            }
            any = true;
            tail.extend(user_bar_lines(msg, inner_w));
            if i + 1 == self.queue.len() {
                // 自动模式在跑时「立即发送」会被拦下（两路流不能并发写同一份
                // 缓冲，cli 2026-10-04），所以提示如实改成「结束后发送」——UI 文案
                // 必须与真实行为一致，不能让用户按了一个无效的快捷键。
                let hint = if is_auto_stage(&self.stage) {
                    "  自动模式进行中 · 结束后发送"
                } else {
                    "  ctrl+x ctrl+s 立即发送"
                };
                tail.push(Line::from(Span::styled(
                    hint,
                    Style::default().fg(TEXT_MUTED),
                )));
            }
        }
        // T2 活动行 (§12.2 ② + §14-P2): while a turn is in flight and no tool
        // card is on screen, show a CC-style spinner verb + elapsed + the live
        // 3-stage pipeline so the user always sees that work is *moving*.
        //
        // page40 (cli 2026-09-24「活动行 青→灰」): the line is **grey**
        // ([`TEXT_MUTED`]) like CC's `◜ Baked for 3s` tail — the old cyan was
        // part of the 「强调色滥用」. (The healthy-work signal proper — the footer
        // status line + the running-tool spinner — still uses [`RUNNING`], per the
        // §14-P2 semantic split; this transcript line deliberately stays quiet.)
        if self.is_busy() && self.running_tool.is_none() {
            if any {
                tail.push(Line::from(""));
            }
            any = true;
            let elapsed_ms = self.turn_started.elapsed().as_millis() as u64;
            let glyph = cc_spinner(elapsed_ms);
            let verb = activity_verb(elapsed_ms);
            // cli 2026-09-28 (「橙色动态行不太稳定…去掉里边 claude 那几个花/星星的
            // emoji」): the mark is a rotating **arc** ([`ACTIVITY_SPINNER`]) — every
            // frame is 1 column wide and present in both monospace fonts installed
            // here, so a burst of screenshots shows the glyph step *cleanly* (the
            // CC asterisk family had to be substituted from a proportional fallback
            // font and collapsed into near-identical stars at varying advances).
            // The verb still steps (cli 2026-09-24 ②「切换词语」), and the line
            // stays CC's brand **orange** ([`ACCENT`]).
            let status = self.activity_status();
            // 自动模式带**当前轮次**（cli 2026-10-04「自动模式需要显示轮次」），与
            // footer 左槽一致。
            let status = if is_auto_stage(&self.stage) && self.auto_round > 0 {
                format!("{status} · 第{}轮", self.auto_round)
            } else {
                status
            };
            let left = format!(
                "{glyph} {verb}… {status} {:.0}s · esc 中断",
                elapsed_ms as f64 / 1000.0
            );
            tail.push(Line::from(Span::styled(left, Style::default().fg(ACCENT))));
        }
        if !any {
            tail.push(Line::from(Span::styled(
                "  问点什么吧 —— 输入后按 Enter；/ 查看命令。",
                Style::default().fg(TEXT_MUTED),
            )));
        }
        // The live tail is wrapped with the same rules (and width) the committed
        // cache was built with, so the two concatenate seamlessly.
        let tail: Vec<Line> = tail
            .into_iter()
            .flat_map(|l| wrap_line(&l, wrap_width))
            .collect();
        let total = committed_len + tail.len();
        let visible = pad.height as usize;
        // §11 paging: `scroll` is lines above the bottom; clamp and remember the
        // bound so `PageUp`/`Home` can't overshoot.
        let max_scroll = total.saturating_sub(visible);
        self.max_scroll = max_scroll.min(u16::MAX as usize) as u16;
        // The anchor pins the viewport top while the user has scrolled up, so
        // freshly-streamed lines appended below do not push the view down (cli
        // 2026-09-27「滚轮翻动后位置保持」). `None` = follow the newest line.
        let start = match self.anchor {
            Some(a) => (a as usize).min(max_scroll),
            None => max_scroll,
        };
        // The transcript may have shrunk (e.g. `/clear`) — clamp the anchor too.
        self.anchor = self.anchor.map(|a| (a as usize).min(max_scroll) as u16);
        let end = (start + visible).min(total);
        // `scroll` is the *actual* lines-above-bottom, kept for the indicator.
        self.scroll = total.saturating_sub(end).min(u16::MAX as usize) as u16;
        // Only the visible window is cloned — everything above stays owned by the
        // cache (§ performance note above).
        let mut shown: Vec<Line> = Vec::with_capacity(end - start);
        for i in start..end {
            if i < committed_len {
                shown.push(self.transcript_cache[i].clone());
            } else {
                shown.push(tail[i - committed_len].clone());
            }
        }
        // Remember the pane's geometry + the visible rows as plain text, so a
        // mouse drag can be mapped to transcript coordinates and a finished
        // selection copied **without** re-rendering the transcript (cli
        // 2026-09-27「鼠标拖动可以选中 UI 上的文字」).
        self.pane_area = pad;
        self.viewport_start = start as u32;
        self.viewport_lines = shown
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        frame.render_widget(Paragraph::new(shown), pad);
        // Paint the live drag selection inverse (§11 增补：应用内选择). Done on the
        // frame buffer after the paragraph, since a selection is an overlay.
        if let Some(sel) = self.selection {
            paint_selection(frame, pad, start, &self.viewport_lines, sel);
        }
        // A scrolled-up indicator (the alternate buffer has no native scrollback,
        // so make the position explicit — §11). Paging to the ends is bound to
        // `Ctrl+Home` / `Ctrl+End` (plain Home/End move the input cursor, §②), so
        // the hint names `ctrl+end` (cli 2026-09-27).
        if self.scroll > 0 && area.height > 2 {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    format!("  ↑ 已上翻 {} 行 · ctrl+end 回到底部", self.scroll),
                    Style::default()
                        .fg(SELECTION_FG)
                        .bg(SELECTION_BG)
                        .add_modifier(Modifier::BOLD),
                ))),
                Rect::new(
                    area.x + 1,
                    area.bottom().saturating_sub(1),
                    area.width.saturating_sub(2),
                    1,
                ),
            );
        }
    }

    /// Extend (or rebuild) [`Self::transcript_cache`] so it holds the whole
    /// committed transcript, flattened under the current fold state and wrapped to
    /// `wrap_width` — the screen lines for `scrollback[..cache_items]`.
    ///
    /// Rebuilt from scratch when the pane width changes, when `ctrl+o` toggles a
    /// fold, or when the transcript shrank (e.g. `take_scrollback` drained it in
    /// tests); otherwise only the **newly pushed** items are appended, so a long
    /// session never re-wraps its history (cli 2026-09-27「翻页很卡」).
    fn ensure_transcript_cache(&mut self, wrap_width: usize) {
        let folds = (self.thinking_expanded, self.output_expanded);
        if self.cache_width != wrap_width
            || self.cache_folds != folds
            || self.cache_items > self.scrollback.len()
        {
            self.transcript_cache.clear();
            self.cache_items = 0;
            self.cache_width = wrap_width;
            self.cache_folds = folds;
        }
        if self.cache_items < self.scrollback.len() {
            let fresh: Vec<Line<'static>> = self.scrollback[self.cache_items..]
                .iter()
                .flat_map(|it| self.item_lines(it, wrap_width))
                .flat_map(|l| wrap_line(&l, wrap_width))
                .collect();
            self.transcript_cache.extend(fresh);
            self.cache_items = self.scrollback.len();
        }
    }

    /// The idle tip shown in the empty input box (§14-P3), or `None` when one
    /// must not show. This is the whole "priority" rule: a tip yields the box to
    /// a turn in flight and to anything the user has typed, and appears only when
    /// the editor is empty and the app is idle — exactly CC's behaviour. The tip
    /// itself steps every `TIP_COOLDOWN` (see [`crate::motion::tip_at`]).
    fn placeholder_tip(&self) -> Option<&'static str> {
        if self.is_busy() || !self.editor.is_empty() {
            return None;
        }
        Some(tip_at(self.tip_started.elapsed().as_millis() as u64))
    }

    /// The editor's text broken into **visual** rows at `width` columns — each
    /// logical line wrapped with [`wrap_cjk`], so a long line no longer runs off
    /// the box (cli 2026-09-27「多行文本框」). Empty text yields one empty row (so
    /// the prompt/mascot still draws), mirroring `text.split('\n')` on `""`.
    fn editor_visual_lines(&self, width: usize) -> Vec<String> {
        let width = width.max(1);
        let mut out = Vec::new();
        for seg in self.editor.text.split('\n') {
            if seg.is_empty() {
                out.push(String::new());
            } else {
                // `wrap_cjk_keep`, **not** `wrap_cjk`: a space the user just typed
                // is real content and must stay visible in the box (cli
                // 2026-10-04「按完空格后空格进去了但是不显示」). `wrap_cjk` drops
                // trailing spaces, which is right for prose (see `wrap.rs`).
                out.extend(wrap_cjk_keep(seg, width));
            }
        }
        out
    }

    /// Number of visual rows in the editor at `width` columns (drives the box
    /// height, cli 2026-09-27).
    fn editor_visual_line_count(&self, width: usize) -> usize {
        self.editor_visual_lines(width).len()
    }

    /// The multiline input box (§12.1 / §13): CC frames the prompt in a rounded
    /// box — `❯ ` on the first line, a two-space indent on continuations. When
    /// the editor is empty and idle, a rotating tip takes the placeholder slot
    /// (§14-P3). The terminal cursor is placed at the editor's cursor position
    /// (mid-line / mid-buffer, §②).
    fn render_input(&mut self, frame: &mut Frame, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(BORDER));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.height == 0 || inner.width == 0 {
            return;
        }
        // Content width leaves room for the `❯ ` / two-space indent (both 2 cols).
        let content_w = (inner.width as usize).saturating_sub(2).max(1);
        let view_rows = inner.height as usize;

        let mut lines: Vec<Line> = Vec::new();
        // Visual cursor row/col (wrapped), so a long line places the cursor on
        // the right wrapped row (cli 2026-09-27「多行文本框」).
        let mut cursor_row = 0usize;
        let mut cursor_col = 0usize;
        if let Some(tip) = self.placeholder_tip() {
            // Empty + idle → the tip takes the placeholder slot (dim, so it
            // reads as a hint rather than typed text) — §14-P3 — and the mascot
            // smiles from the right edge (§12.10 P1-b).
            self.input_scroll = 0;
            let mut spans = vec![
                Span::styled(
                    "❯ ",
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ),
                Span::styled(tip, Style::default().fg(TEXT_MUTED)),
            ];
            push_mascot(&mut spans, inner.width as usize, false);
            lines.push(Line::from(spans));
        } else if self.editor.is_empty() && self.is_busy() && !self.queue.is_empty() {
            // Mid-turn with a queued message: the empty box advertises the queue
            // (cli 2026-09-24 ③, CC 「Press up to edit queued messages」).
            self.input_scroll = 0;
            lines.push(Line::from(vec![
                Span::styled(
                    "❯ ",
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ),
                Span::styled("按 ↑ 编辑排队消息", Style::default().fg(TEXT_MUTED)),
            ]));
        } else {
            // An empty box mid-turn (the buffer clears on submit) shows the
            // mascot napping on the prompt line; a typed buffer shows the text,
            // wrapped to the box width (cli 2026-09-27「多行文本框」).
            let napping = self.editor.is_empty() && self.is_busy();
            let visual = self.editor_visual_lines(content_w);
            let (crow, ccol) =
                visual_cursor_row_col(&self.editor.text, self.editor.cursor, content_w);
            cursor_row = crow;
            cursor_col = ccol;
            // Scroll the box so the cursor stays visible when the buffer is
            // taller than the box.
            let max_scroll = visual.len().saturating_sub(view_rows);
            let mut sc = self.input_scroll as usize;
            if cursor_row < sc {
                sc = cursor_row;
            } else if cursor_row >= sc + view_rows {
                sc = cursor_row.saturating_sub(view_rows - 1);
            }
            sc = sc.min(max_scroll);
            self.input_scroll = sc as u16;
            for (i, vis) in visual.iter().enumerate().skip(sc).take(view_rows) {
                let (prefix, pstyle) = if i == 0 {
                    (
                        "❯ ",
                        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                    )
                } else {
                    ("  ", Style::default().fg(TEXT_MUTED))
                };
                let mut spans = vec![Span::styled(prefix, pstyle)];
                if napping {
                    push_mascot(&mut spans, inner.width as usize, true);
                } else {
                    spans.push(Span::raw(vis.clone()));
                }
                lines.push(Line::from(spans));
            }
        }
        frame.render_widget(Paragraph::new(lines), inner);
        // Cursor follows the editor's char-index cursor in *visual* (wrapped)
        // coordinates (§② / cli 2026-09-27): row = wrapped rows before the
        // cursor minus the box's scroll, col = display width of the prefix +
        // the cursor's column within its wrapped row (CJK = 2 columns).
        let cursor_x = inner.x + 2 + cursor_col as u16;
        let rel_row = cursor_row.saturating_sub(self.input_scroll as usize);
        let cursor_y = (inner.y + rel_row as u16).min(inner.bottom().saturating_sub(1));
        frame.set_cursor_position(Position::new(
            cursor_x.min(inner.right().saturating_sub(1)),
            cursor_y,
        ));
    }

    /// Footer (§12.1, cli 2026-09-24 ①). The shortcut-hint row is **gone**
    /// (「取消操作提示」) and the whiteboard moved above the input box (③), so the
    /// footer is a single row of *useful* info: the persistent **identity**
    /// right-aligned at the bottom (§12.1), and — transiently — the `已复制 N 字符`
    /// confirmation left-aligned on that same row.
    ///
    /// The confirmation used to be painted over the conversation pane's bottom
    /// row, which is exactly where a drag-selection usually ends: it overpainted
    /// the highlight the user had just made (cli 2026-09-27「左下角弹出的复制提示
    /// 顶掉了选择」). The footer row is never part of a selection (`pane_area`
    /// excludes it), so nothing is hidden any more.
    fn render_footer(&mut self, frame: &mut Frame, area: Rect) {
        if area.height == 0 {
            return;
        }
        // cli 2026-09-24 (「活动行那里放状态，下面放状态的可以去掉了」): the footer's
        // **status row is gone** — the activity line in the transcript now carries
        // the running status — and the context breakdown moved up into the band
        // above the input box. What remains is the single persistent **identity**
        // line (§12.1), right-aligned at the bottom.
        let id_text = format!(
            "{} v{} · {} · {}",
            brand::NAME,
            self.version,
            self.model,
            self.cwd
        );
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                truncate(&id_text, area.width as usize),
                Style::default().fg(TEXT_MUTED),
            )))
            .alignment(Alignment::Right),
            Rect {
                x: area.x,
                y: area.y,
                width: area.width,
                height: 1,
            },
        );
        // The **left slot** (cli 2026-09-28 ④「整个UI左下角那个位置看着空空的，加点
        // 元素，平常用来显示状态、阶段」): the running status verb plus the canonical
        // stage name — this half of the row used to be permanently blank. It shares
        // the row with the right-aligned identity (§12.1) and is skipped when the
        // two would not both fit.
        let status = self.footer_status();
        if !status.is_empty() {
            let text = format!(" {status}");
            let used = display_width(&text) + display_width(&id_text) + 1;
            if used <= area.width as usize {
                let colour = if self.state == State::Streaming {
                    ACCENT
                } else {
                    TEXT_MUTED
                };
                frame.render_widget(
                    Paragraph::new(Line::from(Span::styled(text, Style::default().fg(colour)))),
                    Rect {
                        x: area.x,
                        y: area.y,
                        width: area.width,
                        height: 1,
                    },
                );
            }
        }
        // A pending-exit hint (cli 2026-09-28「第一次按下后给提示」) takes the
        // left slot while a first `Ctrl+C` is armed — the user must never be
        // surprised by the quit. It outranks both the status slot and the copy
        // confirmation (a copy can still be re-read from the clipboard, a pending
        // quit cannot).
        if self.take_exit_arm_hint() {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    format!(" {EXIT_ARM_HINT}"),
                    Style::default().fg(WARN).add_modifier(Modifier::BOLD),
                ))),
                Rect {
                    x: area.x,
                    y: area.y,
                    width: area.width,
                    height: 1,
                },
            );
            return;
        }
        // The transient copy confirmation, left-aligned on the same row — only
        // when it still fits beside the identity (never overpainted).
        match &self.copy_note {
            Some((msg, at)) if at.elapsed().as_secs_f64() < COPY_NOTE_SECS => {
                let text = format!(" {msg}");
                let used = display_width(&text) + display_width(&id_text) + 1;
                if used <= area.width as usize {
                    frame.render_widget(
                        Paragraph::new(Line::from(Span::styled(
                            text,
                            Style::default().fg(SUCCESS).add_modifier(Modifier::BOLD),
                        ))),
                        Rect {
                            x: area.x,
                            y: area.y,
                            width: area.width,
                            height: 1,
                        },
                    );
                }
            }
            // Expire it so it cannot linger in the state forever.
            Some(_) => self.copy_note = None,
            None => {}
        }
    }

    /// The footer's **left slot** text (cli 2026-09-28 ④「整个UI左下角那个位置看着
    /// 空空的，加点元素，平常用来显示状态、阶段」): the current status and the
    /// pipeline stage in plain language — `作答中 · 工作阶段` while a turn runs,
    /// `待命` when idle. The slot's width guard lives in [`Self::render_footer`].
    ///
    /// cli 2026-10-05 (「要进界面的，这是核心体验」): while a wait is being polled
    /// the slot says **what** it is waiting on and how long — `外部命令 · 已等 45s`
    /// — so the status text and the transcript heartbeat agree. The footer is a
    /// single line, so this stays a short form; the full `静默 / 裁判` detail is
    /// the transcript line ([`Self::wait_notice_line`]).
    fn footer_status(&self) -> String {
        match self.state {
            State::Streaming => {
                if let Some(w) = &self.wait_notice {
                    let class = if w.class.is_empty() {
                        "等待中"
                    } else {
                        w.class.as_str()
                    };
                    return format!("{class} · 已等 {}s", w.elapsed_ms / 1000);
                }
                // The **verb** half must not come from `activity_status()` here:
                // that returns the role name for auto stages, so the slot read
                // `Auto·Main · Auto·Main` (cli 2026-10-04「左下角状态那里没有」).
                // Split the two halves explicitly — verb from `human_stage`, stage
                // from `self.stage` — so auto reads `作答中 · Auto·Main`.
                let verb = if self.stage.is_empty() {
                    "作答中".to_string()
                } else {
                    human_stage(&self.stage).to_string()
                };
                let base = if self.stage.is_empty() {
                    verb
                } else {
                    format!("{verb} · {}", self.stage)
                };
                // 自动模式的**当前轮次**（cli 2026-10-04「自动模式需要显示轮次」）：
                // 钉在状态槽尾部，与活动行一致 —— 轮次随对话流滚走后就再也看不到，
                // 而「跑到第几轮了」正是判断还要等多久的唯一线索。
                if is_auto_stage(&self.stage) && self.auto_round > 0 {
                    format!("{base} · 第{}轮", self.auto_round)
                } else {
                    base
                }
            }
            State::Idle => "待命".to_string(),
        }
    }

    /// The transcript's **live wait heartbeat** line (cli 2026-10-05「要进界面的，
    /// 这是核心体验」), or `None` when nothing is being polled.
    ///
    /// Shaped like the running tool card it sits under — `◷ 已等 45s · 静默 40s ·
    /// 继续等` — but drawn in muted chrome grey ([`TEXT_MUTED`]) rather than the
    /// running amber: it is *diagnostic* (the poll's own state), not the work
    /// itself, which the tool card above already signals in colour (§8.4 colour is
    /// a signal; §14-P2 the healthy-work signal belongs to the card).
    ///
    /// `rulings` shows how many times the judge was actually consulted — the poll's
    /// real cost on screen (each ruling is one extra model call), which is exactly
    /// the thing cli could not see before.
    fn wait_notice_line(&self) -> Option<Line<'static>> {
        let w = self.wait_notice.as_ref()?;
        let mut spans = vec![Span::styled(
            format!("{WAIT_MARK} "),
            Style::default().fg(TEXT_MUTED),
        )];
        // Name the subject only when the running card above does not already: under
        // a tool card `what` merely repeats it (`bash(cargo fmt …)` then
        // `bash: cargo fmt …`), while for a between-round-trips wait (② / ⑨) this
        // line is the only thing on screen saying what is going on.
        if self.running_tool.is_none() && !w.what.is_empty() {
            spans.push(Span::styled(
                format!("{} · ", truncate(&w.what, 60)),
                Style::default().fg(TEXT_MUTED),
            ));
        }
        spans.push(Span::styled(
            format!("已等 {}s", w.elapsed_ms / 1000),
            Style::default().fg(TEXT_MUTED),
        ));
        spans.push(Span::styled(
            format!(" · 静默 {}s", w.silent_ms / 1000),
            Style::default().fg(TEXT_MUTED),
        ));
        let verdict = match w.phase.as_str() {
            PHASE_ASKING => "问裁判".to_string(),
            PHASE_CONTINUE => "继续等".to_string(),
            PHASE_INTERRUPT => {
                if w.detail.is_empty() {
                    "中断".to_string()
                } else {
                    format!("中断：{}", w.detail)
                }
            }
            // Below the cost gate: collected, not asked.
            PHASE_SAMPLING => "采样中".to_string(),
            _ => "采样中".to_string(),
        };
        spans.push(Span::styled(
            format!(" · {verdict}"),
            Style::default().fg(TEXT_MUTED),
        ));
        if w.rulings > 0 {
            spans.push(Span::styled(
                format!(" · 已裁决 {} 次", w.rulings),
                Style::default().fg(TEXT_MUTED),
            ));
        }
        Some(Line::from(spans))
    }

    /// Is a first `Ctrl+C` still armed (i.e. inside [`EXIT_ARM_SECS`])? Expires
    /// the arm as a side effect, so a stale hint can never linger — the deadline
    /// is enforced by the renderer as well as by the next key press, because the
    /// run loop repaints ~10×/s even when no key arrives.
    fn take_exit_arm_hint(&mut self) -> bool {
        self.expire_exit_arm();
        self.exit_armed.is_some()
    }

    /// Drop an expired [`EXIT_ARM_SECS`] arm (called every frame and by
    /// [`Self::take_exit_arm_hint`]).
    fn expire_exit_arm(&mut self) {
        if let Some(at) = self.exit_armed
            && at.elapsed().as_secs_f64() >= EXIT_ARM_SECS
        {
            self.exit_armed = None;
        }
    }

    /// The **context size** shown at the pane's bottom-right — a single number
    /// (cli 2026-09-27: 「不需要第 N 轮，只显示一个数字，是当前轮次
    /// 的上下文」). It is the provider-measured `input_tokens` of the **most recent
    /// LLM round-trip** ([`Self::ctx_tokens`]; cli 2026-09-28「显示当前 llm 的输入
    /// tokens 就行」), compacted by [`fmt_tokens`] (`20480` → `20.5K`). No label,
    /// no total/remainder split, no window assumption; empty before the first
    /// call.
    fn ctx_summary(&self) -> String {
        if self.ctx_tokens == 0 {
            return String::new();
        }
        fmt_tokens(self.ctx_tokens)
    }

    /// Colour for the short context line: always the muted grey.
    ///
    /// cli 2026-09-27: the system must **not** assume any context-window size —
    /// provider APIs do not report it reliably and the built-in figure was a
    /// guess. So there is no window, no ratio and no threshold alarm here; the
    /// line reports the provider-measured token count and nothing more. (The old
    /// `ctx_tokens / context_window` → amber-at-70% / red-at-90% signal is gone.)
    fn ctx_line_color(&self) -> Color {
        TEXT_MUTED
    }

    // (The per-section token split (`ctx_section_tokens` / `section_tokens`,
    // which backed the old `总 T · 余 R` line) is gone with that line — cli
    // 2026-09-27: the bottom-right now shows the single current-turn number.)
}

/// The activity line's **leading mark** — a rotating arc (cli 2026-09-28).
///
/// cli 2026-09-24 ④ had copied CC's own asterisk family (`· ✢ ✳ ✶ ✻ ✽`), but the
/// 2026-09-28 report (「橙色的动态行不太稳定…去掉里边 claude 那几个花/星星的
/// emoji」) showed the mark jittering: `fc-list :charset=` proves `✢`/`✳`/`✻`/`✽`
/// are missing from every monospace font installed here, and `·`/`✽` are
/// East-Asian Ambiguous — so the terminal pulled substituted glyphs from a
/// proportional fallback and the six frames collapsed into look-alike asterisks at
/// varying advances. The arc frames are Narrow (1 column) **and** present in both
/// monospace fonts, so the animation steps cleanly. See [`ACTIVITY_SPINNER`].
///
/// Steps fast (≈120ms), like CC.
fn cc_spinner(elapsed_ms: u64) -> char {
    const STEP_MS: u64 = 120;
    ACTIVITY_SPINNER[((elapsed_ms / STEP_MS) as usize) % ACTIVITY_SPINNER.len()]
}

/// Map a canonical stage name to a plain-language label (§7 去黑话).
///
/// 自动模式（cli 2026-10-04「左下角没有显示状态…也没显示角色」）: 角色名此前走
/// `other => other` 原样返回 → 左下角成了 `Auto·Main · Auto·Main`（动词与阶段同
/// 一个词，既不是状态也看不出角色）。角色跑的是工作阶段式工具循环，所以动词用
/// 「作答中」，**角色名留在下面那半**（`footer_status` 拼的是
/// `{动词} · {self.stage}`），于是左槽读作 `作答中 · Auto·Main` —— 状态与角色各归
/// 其位。
fn human_stage(stage: &str) -> &str {
    match stage {
        "组织上下文" => "检索中",
        "工作阶段" => "作答中",
        "沉淀阶段" => "沉淀中",
        _ if is_auto_stage(stage) => "作答中",
        other => other,
    }
}

/// Compact token count: `1234` → `1.2K`.
fn fmt_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1000 {
        format!("{:.1}K", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

/// The single most informative argument, rendered CC-style as `name(arg)`.
///
/// Prefers a per-tool canonical key (`bash`→`command`, `read_file`→`file_path`,
/// …), else the first value; collapsed to one line and truncated.
fn tool_primary_arg(tool: &str, args: &serde_json::Value) -> String {
    let key = match tool {
        "bash" => "command",
        "read_file" | "write_file" | "edit" | "delete_file" | "copy_file" => "file_path",
        "grep" | "glob" => "pattern",
        "list_directory" => "path",
        "search_observations" | "search_knowledge" | "search_archive" | "search_memory" => "query",
        _ => "",
    };
    fn pick(v: &serde_json::Value) -> Option<String> {
        let s = match v {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Null => return None,
            other => other.to_string(),
        };
        let s = s.replace('\n', " ");
        let s = s.trim();
        if s.is_empty() {
            None
        } else {
            Some(truncate(s, 60))
        }
    }
    if let Some(v) = args.get(key).and_then(pick) {
        return v;
    }
    if let Some(map) = args.as_object() {
        for v in map.values() {
            if let Some(s) = pick(v) {
                return s;
            }
        }
    }
    String::new()
}

/// Format a tool duration CC/lingmiao style: `4.3ms` / `12ms`.
fn fmt_ms(ms: f64) -> String {
    if ms < 10.0 {
        format!("{ms:.1}ms")
    } else {
        format!("{ms:.0}ms")
    }
}

/// Truncate `s` to `max` display columns.
pub(crate) fn truncate(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut col = 0;
    for ch in s.chars() {
        let w = char_width(ch);
        if col + w > max {
            break;
        }
        out.push(ch);
        col += w;
    }
    out
}

/// Display width of a string in terminal columns (CJK counted as 2).
pub(crate) fn display_width(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

/// The char index in `text` whose **display column** is nearest `col` (CJK = 2
/// columns). A column landing inside a wide glyph resolves to that glyph's char
/// (so a selection never starts or ends mid-character — cli 2026-09-27「鼠标拖动
/// 可以选中 UI 上的文字」). Columns past the end clamp to `chars().count()`.
fn display_col_to_char_idx(text: &str, col: u16) -> usize {
    let col = col as usize;
    let mut w = 0usize;
    for (i, ch) in text.chars().enumerate() {
        let cw = char_width(ch);
        if w + cw > col {
            return i;
        }
        w += cw;
    }
    text.chars().count()
}

/// Paint a live drag selection **inverse** on `frame` — an overlay drawn after
/// the transcript paragraph, so it never disturbs the transcript cache (§23.1).
///
/// Only the visible window is touched, and a wide (CJK) glyph is highlighted over
/// both of its cells, so the inverse block has no seams.
fn paint_selection(
    frame: &mut Frame,
    pad: Rect,
    viewport_start: usize,
    lines: &[String],
    sel: Selection,
) {
    let (mut a, mut b) = (sel.anchor, sel.head);
    if (a.0, a.1) > (b.0, b.1) {
        std::mem::swap(&mut a, &mut b);
    }
    let start = viewport_start as u32;
    let last = start + lines.len().saturating_sub(1) as u32;
    let a_line = a.0.clamp(start, last);
    let b_line = b.0.clamp(start, last);
    let buf = frame.buffer_mut();
    for line in a_line..=b_line {
        let text = &lines[(line - start) as usize];
        let row = pad.y + (line - start) as u16;
        // Per-char x offsets: the start of glyph `i` and its width, so a wide
        // glyph's two cells are both painted.
        let mut x = pad.x;
        for ch in text.chars() {
            let cw = char_width(ch) as u16;
            let (from, to) = if line == a_line && line == b_line {
                (a.1, b.1)
            } else if line == a_line {
                (a.1, u16::MAX)
            } else if line == b_line {
                (0, b.1)
            } else {
                (0, u16::MAX)
            };
            // The glyph's span in display columns is [x - pad.x, x - pad.x + cw).
            let g0 = x - pad.x;
            let g1 = g0 + cw;
            if g1 > from && g0 < to {
                for cx in x..(x + cw) {
                    if cx < pad.right() {
                        buf[(cx, row)].modifier.insert(Modifier::REVERSED);
                    }
                }
            }
            x += cw;
            if x >= pad.right() {
                break;
            }
        }
        // A selection that runs to end-of-line highlights the row's tail too, so
        // the block reads as a full line rather than stopping at the last glyph.
        if line != b_line || b.1 == u16::MAX {
            for cx in x..pad.right() {
                buf[(cx, row)].modifier.insert(Modifier::REVERSED);
            }
        }
    }
}

/// The editor cursor's `(row, col)` in **visual** (wrapped) coordinates — `row`
/// counts wrapped rows before the cursor and `col` is the display width of the
/// cursor's own wrapped row, so the placement stays correct when a long line
/// wraps (cli 2026-09-27「多行文本框」). `width` is the content width in columns
/// (the box width minus the 2-column prompt indent). Pure and unit-testable.
pub(crate) fn visual_cursor_row_col(text: &str, cursor: usize, width: usize) -> (usize, usize) {
    let width = width.max(1);
    let chars: Vec<char> = text.chars().collect();
    let cursor = cursor.min(chars.len());
    let before: String = chars[..cursor].iter().collect();
    let segs: Vec<&str> = before.split('\n').collect();
    let mut row = 0usize;
    let mut col = 0usize;
    for (i, seg) in segs.iter().enumerate() {
        // Same wrapper as the render path (`editor_visual_lines`): a trailing
        // space is a real column, so the caret must sit past it (cli 2026-10-04
        // 空格显示 bug — the two paths must agree, or the caret trails the text).
        let wrapped = wrap_cjk_keep(seg, width);
        if i + 1 == segs.len() {
            // The cursor sits at the end of the last segment.
            row += wrapped.len().saturating_sub(1);
            col = wrapped.last().map(|s| display_width(s)).unwrap_or(0);
        } else {
            row += wrapped.len();
        }
    }
    (row, col)
}

/// Wrap one committed [`Line`] to `width` columns for the conversation pane
/// (§12.9).
///
/// Rendering used to hand ratatui a `.wrap()` and let it break per character —
/// which split ASCII words and ignored CJK 禁则. We now pre-wrap with
/// [`wrap_cjk`] so the rules hold, and drop ratatui's wrap so it cannot undo
/// them. A line that already fits is passed through untouched (keeping all its
/// span styles); only an over-wide line is re-emitted, reusing its dominant
/// (first non-empty) span style — good enough for the uniform-style paragraphs
/// that overflow.
fn wrap_line(line: &Line<'static>, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    if text.is_empty() || display_width(&text) <= width {
        return vec![line.clone()];
    }
    // Flatten to styled characters so every output line keeps the **per-span**
    // styles. cli 2026-09-24 (「有个工具调用的标题是全绿」): the old code re-emitted
    // every wrapped segment with the *first* span's style, so an over-wide
    // multi-colour line — a tool card header (green `●` + grey tag + bright name +
    // grey ms) — collapsed to a single colour the moment it had to wrap (an 80-col
    // terminal turned the whole title green).
    let cells: Vec<(char, Style)> = line
        .spans
        .iter()
        .flat_map(|s| s.content.chars().map(move |c| (c, s.style)))
        .collect();
    // A markdown list item hangs its continuation lines under the item text (§12.9).
    let hanging = list_hanging(&text).filter(|h| h + 2 <= width);
    wrap_cells(&cells, width, hanging)
        .into_iter()
        .map(cells_to_line)
        .collect()
}

/// A run of styled characters — one wrapped output line before it is merged into
/// [`Span`]s (see [`cells_to_line`]).
type StyledCells = Vec<(char, Style)>;

/// Wrap a styled character run to `width` columns, returning one styled run per
/// output line — each character keeps its own [`Style`]. Mirrors
/// [`crate::wrap::wrap_cjk`]'s token + CJK 禁则 rules, and applies a `hanging`
/// indent to every line after the first (a list item's continuations, §12.9).
fn wrap_cells(
    cells: &[(char, Style)],
    width: usize,
    hanging: Option<usize>,
) -> Vec<Vec<(char, Style)>> {
    let width = width.max(1);
    let indent_style = cells.first().map(|(_, s)| *s).unwrap_or_default();
    let (cap, indent) = match hanging {
        Some(h) if h < width => (width - h, h),
        _ => (width, 0),
    };
    let cap = cap.max(1);
    // Wrap each explicit line separately (a committed `Line` normally holds none).
    let mut lines: Vec<Vec<(char, Style)>> = Vec::new();
    for para in cells.split(|(c, _)| *c == '\n') {
        if para.is_empty() {
            continue;
        }
        lines.extend(wrap_paragraph_cells(para, cap));
    }
    if lines.is_empty() {
        return vec![Vec::new()];
    }
    // Trailing spaces at a break are dropped (as `wrap_cjk` does).
    for l in lines.iter_mut() {
        while matches!(l.last(), Some((' ', _))) {
            l.pop();
        }
    }
    // Hang the continuation lines under the item text.
    if indent > 0 {
        let pad: Vec<(char, Style)> = std::iter::repeat_n((' ', indent_style), indent).collect();
        for l in lines.iter_mut().skip(1) {
            if !l.is_empty() {
                let mut with = pad.clone();
                with.append(l);
                *l = with;
            }
        }
    }
    lines
}

/// Greedy token fill of one paragraph of styled cells — the styled mirror of
/// [`crate::wrap::wrap_paragraph`], so the two never diverge on breaks/禁则.
fn wrap_paragraph_cells(cells: &[(char, Style)], width: usize) -> Vec<Vec<(char, Style)>> {
    let width = width.max(1);
    // Tokenise: an ASCII non-space run is one word; every other character its own.
    let mut toks: Vec<Vec<(char, Style)>> = Vec::new();
    let mut word: Vec<(char, Style)> = Vec::new();
    for &(c, st) in cells {
        if c.is_ascii() && c != ' ' && c != '\t' {
            word.push((c, st));
        } else {
            if !word.is_empty() {
                toks.push(std::mem::take(&mut word));
            }
            toks.push(vec![(c, st)]);
        }
    }
    if !word.is_empty() {
        toks.push(word);
    }
    let tw = |t: &[(char, Style)]| t.iter().map(|(c, _)| char_width(*c)).sum::<usize>();
    let is_punct = |t: &[(char, Style)], set: &str| t.len() == 1 && set.contains(t[0].0);
    let flatten = |ts: &[Vec<(char, Style)>]| -> Vec<(char, Style)> {
        ts.iter().flat_map(|t| t.iter().copied()).collect()
    };

    let mut out: Vec<Vec<(char, Style)>> = Vec::new();
    let mut cur: Vec<Vec<(char, Style)>> = Vec::new();
    let mut cur_w = 0usize;
    let mut i = 0usize;
    while i < toks.len() {
        let w = tw(&toks[i]);
        if cur_w + w <= width {
            cur_w += w;
            cur.push(toks[i].clone());
            i += 1;
            continue;
        }
        if cur.is_empty() {
            // A word wider than the whole line has to be cut (no break point).
            let (head, rest) = split_cells_at_width(&toks[i], width);
            out.push(head);
            if rest.is_empty() {
                i += 1;
            } else {
                toks[i] = rest;
            }
            continue;
        }
        // 禁则 (§12.9): a closing punctuation must not start a line and an opening
        // one must not end one — backtrack a token so it keeps a neighbour.
        let pull = (is_punct(&toks[i], CLOSING) || is_punct(cur.last().unwrap(), OPENING))
            && cur.len() >= 2;
        if pull {
            let last = cur.pop().unwrap();
            out.push(flatten(&cur));
            cur.clear();
            cur_w = tw(&last);
            cur.push(last);
            continue; // re-examine the same token on the fresh line
        }
        out.push(flatten(&cur));
        cur.clear();
        cur_w = 0;
        // `i` unchanged — the token starts the next line.
    }
    if !cur.is_empty() {
        out.push(flatten(&cur));
    }
    out
}

/// Split styled `cells` into `(head, rest)` where `head` is the longest prefix
/// whose display width is ≤ `width`.
fn split_cells_at_width(cells: &[(char, Style)], width: usize) -> (StyledCells, StyledCells) {
    let mut w = 0usize;
    let mut i = 0usize;
    while i < cells.len() {
        let cw = char_width(cells[i].0);
        if w + cw > width {
            break;
        }
        w += cw;
        i += 1;
    }
    (cells[..i].to_vec(), cells[i..].to_vec())
}

/// Merge adjacent same-style cells into styled [`Span`]s on one [`Line`].
fn cells_to_line(cells: Vec<(char, Style)>) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let mut cur: Option<Style> = None;
    for (c, st) in cells {
        if cur != Some(st) {
            if let Some(prev) = cur {
                spans.push(Span::styled(std::mem::take(&mut buf), prev));
            }
            cur = Some(st);
        }
        buf.push(c);
    }
    if let Some(prev) = cur {
        spans.push(Span::styled(buf, prev));
    }
    Line::from(spans)
}

/// The display-column width of a markdown list marker — the hanging indent for
/// its continuation lines (§12.9). `None` when the line is not a list item.
///
/// A marker counts **only where a list item can actually begin**: at the very
/// start of the line, after the transcript's own render chrome (`● ` / `∴ ` /
/// `⠋ ` / `❯ `, then an optional stage tag `丨检索中 ` …) and any nesting indent.
///
/// The old version scanned the **whole** line for a `• `/`- `/`* `/`+ `/`N. `
/// sequence, so any line that merely *contained* one hung — a tool-card header
/// (`● 丨作答中 bash(which …; echo "--- DISPLAY")  4s`) hung **70** columns, which
/// left its continuation lines a `width − 70` body: at 80 columns that is 8
/// cells, i.e. one short word per line — the「一个词一行、竖着排」garble of a
/// **running bash card** (cli 2026-09-28 ②, `docs/粘贴的图像 (6).png`).
fn list_hanging(text: &str) -> Option<usize> {
    let (rest, used) = strip_item_chrome(text);
    let chars: Vec<char> = rest.chars().collect();
    let take = |n: usize| -> usize { display_width(&chars[..n].iter().collect::<String>()) };
    // Unordered marker: `• ` / `- ` / `* ` / `+ `.
    if matches!(chars.first(), Some('•' | '-' | '*' | '+')) && chars.get(1) == Some(&' ') {
        return Some(used + take(2));
    }
    // Ordered marker: `1. ` / `12. `.
    if chars.first().is_some_and(|c| c.is_ascii_digit()) {
        let mut j = 0usize;
        while j < chars.len() && chars[j].is_ascii_digit() {
            j += 1;
        }
        if j < chars.len() && chars[j] == '.' && chars.get(j + 1) == Some(&' ') {
            return Some(used + take(j + 2));
        }
    }
    None
}

/// Strip the transcript's **render chrome** — the leading glyph
/// (`●` / `∴` / `⠋` / `❯`), an optional stage tag (`丨检索中 ` / `丨作答中 ` /
/// `丨沉淀中 `) and any nesting indent — returning the remainder plus the display
/// columns consumed. This is what makes [`list_hanging`] fire only on a
/// *line-leading* marker instead of any marker-shaped substring (§12.9).
fn strip_item_chrome(text: &str) -> (&str, usize) {
    let mut rest = text;
    let mut used = 0usize;
    // The leading glyph: the running-card spinner is the braille [`SPINNER`]
    // family (it steps every 100 ms), the rest are fixed.
    let mut chars = rest.chars();
    if let Some(c) = chars.next() {
        if SPINNER.contains(&c) {
            let glyph = format!("{c} ");
            if let Some(r) = rest.strip_prefix(&glyph) {
                used += display_width(&glyph);
                rest = r;
            }
        }
    }
    for glyph in ["● ", "∴ ", "❯ ", "◜ "] {
        if let Some(r) = rest.strip_prefix(glyph) {
            used += display_width(glyph);
            rest = r;
            break;
        }
    }
    for tag in ["丨检索中 ", "丨作答中 ", "丨沉淀中 "] {
        if let Some(r) = rest.strip_prefix(tag) {
            used += display_width(tag);
            rest = r;
            break;
        }
    }
    // Nesting indent: markdown emits two spaces per list depth.
    let trimmed = rest.trim_start_matches(' ');
    used += display_width(&rest[..rest.len() - trimmed.len()]);
    (trimmed, used)
}

/// The working directory, with `$HOME` shortened to `~` for the identity block
/// (startup banner + footer bottom-right, §12.1).
fn short_cwd() -> String {
    let Ok(dir) = std::env::current_dir() else {
        return String::from(".");
    };
    let s = dir.display().to_string();
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() {
            if let Some(rest) = s.strip_prefix(&home) {
                return format!("~{rest}");
            }
        }
    }
    s
}

/// Approximate `wcwidth` for the ranges our text actually uses.
pub(crate) fn char_width(c: char) -> usize {
    match c as u32 {
        0x1100..=0x115F
        | 0x2E80..=0x303E
        | 0x3041..=0x33FF
        | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF
        | 0xA000..=0xA4CF
        | 0xAC00..=0xD7A3
        | 0xF900..=0xFAFF
        | 0xFE30..=0xFE4F
        | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6
        | 0x1F300..=0x1FAFF // emoji (🔧 etc.) — rendered double-width
        | 0x20000..=0x3FFFD => 2,
        _ => 1,
    }
}

/// Short stage badge shown on conversation items (§4 「来自哪个阶段」).
///
/// cli 2026-09-24 ⑦ (「每个思考工具调用和正文输出都要有阶段标志，饱和度拉低降低
/// 存在感」): every item — thinking, tool call and reply — is tagged with the
/// stage that produced it. The tag is drawn in the **low-saturation**
/// [`STAGE_TAG`] grey so it is present for attribution without shouting over the
/// body (colour stays a signal, §8.4).
///
/// 自动模式（cli 2026-10-04「各控件也没有显示阶段和角色」）: 角色的 `stage` 是
/// `Auto·Main` / `Auto·Auditor`，此前落进 `""` 兜底 —— 角色的思考块、工具卡、
/// 正文**一个标签都没有**，屏幕上分不出这是 Main 干的还是 Auditor 干的。角色
/// 标签因此用**同一个 `丨` 指针**（同一套低饱和灰，不是新控件），只把归属名换成
/// 角色名：`丨Main` / `丨Auditor`。
fn stage_badge(stage: &str) -> &'static str {
    if stage == STAGE_B {
        "丨检索中"
    } else if stage == STAGE_C {
        "丨作答中"
    } else if stage == STAGE_SUMMARY {
        "丨沉淀中"
    } else if stage == AUTO_MAIN_STAGE {
        "丨Main"
    } else if stage == AUTO_AUDITOR_STAGE {
        "丨Auditor"
    } else {
        ""
    }
}

/// The stage tag as a styled span (low-saturation, cli 2026-09-24 ⑦), or `None`
/// when the stage has no badge (unknown / empty stage).
fn stage_tag_span(stage: &str) -> Option<Span<'static>> {
    let tag = stage_badge(stage);
    if tag.is_empty() {
        None
    } else {
        Some(Span::styled(
            format!("{tag} "),
            Style::default().fg(STAGE_TAG),
        ))
    }
}

/// The CC-style **reverse-video user bar** for `text` at pane `width`: ` ❯ ` on
/// the first line, a three-space continuation indent, painted inverse and padded
/// to the pane so it reads as a solid band. Shared by a committed user turn
/// ([`App::push_turn`]) and a pending **queued** message (cli 2026-09-24 ③).
/// Empty when `text` is blank.
fn user_bar_lines(text: &str, width: usize) -> Vec<Line<'static>> {
    let width = width.max(8);
    let text = text.trim();
    if text.is_empty() {
        return Vec::new();
    }
    let bar = Style::default().add_modifier(Modifier::REVERSED);
    let prompt = Style::default()
        .fg(ACCENT)
        .add_modifier(Modifier::BOLD | Modifier::REVERSED);
    // The lead (` ❯ ` / continuation `   `) is 3 columns wide, so the wrapped
    // text must leave those 3 columns for the bar to reach exactly `width` —
    // otherwise its right edge would be off by one and `wrap_line` would
    // re-split it (§12.9).
    let wrap = width.saturating_sub(3).max(4);
    let mut lines = Vec::new();
    for (i, seg) in wrap_cjk(text, wrap).into_iter().enumerate() {
        let lead = if i == 0 { " ❯ " } else { "   " };
        let mut spans = vec![Span::styled(lead, prompt)];
        let used = display_width(lead) + display_width(&seg);
        spans.push(Span::styled(seg, bar));
        if used < width {
            spans.push(Span::styled(" ".repeat(width - used), bar));
        }
        lines.push(Line::from(spans));
    }
    lines
}

/// The trailing hint a folded tool output collapses to (§12.10 P1-a), aligned
/// under the `⎿` result body. It reuses the *same* `ctrl+o 展开` affordance as
/// the reasoning fold so a single key reveals every hidden block.
fn tool_fold_hint(hidden: usize) -> Line<'static> {
    Line::from(Span::styled(
        format!("    ∴ … 还有 {hidden} 行 · ctrl+o 展开"),
        Style::default().fg(TEXT_MUTED),
    ))
}

/// 灵妙's tiny idle mascot (§12.10 P1-b) — a 3-glyph face, upright and
/// normal-spaced (cli has no patience for the「字距怪」look that italic/CJK-width
/// glyphs produce). It naps mid-turn (`•-•`) and smiles at rest (`•ᴗ•`), giving
/// the empty input box the "sign of life" CC gets from its idle animation.
fn mascot(busy: bool) -> &'static str {
    if busy { "•-•" } else { "•ᴗ•" }
}

/// Right-align the mascot on an input line (§12.10 P1-b) if the row has room;
/// otherwise it is dropped rather than overflowing the box. Pure over the
/// already-built `spans`, so the placement is testable without a terminal.
fn push_mascot(spans: &mut Vec<Span<'static>>, inner_width: usize, busy: bool) {
    let glyph = mascot(busy);
    let glyph_w = display_width(glyph);
    let used: usize = spans.iter().map(|s| display_width(&s.content)).sum();
    // Need at least a one-column gap between the content and the mascot.
    if used + 1 + glyph_w <= inner_width {
        let pad = inner_width - used - glyph_w;
        spans.push(Span::raw(" ".repeat(pad)));
        spans.push(Span::styled(glyph, Style::default().fg(ACCENT)));
    }
}

/// The trailing `max_chars` of `text` (at a `char` boundary), used to wrap only
/// the reasoning the folded block can actually show (cli 2026-09-27「翻页很卡」).
///
/// A folded block displays at most [`THINKING_TAIL_LINES`] wrapped rows, so
/// wrapping the whole (possibly megabyte) reasoning just to drop all but six rows
/// dominated the frame time. Cutting a generous suffix bounds the work; the
/// visible rows are the same trailing text (a greedy wrap restarted inside the
/// suffix can only shift a break by at most one row, and the window is several
/// rows wider than what is shown).
///
/// Scanned **from the end** and counting UTF-8 lead bytes (`& 0xC0 != 0x80`), so
/// the cost is ≤ 4·`max_chars` bytes rather than a walk over the whole string —
/// the first version used `char_indices().nth(n - max_chars)`, which cost ~25 ms
/// per frame on a 648 K-char buffer.
fn tail_window(text: &str, max_chars: usize) -> &str {
    if max_chars == 0 || text.is_empty() {
        return "";
    }
    let bytes = text.as_bytes();
    let mut placed = 0usize;
    let mut i = bytes.len();
    while i > 0 && placed < max_chars {
        i -= 1;
        if bytes[i] & 0xC0 != 0x80 {
            placed += 1;
        }
    }
    &text[i..]
}

/// Render a reasoning block (§12.7 思考折叠).
///
/// Collapsed (`expanded == false`) is the CC default — a **title line**
/// (`∴ 思考 Ns · ctrl+o 展开`) over the last few live lines. The title is the
/// **accent** colour (it is a control, not chrome — cli 2026-09-24: 「思考控件的
/// 标题不应该是灰色的」), while the fold hint and the reasoning **body** stay
/// grey and upright (cli 2026-09-21: CC's thought is grey, not cyan/italic — grey
/// is reserved for hidden/returned detail). Keyword emphasis is retained (cli
/// 2026-09-24). Expanded shows the full grey reasoning. No stage badge (cli
/// 2026-09-24: 「先按 CC 的样式做」).
fn thinking_lines(
    stage: &str,
    text: &str,
    secs: f64,
    expanded: bool,
    width: usize,
) -> Vec<Line<'static>> {
    let text = text.trim();
    if text.is_empty() {
        return Vec::new();
    }
    // Reasoning is grey and upright (cli 2026-09-21); keywords are lifted to a
    // soft blue (cli 2026-09-24) — no cyan, no italic, colour only (CC's lift is
    // a hue, not a weight).
    let base = Style::default().fg(TEXT_MUTED);
    let kw = Style::default().fg(THINKING_KW);
    if !expanded {
        // Header line (§12.7): the fold's **title** (`∴ 思考 Ns`) is a real,
        // *coloured* control title — **not** grey chrome (cli 2026-09-24:
        // 「思考控件的标题不应该是灰色的」→「思考的标题用白色」). White (the default
        // foreground) is what cli settled on. Grey is reserved for *hidden*
        // detail: the fold hint (`· ctrl+o 展开`) and the reasoning body.
        let title = Style::default().fg(TEXT).add_modifier(Modifier::BOLD);
        let mut title_spans = vec![Span::styled("∴ ", title)];
        // cli 2026-09-24 ⑦: the thinking block carries its stage tag.
        if let Some(tag) = stage_tag_span(stage) {
            title_spans.push(tag);
        }
        title_spans.push(Span::styled(format!("思考 {secs:.0}s"), title));
        title_spans.push(Span::styled(" · ctrl+o 展开", base));
        let mut out = vec![Line::from(title_spans)];
        // … then the **live tail** (cli 2026-09-21): the last few wrapped lines,
        // grey and upright, so the block never reads as frozen while the model is
        // still thinking.
        //
        // Performance (cli 2026-09-27「翻页很卡」): only a bounded suffix of the
        // reasoning is wrapped — the fold shows at most [`THINKING_TAIL_LINES`]
        // rows, so wrapping a megabyte of text every frame (while it streams) is
        // pure waste. The window is generously larger than the visible tail, so
        // the displayed rows are identical to a full wrap in practice.
        let wrap = width.saturating_sub(4).max(8);
        let segs = wrap_cjk(tail_window(text, (THINKING_TAIL_LINES + 6) * wrap), wrap);
        let start = segs.len().saturating_sub(THINKING_TAIL_LINES);
        for seg in segs.into_iter().skip(start) {
            let mut spans = vec![Span::styled("  ", base)];
            spans.extend(thinking_text_spans(&seg, base, kw));
            out.push(Line::from(spans));
        }
        return out;
    }
    let wrap = width.saturating_sub(4).max(8);
    let mut out = Vec::new();
    for (i, seg) in wrap_cjk(text, wrap).into_iter().enumerate() {
        let mut spans = vec![Span::styled(if i == 0 { "∴ " } else { "  " }, base)];
        if i == 0 {
            // cli 2026-09-24 ⑦: the expanded block keeps its stage tag.
            if let Some(tag) = stage_tag_span(stage) {
                spans.push(tag);
            }
        }
        spans.extend(thinking_text_spans(&seg, base, kw));
        out.push(Line::from(spans));
    }
    out
}

/// Split a wrapped reasoning segment into styled spans, lifting **keywords** to
/// `kw` and leaving prose at `base` (cli 2026-09-24: 「CC 支持在 thinking 里高亮
/// 关键词，咱们也要有」).
fn thinking_text_spans(text: &str, base: Style, kw: Style) -> Vec<Span<'static>> {
    thinking_keyword_runs(text)
        .into_iter()
        .map(|(run, is_kw)| Span::styled(run, if is_kw { kw } else { base }))
        .collect()
}

/// Tokenise `text` into `(run, is_keyword)` pieces for reasoning keyword
/// emphasis. The text is cut into maximal **identifier-ish** runs (ASCII word
/// chars plus the punctuation that joins identifiers, paths and modules) and the
/// gaps between them; a run is a **keyword** when it reads as code/API rather
/// than prose (see [`is_keyword_token`]) — so CJK text glued to a token (`读取
/// app.rs并分析`) still isolates the token. Prose runs collapse into as few spans
/// as possible (「少即是多」) and the concatenation of all runs equals `text`
/// exactly. Pure and unit-tested.
fn thinking_keyword_runs(text: &str) -> Vec<(String, bool)> {
    let mut out: Vec<(String, bool)> = Vec::new();
    let mut cur = String::new();
    let mut cur_code = false;
    let mut started = false;
    for ch in text.chars() {
        let code = is_code_char(ch);
        if !started {
            started = true;
            cur_code = code;
        } else if code != cur_code {
            let kw = cur_code && is_keyword_token(&cur);
            push_run(&mut out, &mut cur, kw);
            cur_code = code;
        }
        cur.push(ch);
    }
    if started {
        let kw = cur_code && is_keyword_token(&cur);
        push_run(&mut out, &mut cur, kw);
    }
    out
}

/// A character that can appear inside a code/API token: ASCII word chars plus
/// the punctuation that joins identifiers, paths, module paths and backticks.
fn is_code_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '_' | '/' | ':' | '.' | '`')
}

/// Push `cur` into `out` with class `kw`, merging into the previous run when the
/// class matches (so prose collapses into as few spans as possible).
fn push_run(out: &mut Vec<(String, bool)>, cur: &mut String, kw: bool) {
    if cur.is_empty() {
        return;
    }
    let s = std::mem::take(cur);
    if let Some((last, last_kw)) = out.last_mut() {
        if *last_kw == kw {
            last.push_str(&s);
            return;
        }
    }
    out.push((s, kw));
}

/// Does `tok` read as a code/API keyword? See [`thinking_keyword_runs`].
fn is_keyword_token(tok: &str) -> bool {
    if tok.chars().count() < 2 {
        return false;
    }
    if tok.contains('_') || tok.contains('/') || tok.contains("::") {
        return true;
    }
    // `` `backtick` ``-wrapped, or a `name.ext` filename (≥2 trailing chars).
    if tok.starts_with('`') || tok.ends_with('`') {
        return true;
    }
    let chars: Vec<char> = tok.chars().collect();
    for i in 0..chars.len().saturating_sub(1) {
        if chars[i] == '.' && chars[i + 1].is_alphabetic() {
            let n = chars[i + 1..]
                .iter()
                .take_while(|c| c.is_alphanumeric() || **c == '_')
                .count();
            if n >= 2 {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::motion::VERBS;
    use serde_json::{Value, json};

    #[test]
    fn is_keyword_token_classifies_code_tokens() {
        assert!(is_keyword_token("search_memory"));
        assert!(is_keyword_token("app.rs"));
        assert!(is_keyword_token("std::vec"));
        assert!(is_keyword_token("src/lib.rs"));
        assert!(is_keyword_token("`code`"));
        assert!(!is_keyword_token("hello"));
        assert!(!is_keyword_token("e.g."));
        assert!(!is_keyword_token("0.5s"));
        assert!(!is_keyword_token("a"));
    }

    #[test]
    fn thinking_keyword_runs_flags_code_tokens_and_preserves_text() {
        // cli 2026-09-24: reasoning keywords are emphasised; prose is not.
        let text = "重新读取 app.rs 里的 search_memory 与 std::vec，普通词不算";
        let runs = thinking_keyword_runs(text);
        let kw: Vec<&str> = runs
            .iter()
            .filter(|(_, k)| *k)
            .map(|(s, _)| s.as_str())
            .collect();
        assert!(kw.contains(&"app.rs"), "filename: {kw:?}");
        assert!(kw.contains(&"search_memory"), "snake_case: {kw:?}");
        assert!(kw.contains(&"std::vec"), "module path: {kw:?}");
        assert!(
            !kw.iter().any(|s| s.contains("普通词")),
            "prose must not be a keyword: {kw:?}"
        );
        // Reassembling the runs reproduces the original exactly (no data loss).
        let joined: String = runs.iter().map(|(s, _)| s.as_str()).collect();
        assert_eq!(joined, text);
    }

    #[test]
    fn thinking_block_is_grey_upright_with_keyword_emphasis() {
        // cli 2026-09-21: no cyan / no italic. cli 2026-09-24: keywords lifted.
        let lines = thinking_lines(
            "正在作答",
            "use search_memory to read app.rs",
            3.0,
            true,
            80,
        );
        assert!(!lines.is_empty());
        let spans: Vec<&Span> = lines.iter().flat_map(|l| l.spans.iter()).collect();
        for s in &spans {
            // Reasoning is grey (TEXT_MUTED) with keyword lifts (THINKING_KW) and
            // the low-saturation stage tag (STAGE_TAG) — never the old cyan, never
            // italic (cli 2026-09-21 / 2026-09-24).
            assert!(
                s.style.fg == Some(TEXT_MUTED)
                    || s.style.fg == Some(THINKING_KW)
                    || s.style.fg == Some(STAGE_TAG),
                "reasoning is grey + keyword-lifted + stage-tagged only: {:?}",
                s.style.fg
            );
            assert!(
                !s.style.add_modifier.contains(Modifier::ITALIC),
                "thinking must be upright"
            );
        }
        assert!(
            spans.iter().any(|s| s.style.fg == Some(THINKING_KW)),
            "a keyword span is emphasised"
        );
        // cli 2026-09-24 (「高亮的处理你看看」): CC lifts the keyword to a **soft
        // blue** — colour only, regular weight. Ours used to be white + bold,
        // which read as plain body text; the lift must not be bold.
        let kw_span = spans
            .iter()
            .find(|s| s.style.fg == Some(THINKING_KW))
            .expect("a keyword span");
        assert!(
            !kw_span.style.add_modifier.contains(Modifier::BOLD),
            "the keyword lift is colour, not weight: {:?}",
            kw_span.style
        );
        let joined: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            joined.contains("search_memory") && joined.contains("app.rs"),
            "keyword text survives verbatim: {joined:?}"
        );
    }

    #[test]
    fn thinking_title_is_white_not_grey() {
        // cli 2026-09-24: 「思考控件的标题不应该是灰色的」→「思考的标题用白色」. The
        // fold's **title** line draws the primary foreground (white), not grey;
        // only the fold *hint* and the reasoning *body* stay grey.
        let lines = thinking_lines("正在作答", "some reasoning here", 7.0, false, 80);
        let title = &lines[0];
        let title_text: String = title.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(title_text.contains("思考"), "title text: {title_text:?}");
        assert!(
            title
                .spans
                .iter()
                .any(|s| { s.style.fg == Some(TEXT) && s.content.contains("思考") }),
            "the thinking title is white (TEXT), not grey: {:?}",
            title.spans.iter().map(|s| s.style.fg).collect::<Vec<_>>()
        );
        // Body (the live tail) stays grey / keyword-lifted only.
        let body_fgs: Vec<Option<Color>> = lines[1..]
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.style.fg)
            .collect();
        assert!(
            body_fgs
                .iter()
                .all(|f| *f == Some(TEXT_MUTED) || *f == Some(THINKING_KW)),
            "reasoning body stays grey: {body_fgs:?}"
        );
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn type_str(app: &mut App, s: &str) {
        for c in s.chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
    }

    fn scrollback_text(app: &mut App) -> String {
        app.take_scrollback()
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn enter_submits_and_echoes_user_turn() {
        let mut app = App::new("m", "p");
        type_str(&mut app, "hello");
        assert_eq!(
            app.on_key(key(KeyCode::Enter)),
            Action::Submit("hello".into())
        );
        assert!(app.editor.is_empty());
        assert!(app.is_busy());
        let sb = scrollback_text(&mut app);
        assert!(sb.contains("❯"));
        assert!(sb.contains("hello"));
        // A second Enter while busy is ignored.
        assert_eq!(app.on_key(key(KeyCode::Enter)), Action::None);
    }

    #[test]
    fn blank_input_is_not_submitted() {
        let mut app = App::new("m", "p");
        app.on_key(key(KeyCode::Char(' ')));
        assert_eq!(app.on_key(key(KeyCode::Enter)), Action::None);
        assert!(!app.is_busy());
    }

    #[test]
    fn up_down_browse_input_history() {
        // §12.4: ↑/↓ recall submitted input (the sidebar navigation they used to
        // drive is gone).
        let mut app = App::new("m", "p");
        // No history yet → ↑ is a no-op that leaves the draft alone.
        assert_eq!(app.on_key(key(KeyCode::Up)), Action::None);
        assert_eq!(app.editor.text, "");
        type_str(&mut app, "first");
        app.on_key(key(KeyCode::Enter));
        app.on_turn_done(Ok(SummaryReport::default()));
        type_str(&mut app, "second");
        app.on_key(key(KeyCode::Enter));
        app.on_turn_done(Ok(SummaryReport::default()));
        // ↑ recalls the newest, then older.
        assert_eq!(app.on_key(key(KeyCode::Up)), Action::None);
        assert_eq!(app.editor.text, "second");
        app.on_key(key(KeyCode::Up));
        assert_eq!(app.editor.text, "first");
        // ↑ at the oldest stays put.
        app.on_key(key(KeyCode::Up));
        assert_eq!(app.editor.text, "first");
        // ↓ walks back out to the (empty) live draft.
        app.on_key(key(KeyCode::Down));
        assert_eq!(app.editor.text, "second");
        app.on_key(key(KeyCode::Down));
        assert_eq!(app.editor.text, "");
    }

    #[test]
    fn ctrl_c_arms_then_quits_on_the_second_press() {
        // cli 2026-09-28: 「按一下 ctrl+c 我本来想复制直接就退出去了，这个组合键要按
        // 两次，第一次按下后给提示」. The first press only **arms** (and shows the
        // hint); the second one, within the window, quits.
        let mut app = App::new("m", "p");
        assert_eq!(app.on_key(ctrl_key('c')), Action::None);
        assert!(app.exit_armed.is_some(), "the first press arms the exit");
        assert!(!app.should_quit(), "…and does not quit");
        assert_eq!(app.on_key(ctrl_key('c')), Action::Quit);
        assert!(app.should_quit());
    }

    #[test]
    fn ctrl_c_arm_expires_so_a_later_press_only_re_arms() {
        // The arm is a double-press *window* (CC's own 800ms), not a sticky flag:
        // a second press after it has lapsed merely arms again.
        let mut app = App::new("m", "p");
        app.on_key(ctrl_key('c'));
        // Backdate the arm past the window instead of sleeping in a test.
        app.exit_armed = Some(Instant::now() - std::time::Duration::from_millis(900));
        assert_eq!(app.on_key(ctrl_key('c')), Action::None);
        assert!(!app.should_quit(), "a late second press does not quit");
        assert!(app.exit_armed.is_some(), "…it re-arms");
    }

    #[test]
    fn any_other_key_disarms_the_pending_exit() {
        // The second Ctrl+C has to be the very next thing the user does — typing
        // in between is a clear "I did not mean to quit".
        let mut app = App::new("m", "p");
        app.on_key(ctrl_key('c'));
        type_str(&mut app, "still here");
        assert!(app.exit_armed.is_none(), "typing disarms the exit");
        assert_eq!(app.on_key(ctrl_key('c')), Action::None);
        assert!(!app.should_quit(), "the re-armed press does not quit");
    }

    #[test]
    fn ctrl_c_with_a_selection_only_copies_never_quits() {
        // The reflex Ctrl+C the change is about: with transcript text selected the
        // press **copies** (the terminal-style「复制我选中的东西」) and that is all
        // — the session the user used to lose is now unreachable by that reflex.
        //
        // A copy must not even *arm* the exit: two quick Ctrl+C aimed at "copy it
        // again" would then quit. So with a highlight live, Ctrl+C stays a copy
        // for as long as the highlight does (drop it with `Esc` to get the
        // arm-and-quit behaviour back), and the selection is never consumed.
        let mut app = App::new("m", "p");
        app.push_info(&["hello world"]);
        let _ = rendered(&mut app);
        drag_select(&mut app, "hello world", 5);
        let before = app.selection;
        assert_eq!(app.on_key(ctrl_key('c')), Action::Copy("hello".into()));
        assert!(!app.should_quit(), "one press never quits");
        assert_eq!(app.selection, before, "the copy keeps the selection");
        assert!(
            app.exit_armed.is_none(),
            "a copy does not arm the exit (a second copy must not quit)"
        );
        // A double-tap of the copy reflex still only copies.
        assert_eq!(app.on_key(ctrl_key('c')), Action::Copy("hello".into()));
        assert!(!app.should_quit(), "two presses still do not quit");
        // Dropping the selection restores the arm-then-quit chord.
        app.on_key(key(KeyCode::Esc));
        assert!(app.selection.is_none());
        assert_eq!(app.on_key(ctrl_key('c')), Action::None);
        assert!(app.exit_armed.is_some());
        assert_eq!(app.on_key(ctrl_key('c')), Action::Quit);
    }

    #[test]
    fn ctrl_c_closes_an_open_history_search() {
        // While the ctrl+r overlay owns the keyboard, ctrl+c cancels it (CC binds
        // `ctrl+c` to `historySearch:cancel` there) — it must not arm a quit the
        // user did not ask for.
        let mut app = App::new("m", "p");
        type_str(&mut app, "first");
        app.on_key(key(KeyCode::Enter));
        app.on_turn_done(Ok(SummaryReport::default()));
        app.on_key(ctrl_key('r'));
        assert!(app.search_open);
        assert_eq!(app.on_key(ctrl_key('c')), Action::None);
        assert!(!app.search_open, "the overlay closes");
        assert!(app.exit_armed.is_none(), "no quit was armed");
    }

    #[test]
    fn ctrl_c_hint_renders_on_the_footer() {
        // cli 2026-09-28: 「第一次按下后给提示」 — the armed state is announced on
        // the footer row (the same slot as the copy confirmation), never over the
        // conversation pane.
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut app = App::new("m", "p");
        app.push_info(&["hello world"]);
        app.on_key(ctrl_key('c'));
        assert!(app.exit_armed.is_some());
        let mut terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();
        terminal.draw(|f| app.render(f)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let rows: Vec<String> = (0..24)
            .map(|r| (0..120).map(|c| buf[(c, r)].symbol()).collect())
            .collect();
        let footer = rows.last().expect("a footer row");
        // CJK chars are double-width in the test buffer, so match per char (as
        // the other render tests do).
        assert!(
            footer.contains('再') && footer.contains('退') && footer.contains('出'),
            "the pending-exit hint is on the footer: {footer:?}"
        );
        // A copy confirmation would share the slot, but a pending quit outranks it
        // (the hint must never be hidden).
        app.set_copy_note("已复制 3 字符".to_string());
        terminal.draw(|f| app.render(f)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let footer: String = (0..120).map(|c| buf[(c, 23)].symbol()).collect();
        assert!(
            footer.contains('再'),
            "the exit hint wins the slot: {footer:?}"
        );
    }

    #[test]
    fn ctrl_v_pastes_from_the_clipboard() {
        // cli 2026-09-28「默认支持 win 和 linux 的复制粘贴」: a bare `ctrl+v`
        // carries no text, so the app asks the run loop to read the OS clipboard
        // (`Action::Paste`); the text then goes in through [`App::paste`].
        let mut app = App::new("m", "p");
        assert_eq!(app.on_key(ctrl_key('v')), Action::Paste);
        app.paste("from the clipboard");
        assert_eq!(app.editor.text, "from the clipboard");
        // `shift+insert` is the classic X11/WSL paste chord — same action.
        assert_eq!(
            app.on_key(KeyEvent::new(KeyCode::Insert, KeyModifiers::SHIFT)),
            Action::Paste
        );
        // Mid-turn the paste is still requested (the run loop decides) and, since
        // cli 2026-09-29, it now lands in the box (which edits the queued message
        // mid-turn) instead of being silently dropped.
        type_str(&mut app, "go");
        app.on_key(key(KeyCode::Enter));
        assert!(app.is_busy());
        assert_eq!(app.on_key(ctrl_key('v')), Action::Paste);
        app.paste("mid-turn paste");
        assert!(app.editor.text.contains("mid-turn paste"));
    }

    #[test]
    fn esc_cancels_only_when_busy() {
        let mut app = App::new("m", "p");
        assert_eq!(app.on_key(key(KeyCode::Esc)), Action::None);
        type_str(&mut app, "hi");
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.on_key(key(KeyCode::Esc)), Action::Cancel);
        app.on_cancel();
        assert!(!app.is_busy());
        assert!(scrollback_text(&mut app).contains("已取消"));
    }

    #[test]
    fn auto_and_round_require_a_goal_and_dispatch_their_action() {
        let mut app = App::new("m", "p");
        // 无参数 → 报错提示，不派发动作（避免空目标开一个无意义会话）。
        type_str(&mut app, "/auto");
        assert_eq!(app.on_key(key(KeyCode::Enter)), Action::None);
        assert!(scrollback_text(&mut app).contains("用法：/auto"));
        type_str(&mut app, "/round");
        assert_eq!(app.on_key(key(KeyCode::Enter)), Action::None);
        assert!(scrollback_text(&mut app).contains("用法：/round"));
        // 带目标 → 动作携带目标原文（自动模式：Main+Auditor 循环）。
        let mut app = App::new("m", "p");
        type_str(&mut app, "/auto 给 utils 加单元测试");
        assert_eq!(
            app.on_key(key(KeyCode::Enter)),
            Action::Auto("给 utils 加单元测试".to_string())
        );
        // 单轮审计同形。
        let mut app = App::new("m", "p");
        type_str(&mut app, "/round 修一下 lint");
        assert_eq!(
            app.on_key(key(KeyCode::Enter)),
            Action::Round("修一下 lint".to_string())
        );
    }

    #[test]
    fn auto_notice_events_land_in_the_transcript_at_the_right_level() {
        // 自动模式的进度经 `Event::AutoNotice` 进对话流：`error` 走红字错误行，
        // 其余（`message` / `status`）走灰色提示行。
        let mut app = App::new("m", "p");
        app.on_event(&Event::AutoNotice {
            tag: "message".into(),
            text: "▶ 自动模式启动".into(),
            iteration: 0,
        });
        app.on_event(&Event::AutoNotice {
            tag: "status".into(),
            text: "🔍 Auditor·第1轮｜continue=true".into(),
            iteration: 1,
        });
        let text = scrollback_text(&mut app);
        assert!(text.contains("▶ 自动模式启动"), "{text}");
        assert!(text.contains("continue=true"), "{text}");
        app.on_event(&Event::AutoNotice {
            tag: "error".into(),
            text: "❌ 预检错误：x".into(),
            iteration: 0,
        });
        assert!(scrollback_text(&mut app).contains("✖ ❌ 预检错误：x"));
    }

    #[test]
    fn auto_round_shows_in_the_footer_and_activity_line() {
        // cli 2026-10-04「自动模式需要显示轮次，也就是到第几轮了」：轮次由
        // `AutoNotice.iteration` 驱动，钉在 footer 左槽 + 活动行 —— 不再只靠随对话
        // 流滚走的「Main·第N轮 — 开始」。
        let mut app = App::new("m", "p");
        app.begin_auto_turn("修 calc.py");
        app.on_event(&Event::StageStarted {
            stage: AUTO_MAIN_STAGE.into(),
            ts: String::new(),
        });
        // Main 第 1 轮开始：带上轮次。
        app.on_event(&Event::AutoNotice {
            tag: "message".into(),
            text: "🤖 Main·第1轮 — 开始".into(),
            iteration: 1,
        });
        assert_eq!(app.footer_status(), "作答中 · Auto·Main · 第1轮");
        // 第 3 轮：footer 跟着走。
        app.on_event(&Event::AutoNotice {
            tag: "message".into(),
            text: "🤖 Main·第3轮 — 开始".into(),
            iteration: 3,
        });
        assert_eq!(app.footer_status(), "作答中 · Auto·Main · 第3轮");
        // 无轮次的提示（如「▶ 启动」）不覆盖已记录轮次。
        app.on_event(&Event::AutoNotice {
            tag: "status".into(),
            text: "📋 日志：…".into(),
            iteration: 0,
        });
        assert_eq!(app.footer_status(), "作答中 · Auto·Main · 第3轮");
        // 普通回合不再带轮次（`begin_turn` 归零）。
        app.begin_turn("普通提问");
        app.on_event(&Event::StageStarted {
            stage: STAGE_C.into(),
            ts: String::new(),
        });
        assert_eq!(app.footer_status(), "作答中 · 工作阶段");
    }

    #[test]
    fn slash_help_does_not_submit() {
        let mut app = App::new("m", "p");
        type_str(&mut app, "/help");
        assert_eq!(app.on_key(key(KeyCode::Enter)), Action::None);
        assert!(!app.is_busy());
        assert!(scrollback_text(&mut app).contains("/memory"));
    }

    #[test]
    fn slash_clear_wipes_the_panel_but_keeps_the_archive() {
        // 原版对齐 (cli 2026-10-05「按原版设计来」): `/clear` is a **UI-only** wipe —
        // the engine holds no conversation state, and the #1 archive is untouched
        // (the next 组织上下文 still reads it back). The notice must say so, so a
        // cleared panel never reads as "the model forgot".
        let mut app = App::new("m", "p");
        app.push_turn(true, "早先提问");
        app.push_info(&["早先回答"]);
        assert!(!scrollback_text(&mut app).is_empty());

        type_str(&mut app, "/clear");
        assert_eq!(app.on_key(key(KeyCode::Enter)), Action::Clear);
        let sb = scrollback_text(&mut app);
        assert!(!sb.contains("早先提问"), "the panel was wiped: {sb}");
        assert!(sb.contains("对话面板已清空"), "{sb}");
        assert!(sb.contains("历史仍在引擎归档中"), "{sb}");
    }

    #[test]
    fn slash_clear_and_quit_and_stats() {
        let mut app = App::new("m", "p");
        type_str(&mut app, "/clear");
        assert_eq!(app.on_key(key(KeyCode::Enter)), Action::Clear);

        let mut app = App::new("m", "p");
        type_str(&mut app, "/memory");
        assert_eq!(app.on_key(key(KeyCode::Enter)), Action::Stats);

        let mut app = App::new("m", "p");
        type_str(&mut app, "/quit");
        assert_eq!(app.on_key(key(KeyCode::Enter)), Action::Quit);
        assert!(app.should_quit());
    }

    #[test]
    fn unknown_slash_command_reports_error() {
        let mut app = App::new("m", "p");
        type_str(&mut app, "/nope");
        assert_eq!(app.on_key(key(KeyCode::Enter)), Action::None);
        assert!(scrollback_text(&mut app).contains("未知命令"));
    }

    /// Lockstep with `help.json` topic=`commands`: every slash command the app
    /// actually dispatches must appear there — the help page once listed only 5
    /// of the 8 real commands.
    #[test]
    fn help_commands_topic_lists_every_slash_command() {
        let topics = lingmiao_core::config::embedded_help_topics();
        let body = &topics.get("commands").expect("commands help topic").body;
        for (name, _) in COMMANDS {
            assert!(
                body.contains(&format!("`/{name}`")),
                "help/commands omits `/{name}`: {body}"
            );
        }
    }

    // ── §12.2 ④ slash-command completion palette ──────────────────

    #[test]
    fn slash_palette_lists_and_filters_commands() {
        // Typing `/` opens the palette with every command; the typed prefix
        // narrows it; a space (an argument) or a non-slash buffer closes it.
        let mut app = App::new("m", "p");
        type_str(&mut app, "/");
        assert_eq!(app.completion_matches().len(), COMMANDS.len());
        type_str(&mut app, "me");
        let m = app.completion_matches();
        assert_eq!(m.len(), 1);
        assert_eq!(COMMANDS[m[0]].0, "memory");
        // A trailing space begins an argument → the palette yields.
        type_str(&mut app, " ");
        assert!(app.completion_matches().is_empty());

        let mut app = App::new("m", "p");
        type_str(&mut app, "hello");
        assert!(app.completion_matches().is_empty(), "not a command word");
        // While a turn is in flight the palette is suppressed.
        let mut busy = App::new("m", "p");
        type_str(&mut busy, "hi");
        busy.on_key(key(KeyCode::Enter));
        type_str(&mut busy, "/he");
        assert!(busy.is_busy());
        assert!(
            busy.completion_matches().is_empty(),
            "busy suppresses the menu"
        );
    }

    #[test]
    fn slash_palette_tab_completes_the_highlighted_command() {
        let mut app = App::new("m", "p");
        type_str(&mut app, "/me");
        assert_eq!(app.on_key(key(KeyCode::Tab)), Action::None);
        assert_eq!(app.editor.text, "/memory");
        assert!(!app.is_busy(), "Tab only fills the buffer, never submits");
    }

    #[test]
    fn slash_palette_up_down_move_and_clamp_the_selection() {
        let mut app = App::new("m", "p");
        type_str(&mut app, "/");
        assert_eq!(app.on_key(key(KeyCode::Down)), Action::None);
        assert_eq!(app.on_key(key(KeyCode::Down)), Action::None);
        assert_eq!(app.menu_selected, 2);
        app.on_key(key(KeyCode::Up));
        assert_eq!(app.menu_selected, 1);
        for _ in 0..5 {
            app.on_key(key(KeyCode::Up));
        }
        assert_eq!(app.menu_selected, 0, "clamps at the top");
        for _ in 0..20 {
            app.on_key(key(KeyCode::Down));
        }
        assert_eq!(
            app.menu_selected,
            COMMANDS.len() - 1,
            "clamps at the bottom"
        );
        // Typing resets the highlight to the top.
        type_str(&mut app, "x");
        assert_eq!(app.menu_selected, 0);
    }

    #[test]
    fn slash_palette_enter_runs_the_highlighted_command() {
        // A bare `/` + Enter runs the top command (help) — no submit, and no
        // more `未知命令：/`.
        let mut app = App::new("m", "p");
        type_str(&mut app, "/");
        assert_eq!(app.on_key(key(KeyCode::Enter)), Action::None);
        assert!(!app.is_busy());
        assert!(scrollback_text(&mut app).contains("/memory"));
    }

    #[test]
    fn slash_palette_renders_above_the_input_box() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut app = App::new("m", "p");
        type_str(&mut app, "/");
        let mut terminal = Terminal::new(TestBackend::new(100, 40)).unwrap();
        let screen = |t: &Terminal<TestBackend>| -> String {
            t.backend()
                .buffer()
                .content
                .iter()
                .map(|c| c.symbol())
                .collect()
        };
        terminal.draw(|f| app.render(f)).unwrap();
        let text = screen(&terminal);
        assert!(text.contains("/memory"), "palette lists commands: {text:?}");
        // CJK descriptions are double-width (a blank cell follows each char), so
        // match them per char like the other render tests.
        for ch in "记忆库统计".chars() {
            assert!(text.contains(ch), "…with their descriptions (`{ch}`)");
        }
        // Filtering to `he` leaves only /help.
        type_str(&mut app, "he");
        terminal.draw(|f| app.render(f)).unwrap();
        let text = screen(&terminal);
        assert!(text.contains("/help"), "filtered to /help: {text:?}");
        assert!(!text.contains("/memory"), "others filtered out: {text:?}");
    }

    #[test]
    fn only_c_stage_content_streams_to_the_answer() {
        let mut app = App::new("m", "p");
        type_str(&mut app, "hi");
        app.on_key(key(KeyCode::Enter));
        app.on_event(&Event::LlmDelta {
            stage: "组织上下文".into(),
            kind: "content".into(),
            text: "internal".into(),
        });
        app.on_event(&Event::LlmDelta {
            stage: STAGE_C.into(),
            kind: "content".into(),
            text: "你".into(),
        });
        app.on_event(&Event::LlmDelta {
            stage: STAGE_C.into(),
            kind: "content".into(),
            text: "好".into(),
        });
        assert_eq!(app.streaming, "你好");
    }

    #[test]
    fn tool_call_event_adds_card_and_counts() {
        let mut app = App::new("m", "p");
        type_str(&mut app, "hi");
        app.on_key(key(KeyCode::Enter));
        app.on_event(&Event::ToolCalled {
            stage: "工作阶段".into(),
            tool: "read_file".into(),
            args: json!({"path": "src/main.rs"}),
            result_preview: "fn main() {}".into(),
            ms: 12.0,
            error: false,
            call_id: "c1".into(),
            diff: Value::Array(vec![]),
        });
        assert_eq!(app.tool_calls, 1);
        assert_eq!(app.turn_tools, 1);
        let sb = scrollback_text(&mut app);
        assert!(
            sb.contains("read_file"),
            "English tool name (cli 2026-09-24 ⑥)"
        );
        assert!(
            sb.contains("丨作答中"),
            "the stage tag is shown (cli 2026-09-24 ⑦)"
        );
        assert!(sb.contains("src/main.rs"), "primary arg shown");
        assert!(sb.contains("⎿"));
        assert!(sb.contains("12ms"));
    }

    #[test]
    fn tool_result_is_a_middle_grey_above_the_reasoning() {
        // cli 2026-09-24 (「工具调用返回值显示为比思考稍微亮一点点的灰色」): the
        // result body is a *middle* grey ([`TOOL_RESULT`], ANSI 37) — one notch
        // brighter than the reasoning (`TEXT_MUTED`, ANSI 90) but calmer than the
        // bright answer (`TEXT`). Only the timing chrome stays muted.
        let mut app = App::new("m", "p");
        type_str(&mut app, "hi");
        app.on_key(key(KeyCode::Enter));
        app.on_event(&Event::ToolCalled {
            stage: STAGE_C.into(),
            tool: "bash".into(),
            args: json!({"command": "ls -la /tmp"}),
            result_preview: "总计 28852".into(),
            ms: 4.0,
            error: false,
            call_id: "c1".into(),
            diff: Value::Array(vec![]),
        });
        let items = app.take_scrollback();
        // The `⎿` result body line carries the middle grey foreground.
        let body: Vec<&Line> = items
            .iter()
            .filter(|l| l.spans.iter().any(|s| s.content.contains('⎿')))
            .collect();
        assert_eq!(body.len(), 1, "one ⎿ result line: {body:?}");
        let body_fgs: Vec<Option<Color>> = body[0].spans.iter().map(|s| s.style.fg).collect();
        assert!(
            body_fgs.contains(&Some(TOOL_RESULT)),
            "tool result is the middle grey (TOOL_RESULT): {body_fgs:?}"
        );
        assert!(
            !body_fgs.contains(&Some(TEXT_MUTED)),
            "tool result is brighter than the muted reasoning: {body_fgs:?}"
        );
        assert!(
            !body_fgs.contains(&Some(TEXT)),
            "tool result is calmer than the bright answer: {body_fgs:?}"
        );
    }

    #[test]
    fn error_tool_result_stays_red() {
        // A failed tool keeps the error signal even though a normal result is
        // now bright (§12.15) — semantics over brightness.
        let mut app = App::new("m", "p");
        type_str(&mut app, "hi");
        app.on_key(key(KeyCode::Enter));
        app.on_event(&Event::ToolCalled {
            stage: STAGE_C.into(),
            tool: "bash".into(),
            args: json!({"command": "false"}),
            result_preview: "exit=1".into(),
            ms: 2.0,
            error: true,
            call_id: "c1".into(),
            diff: Value::Array(vec![]),
        });
        let items = app.take_scrollback();
        let body = items
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content.contains('⎿')))
            .expect("a ⎿ result line");
        assert!(
            body.spans.iter().any(|s| s.style.fg == Some(ERROR)),
            "failed tool result stays red"
        );
    }

    #[test]
    fn pipeline_stage_tool_cards_carry_stage_tags() {
        // B (检索中) and C (作答中) tools are user-facing; 沉淀阶段's internal
        // bookkeeping tools stay out of the transcript. cli 2026-09-24 ⑦: the cards
        // carry a **low-saturation** stage tag; names show in English (⑥).
        let mut app = App::new("m", "p");
        type_str(&mut app, "hi");
        app.on_key(key(KeyCode::Enter));
        app.on_event(&Event::ToolStarted {
            stage: STAGE_B.into(),
            tool: "search_knowledge".into(),
            args: json!({"query": "x"}),
            call_id: "b1".into(),
        });
        assert!(
            app.running_tool.is_some(),
            "B-stage retrieval drives a running card"
        );
        app.on_event(&Event::ToolCalled {
            stage: STAGE_B.into(),
            tool: "search_knowledge".into(),
            args: json!({"query": "x"}),
            result_preview: "node…".into(),
            ms: 3.0,
            error: false,
            call_id: "b1".into(),
            diff: Value::Array(vec![]),
        });
        let sb = scrollback_text(&mut app);
        assert!(sb.contains("search_knowledge"), "English tool name: {sb}");
        assert!(sb.contains("丨检索中"), "B stage tag shown: {sb}");
        // 沉淀阶段's internal tools do not leak into the transcript.
        app.on_event(&Event::ToolCalled {
            stage: STAGE_SUMMARY.into(),
            tool: "update_knowledge".into(),
            args: json!({"name": "x"}),
            result_preview: "ok".into(),
            ms: 1.0,
            error: false,
            call_id: "s1".into(),
            diff: Value::Array(vec![]),
        });
        assert!(!scrollback_text(&mut app).contains("update_knowledge"));
        // A C-stage tool shows its card with the C tag.
        app.on_event(&Event::ToolCalled {
            stage: STAGE_C.into(),
            tool: "read_file".into(),
            args: json!({"file_path": "src/main.rs"}),
            result_preview: "fn main() {}".into(),
            ms: 1.0,
            error: false,
            call_id: "c1".into(),
            diff: Value::Array(vec![]),
        });
        let sb = scrollback_text(&mut app);
        assert!(sb.contains("read_file"));
        assert!(sb.contains("丨作答中"), "C stage tag shown: {sb}");
    }

    #[test]
    fn reasoning_streams_into_the_thinking_block() {
        let mut app = App::new("m", "p");
        type_str(&mut app, "hi");
        app.on_key(key(KeyCode::Enter));
        // Reasoning arrives first, then a tool call flushes it above the card.
        app.on_event(&Event::LlmDelta {
            stage: STAGE_C.into(),
            kind: "reasoning".into(),
            text: "先想想".into(),
        });
        app.on_event(&Event::LlmDelta {
            stage: STAGE_C.into(),
            kind: "reasoning".into(),
            text: "再作答".into(),
        });
        assert_eq!(app.thinking, "先想想再作答");
        app.on_event(&Event::ToolStarted {
            stage: STAGE_C.into(),
            tool: "bash".into(),
            args: json!({"command": "ls -la"}),
            call_id: "c1".into(),
        });
        assert!(app.thinking.is_empty(), "reasoning flushed on tool start");
        assert!(app.running_tool.is_some());
        let sb = scrollback_text(&mut app);
        assert!(sb.contains("∴ "), "∴ thinking line committed");
        assert!(
            sb.contains("丨作答中"),
            "the thinking block carries its stage tag (cli 2026-09-24 ⑦)"
        );
        assert!(sb.contains("ctrl+o"), "collapsed block hints at ctrl+o");
        assert!(
            sb.contains("先想想再作答"),
            "the (short) reasoning shows as the live tail: {sb:?}"
        );
    }

    #[test]
    fn thinking_collapsed_shows_header_and_live_tail() {
        // §12.7 (cli 2026-09-21): the fold must not read as frozen. A long
        // reasoning keeps only its **last few** lines on screen (grey), with the
        // stage badge + elapsed + ctrl+o hint on the header line.
        let mut app = App::new("m", "p");
        type_str(&mut app, "hi");
        app.on_key(key(KeyCode::Enter));
        // Enough text to wrap to **more** than THINKING_TAIL_LINES segments at the
        // default 100-col width (cli 2026-09-28 raised the tail to 8, so the
        // fixture has to be long enough that the fold is still a strict subset).
        let long = (0..600)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(",");
        app.on_event(&Event::LlmDelta {
            stage: STAGE_C.into(),
            kind: "reasoning".into(),
            text: long,
        });
        app.on_event(&Event::ToolStarted {
            stage: STAGE_C.into(),
            tool: "bash".into(),
            args: json!({"command": "ls"}),
            call_id: "c1".into(),
        });
        let sb = scrollback_text(&mut app);
        let thinking: Vec<&str> = sb
            .lines()
            .filter(|l| l.contains('∴') || l.trim_start().starts_with("1,"))
            .collect();
        assert!(
            thinking[0].contains("ctrl+o"),
            "header keeps the expand hint: {thinking:?}"
        );
        assert!(
            thinking[0].contains("丨作答中"),
            "the thinking header carries its stage tag (cli 2026-09-24 ⑦): {thinking:?}"
        );
        assert!(sb.contains("599"), "the newest reasoning is visible: {sb}");
        // cli 2026-09-28 (「思考改为8」): the reasoning window is **eight** live
        // lines — the one widget that shows more than the three-line tool fold.
        assert_eq!(THINKING_TAIL_LINES, 8, "folded reasoning tail = 8 lines");
        assert!(
            !sb.contains("0,1,2,"),
            "the oldest reasoning is folded away: {sb}"
        );
    }

    #[test]
    fn ctrl_o_toggles_thinking_expansion() {
        // §12.7: `ctrl+o` expands the folded reasoning to the whole thing and
        // re-folds it back to the live tail.
        let mut app = App::new("m", "p");
        type_str(&mut app, "hi");
        app.on_key(key(KeyCode::Enter));
        // Long enough that the folded 8-line tail is a strict subset of the wrap.
        let long = (0..600)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(",");
        app.on_event(&Event::LlmDelta {
            stage: STAGE_C.into(),
            kind: "reasoning".into(),
            text: long,
        });
        app.on_event(&Event::ToolStarted {
            stage: STAGE_C.into(),
            tool: "bash".into(),
            args: json!({"command": "ls"}),
            call_id: "c1".into(),
        });
        let flat = |app: &App| -> String {
            app.history_lines()
                .iter()
                .map(|l| {
                    l.spans
                        .iter()
                        .map(|s| s.content.as_ref())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        assert!(
            !flat(&app).contains("0,1,2,"),
            "folded keeps only the tail: {}",
            flat(&app)
        );
        assert!(
            flat(&app).contains("599"),
            "folded keeps the newest line: {}",
            flat(&app)
        );
        // ctrl+o → expand (works mid-turn too).
        assert_eq!(
            app.on_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL)),
            Action::None
        );
        assert!(app.thinking_expanded);
        assert!(
            flat(&app).contains("0,1,2,"),
            "expanded reveals the whole reasoning: {}",
            flat(&app)
        );
        // ctrl+o again → collapse.
        app.on_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
        assert!(!app.thinking_expanded);
        assert!(
            !flat(&app).contains("0,1,2,"),
            "folded again: {}",
            flat(&app)
        );
    }

    #[test]
    fn running_tool_card_renders_with_spinner_then_clears() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut app = App::new("m", "p");
        type_str(&mut app, "hi");
        app.on_key(key(KeyCode::Enter));
        app.on_event(&Event::ToolStarted {
            stage: STAGE_C.into(),
            tool: "bash".into(),
            args: json!({"command": "sleep 1"}),
            call_id: "c1".into(),
        });
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|f| app.render(f)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("sleep 1"));
        // cli 2026-09-24 ⑥: the running card shows the English tool name.
        assert!(text.contains("bash"), "running card shows the tool name");
        app.on_event(&Event::ToolCalled {
            stage: STAGE_C.into(),
            tool: "bash".into(),
            args: json!({"command": "sleep 1"}),
            result_preview: "done".into(),
            ms: 1000.0,
            error: false,
            call_id: "c1".into(),
            diff: Value::Array(vec![]),
        });
        assert!(app.running_tool.is_none(), "running card cleared on finish");
    }

    /// The rendered frame as one string (CJK double-width cells produce a blank
    /// after each glyph, so render assertions match per char).
    fn frame_text(app: &mut App, w: u16, h: u16) -> String {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|f| app.render(f)).unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    #[test]
    fn wait_heartbeat_is_visible_in_the_transcript() {
        // cli 2026-10-05（「要进界面的，这是核心体验」）: the F 项 poll ran and decided
        // correctly but left **no trace on screen**, so a silent `bash` call still
        // read as a hang («调用 bash 的时候没有轮询，还是等了60多s»). The heartbeat
        // event must therefore render: elapsed + silence + the judge's ruling +
        // how many times the judge was actually asked (the poll's real cost).
        let mut app = App::new("m", "p");
        type_str(&mut app, "hi");
        app.on_key(key(KeyCode::Enter));
        app.on_event(&Event::ToolStarted {
            stage: STAGE_C.into(),
            tool: "bash".into(),
            args: json!({"command": "cargo fmt --all --check"}),
            call_id: "c1".into(),
        });
        app.on_event(&Event::WaitPolled {
            class: "外部命令".into(),
            what: "bash: cargo fmt --all --check".into(),
            elapsed_ms: 40_000,
            silent_ms: 35_000,
            phase: PHASE_SAMPLING.into(),
            detail: String::new(),
        });
        // The TestBackend pads every double-width CJK cell with a blank, so
        // assertions compare with whitespace stripped (same trick as the other
        // render tests).
        let flat = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>();
        let text = frame_text(&mut app, 120, 40);
        let flat_text = flat(&text);
        assert!(
            text.contains(WAIT_MARK),
            "the heartbeat mark shows: {text:?}"
        );
        assert!(flat_text.contains("已等40s"), "elapsed is shown: {text:?}");
        assert!(flat_text.contains("静默35s"), "silence is shown: {text:?}");
        assert!(flat_text.contains("采样中"), "the phase is named: {text:?}");

        // A real ruling replaces it in place and counts the consultation (each
        // one is an extra model call — the cost cli could not see before).
        app.on_event(&Event::WaitPolled {
            class: "外部命令".into(),
            what: "bash: cargo fmt --all --check".into(),
            elapsed_ms: 45_000,
            silent_ms: 40_000,
            phase: PHASE_ASKING.into(),
            detail: String::new(),
        });
        app.on_event(&Event::WaitPolled {
            class: "外部命令".into(),
            what: "bash: cargo fmt --all --check".into(),
            elapsed_ms: 45_000,
            silent_ms: 40_000,
            phase: PHASE_CONTINUE.into(),
            detail: String::new(),
        });
        let text = flat(&frame_text(&mut app, 120, 40));
        assert!(text.contains("继续等"), "the verdict is shown: {text:?}");
        assert!(text.contains("已裁决1次"), "ruling count: {text:?}");
        assert!(!text.contains("已等40s"), "the line is replaced: {text:?}");
        assert!(
            text.contains("已等45s"),
            "replaced by the newer sample: {text:?}"
        );
    }

    #[test]
    fn wait_heartbeat_clears_when_the_wait_or_turn_ends() {
        // The live line must not outlive the wait it describes — a stale
        // 「已等 40s」 under an idle prompt would be worse than no line at all.
        let mut app = App::new("m", "p");
        type_str(&mut app, "hi");
        app.on_key(key(KeyCode::Enter));
        app.on_event(&Event::WaitPolled {
            class: "外部命令".into(),
            what: "bash: sleep 600".into(),
            elapsed_ms: 5_000,
            silent_ms: 5_000,
            phase: PHASE_SAMPLING.into(),
            detail: String::new(),
        });
        assert!(app.wait_notice.is_some());
        // `done` → gone (the tool card carries the settled duration).
        app.on_event(&Event::WaitPolled {
            class: "外部命令".into(),
            what: "bash: sleep 600".into(),
            elapsed_ms: 50_000,
            silent_ms: 5_000,
            phase: PHASE_DONE.into(),
            detail: String::new(),
        });
        assert!(app.wait_notice.is_none(), "done clears the heartbeat");

        // …and a fresh heartbeat never survives the turn ending.
        app.on_event(&Event::WaitPolled {
            class: "外部命令".into(),
            what: "bash: sleep 600".into(),
            elapsed_ms: 9_000,
            silent_ms: 9_000,
            phase: PHASE_CONTINUE.into(),
            detail: String::new(),
        });
        assert!(app.wait_notice.is_some());
        app.on_turn_done(Ok(SummaryReport::failed(String::new())));
        assert!(app.wait_notice.is_none(), "no heartbeat outlives its turn");
    }

    #[test]
    fn footer_status_names_the_wait_while_one_is_polled() {
        // cli 2026-09-28 ④ put the status/stage in the footer's left slot; while a
        // wait is being polled the slot must describe **that** (matching the
        // transcript heartbeat) instead of the stage in general.
        let mut app = App::new("m", "p");
        type_str(&mut app, "hi");
        app.on_key(key(KeyCode::Enter));
        app.on_event(&Event::WaitPolled {
            class: "外部命令".into(),
            what: "bash: cargo build".into(),
            elapsed_ms: 45_000,
            silent_ms: 40_000,
            phase: PHASE_CONTINUE.into(),
            detail: String::new(),
        });
        assert_eq!(app.footer_status(), "外部命令 · 已等 45s");
    }

    #[test]
    fn turn_tail_line_is_cc_shaped() {
        let mut app = App::new("m", "p");
        type_str(&mut app, "hi");
        app.on_key(key(KeyCode::Enter));
        app.on_event(&Event::ToolCalled {
            stage: STAGE_C.into(),
            tool: "grep".into(),
            args: json!({"pattern": "fn main"}),
            result_preview: "1 match".into(),
            ms: 3.0,
            error: false,
            call_id: "c1".into(),
            diff: Value::Array(vec![]),
        });
        app.on_turn_done(Ok(SummaryReport::default()));
        let sb = scrollback_text(&mut app);
        // §14-P2: the tail is CC-shaped — a spinner verb, `for Ns`, and a local
        // `done HH:MM:SS` stamp. cli 2026-09-24: the `· N 个工具` chatter is gone.
        assert!(sb.contains("◜ "), "CC-style tail line");
        // cli 2026-09-24 (「活动行要跟屏幕最左侧对齐」): the tail line is flush at the
        // left margin — no leading indent (it used to carry a 2-space lead).
        let tail = sb
            .lines()
            .find(|l| l.contains(TAIL_MARK))
            .expect("a tail line");
        assert!(
            tail.starts_with(TAIL_MARK),
            "tail starts flush at the left margin: {tail:?}"
        );
        assert!(sb.contains(" for "), "tail names the elapsed as `for Ns`");
        assert!(!sb.contains("个工具"), "no tool-count chatter: {sb}");
        assert!(sb.contains("done "), "tail carries a done timestamp");
        assert!(
            VERBS.iter().any(|v| sb.contains(v)),
            "the tail reports a verb from the CC wheel: {sb}"
        );
    }

    #[test]
    fn tool_primary_args_pick_the_canonical_key() {
        // cli 2026-09-24 ⑥: tool names are shown in English now, so there is no
        // Chinese label left to test — only the primary-arg picker.
        // Canonical key wins.
        assert_eq!(tool_primary_arg("bash", &json!({"command": "ls"})), "ls");
        // Falls back to the first value when the canonical key is absent.
        assert_eq!(
            tool_primary_arg("read_file", &json!({"path": "src/main.rs"})),
            "src/main.rs"
        );
        // Nothing to show → empty (the card still renders bare call parens `()`).
        assert_eq!(tool_primary_arg("edit", &json!({})), "");
        assert_eq!(fmt_ms(4.3), "4.3ms");
        assert_eq!(fmt_ms(120.0), "120ms");
    }

    #[test]
    fn file_mutation_card_paints_the_diff_green_and_red() {
        // cli 2026-09-28 (「参照 CC 实现代码改动时的红绿对比格式显示样式，包括写入
        // 的时候也是」): an `edit` / `write_file` card shows the unified diff under
        // its header — added lines green ([`DIFF_ADD`]), removed lines red
        // ([`DIFF_REMOVE`]), context muted — with CC's `+N -M` stat in the header.
        let mut app = App::new("m", "p");
        type_str(&mut app, "hi");
        app.on_key(key(KeyCode::Enter));
        app.on_event(&Event::ToolCalled {
            stage: STAGE_C.into(),
            tool: "edit".into(),
            args: json!({"file_path": "src/main.rs"}),
            result_preview: "Edited src/main.rs (1 replacement)".into(),
            ms: 3.0,
            error: false,
            call_id: "c1".into(),
            diff: json!([
                {"kind": "hunk", "text": "@@ -1,3 +1,3 @@"},
                {"kind": "context", "text": "fn main() {"},
                {"kind": "remove", "text": "    let x = 1;"},
                {"kind": "add", "text": "    let x = 2;"},
                {"kind": "context", "text": "}"},
            ]),
        });
        let items = app.take_scrollback();
        // The header carries the stat, coloured per sign.
        let header = items
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content.contains("edit")))
            .expect("a tool card header");
        let header_text: String = header.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(header_text.contains("+1"), "adds counted: {header_text:?}");
        assert!(
            header_text.contains("-1"),
            "removals counted: {header_text:?}"
        );
        let header_fgs: Vec<Option<Color>> = header.spans.iter().map(|s| s.style.fg).collect();
        assert!(
            header_fgs.contains(&Some(DIFF_ADD)) && header_fgs.contains(&Some(DIFF_REMOVE)),
            "the +N -M stat is green/red: {header_fgs:?}"
        );
        // The diff body: one coloured line per change, marker-prefixed.
        // Diff rows all start with two spaces then the marker (`+`/`-`/` `/`@`);
        // the `⎿` result body is excluded (it also starts with two spaces).
        let diff_lines: Vec<&Line> = items
            .iter()
            .filter(|l| {
                let t: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
                t.starts_with("  ") && !t.contains("⎿")
            })
            .collect();
        let by_text = |needle: &str| -> Option<&Line> {
            diff_lines
                .iter()
                .copied()
                .find(|l| l.spans.iter().any(|s| s.content.contains(needle)))
        };
        let add = by_text("let x = 2;").expect("the added line");
        assert!(
            add.spans.iter().all(|s| s.style.fg == Some(DIFF_ADD)),
            "the added line is green: {:?}",
            add.spans.iter().map(|s| s.style.fg).collect::<Vec<_>>()
        );
        let remove = by_text("let x = 1;").expect("the removed line");
        assert!(
            remove.spans.iter().all(|s| s.style.fg == Some(DIFF_REMOVE)),
            "the removed line is red: {:?}",
            remove.spans.iter().map(|s| s.style.fg).collect::<Vec<_>>()
        );
        let ctx = by_text("fn main() {").expect("a context line");
        assert!(
            ctx.spans.iter().all(|s| s.style.fg == Some(DIFF_CONTEXT)),
            "context stays muted: {:?}",
            ctx.spans.iter().map(|s| s.style.fg).collect::<Vec<_>>()
        );
        let hunk = by_text("@@").expect("the hunk header");
        assert!(
            hunk.spans.iter().all(|s| s.style.fg == Some(DIFF_CONTEXT)),
            "the @@ header is dim (git-like): {:?}",
            hunk.spans.iter().map(|s| s.style.fg).collect::<Vec<_>>()
        );
    }

    #[test]
    fn write_file_card_also_shows_a_diff() {
        // cli 2026-09-28 (「包括写入的时候也是」): `write_file` gets the same
        // red/green treatment (CC's `Write` renders a patch too), and a brand-new
        // file diffs as pure additions.
        let mut app = App::new("m", "p");
        app.on_event(&Event::ToolCalled {
            stage: STAGE_C.into(),
            tool: "write_file".into(),
            args: json!({"file_path": "new.rs"}),
            result_preview: "Wrote 12 bytes to new.rs".into(),
            ms: 2.0,
            error: false,
            call_id: "c1".into(),
            diff: json!([
                {"kind": "hunk", "text": "@@ -0,0 +1,1 @@"},
                {"kind": "add", "text": "fn main() {}"},
            ]),
        });
        let items = app.take_scrollback();
        let add = items
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content.contains("fn main() {}")))
            .expect("the added line");
        assert!(
            add.spans.iter().all(|s| s.style.fg == Some(DIFF_ADD)),
            "a created file is all green: {:?}",
            add.spans.iter().map(|s| s.style.fg).collect::<Vec<_>>()
        );
        // The header carries the `+1` stat (the scrollback is already drained
        // above, so inspect the captured items, not a second `scrollback_text`).
        let header: String = items
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content.contains("write_file")))
            .expect("a write_file header")
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(
            header.contains("+1"),
            "the stat shows the addition: {header:?}"
        );
    }

    #[test]
    fn non_file_tools_show_no_diff() {
        // Every other tool carries an empty diff → the card is exactly as before
        // (header + `⎿` result), with no stray `+`/`-` lines.
        let mut app = App::new("m", "p");
        app.on_event(&Event::ToolCalled {
            stage: STAGE_C.into(),
            tool: "read_file".into(),
            args: json!({"file_path": "src/main.rs"}),
            result_preview: "fn main() {}".into(),
            ms: 1.0,
            error: false,
            call_id: "c1".into(),
            diff: Value::Array(vec![]),
        });
        let items = app.take_scrollback();
        let mut saw_result = false;
        for l in &items {
            let t: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
            // A diff row is `  <marker><text>`; the only `⎿` row is the result.
            let is_diff = t.starts_with("  +") || t.starts_with("  -") || t.starts_with("  @@");
            assert!(!is_diff, "no diff lines for a read: {t:?}");
            saw_result |= t.contains("⎿");
        }
        assert!(saw_result, "the ⎿ result body is still drawn");
    }

    #[test]
    fn parse_diff_ignores_unknown_kinds_and_bad_payloads() {
        // Forward-compatible: an unknown `kind` (a future diff flavour) is dropped
        // rather than panicking, and an absent payload yields an empty diff.
        assert!(parse_diff(&Value::Null).is_empty());
        assert!(parse_diff(&json!({})).is_empty());
        assert!(parse_diff(&json!([{"kind": "future", "text": "x"}])).is_empty());
        assert!(parse_diff(&json!([{"kind": "add"}])).is_empty());
        let parsed = parse_diff(&json!([
            {"kind": "add", "text": "a"},
            {"kind": "remove", "text": "b"},
            {"kind": "weird", "text": "c"},
        ]));
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].kind, DiffKind::Add);
        assert_eq!(parsed[1].kind, DiffKind::Remove);
        assert_eq!(diff_stat(&parsed), "+1 -1");
    }

    #[test]
    fn tool_card_always_shows_call_parens() {
        // cli 2026-09-24 (「如果没有参数也要有()」): a tool with no surfaced
        // primary argument still renders `()` so the header reads as a function
        // call, not a bare label.
        let mut app = App::new("m", "p");
        app.on_event(&Event::ToolCalled {
            stage: STAGE_C.into(),
            tool: "memory_kinds".into(),
            args: json!({}),
            result_preview: "{}".into(),
            ms: 1.0,
            error: false,
            call_id: "c1".into(),
            diff: Value::Array(vec![]),
        });
        let sb = scrollback_text(&mut app);
        assert!(sb.contains("memory_kinds()"), "bare call parens: {sb}");
    }

    #[test]
    fn stage_result_accumulates_tokens() {
        let mut app = App::new("m", "p");
        type_str(&mut app, "hi");
        app.on_key(key(KeyCode::Enter));
        app.on_event(&Event::StageResultReported {
            stage: "C".into(),
            ok: true,
            data: Value::Null,
            fault_type: String::new(),
            fault_detail: String::new(),
            tokens: json!({"input_tokens": 100, "output_tokens": 40}),
            tool_calls: 0,
            elapsed_ms: 900.0,
        });
        assert_eq!(app.tokens_in, 100);
        assert_eq!(app.tokens_out, 40);
        // T3: a per-stage result no longer sets the session `⏱` (it used to take
        // the slowest single stage). The session total accumulates whole turns.
        assert_eq!(app.elapsed_ms, 0.0);
        app.on_turn_done(Ok(SummaryReport::default()));
        assert!(app.elapsed_ms >= 0.0);
    }

    #[test]
    fn session_elapsed_accumulates_whole_turns() {
        let mut app = App::new("m", "p");
        type_str(&mut app, "hi");
        app.on_key(key(KeyCode::Enter));
        // A per-stage event must *not* move the session clock (T3 regression).
        app.on_event(&Event::StageResultReported {
            stage: "C".into(),
            ok: true,
            data: Value::Null,
            fault_type: String::new(),
            fault_detail: String::new(),
            tokens: json!({"input_tokens": 10, "output_tokens": 5}),
            tool_calls: 0,
            elapsed_ms: 12345.0,
        });
        assert_eq!(
            app.elapsed_ms, 0.0,
            "per-stage time must not become the session total"
        );
        app.on_turn_done(Ok(SummaryReport::default()));
        // The turn's own wall-clock is now banked into the session total.
        assert!(app.elapsed_ms >= 0.0);
    }

    #[test]
    fn tool_loop_preambles_are_flushed_not_concatenated() {
        // T14: the C-stage tool loop re-invokes the model after each tool result.
        // Each iteration's content used to append into the same `streaming`
        // buffer, so the final reply echoed the earlier preambles.
        let mut app = App::new("m", "p");
        type_str(&mut app, "hi");
        app.on_key(key(KeyCode::Enter));
        // Iteration 1: a preamble → tool call.
        app.on_event(&Event::LlmDelta {
            stage: STAGE_C.into(),
            kind: "content".into(),
            text: "我先查一下".into(),
        });
        app.on_event(&Event::ToolStarted {
            stage: STAGE_C.into(),
            tool: "search_memory".into(),
            args: json!({"query": "x"}),
            call_id: "c1".into(),
        });
        // The preamble is committed as its own `●` block, buffer cleared.
        assert!(app.streaming.is_empty(), "preamble flushed on tool start");
        // Iteration 2: the real answer.
        app.on_event(&Event::LlmDelta {
            stage: STAGE_C.into(),
            kind: "content".into(),
            text: "答案是 42".into(),
        });
        app.on_turn_done(Ok(SummaryReport::default()));
        let sb = scrollback_text(&mut app);
        assert!(sb.contains("我先查一下"), "preamble kept");
        assert!(sb.contains("答案是 42"), "final answer kept");
        // Each appears exactly once — no echo/duplication.
        assert_eq!(sb.matches("我先查一下").count(), 1);
        assert_eq!(sb.matches("答案是 42").count(), 1);
    }

    #[test]
    fn markdown_reply_is_rendered() {
        // T12: `**bold**` markdown becomes a styled span (bold), not literal `**`.
        let lines = markdown_lines("这是 **加粗** 与 `code`");
        let joined: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(joined.contains("加粗"), "bold text present");
        assert!(!joined.contains("**"), "markdown markers consumed");
        assert!(joined.contains("code"), "inline code present");
        let bold = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .any(|s| s.content.contains("加粗") && s.style.add_modifier.contains(Modifier::BOLD));
        assert!(bold, "bold span carries the BOLD modifier");
    }

    #[test]
    fn markdown_headings_and_fences_are_consumed() {
        // T12 fix for cli「markdown没渲染」: heading `#`s and code fences must
        // never reach the screen as raw markdown.
        let lines = markdown_lines("### 标题\n\n```rust\nfn main() {}\n```\n");
        let joined: String = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !joined.contains('#'),
            "heading markers stripped: {joined:?}"
        );
        assert!(!joined.contains("```"), "code fences stripped: {joined:?}");
        assert!(joined.contains("标题"), "heading text kept");
        assert!(joined.contains("fn main()"), "code body kept");
        // The heading line renders bold + accent (no `#`, no cyan block).
        assert!(
            lines.iter().any(|l| l.spans.iter().any(
                |s| s.content.contains("标题") && s.style.add_modifier.contains(Modifier::BOLD)
            )),
            "heading becomes bold text"
        );
    }

    #[test]
    fn stage_badges_label_conversation_items() {
        assert_eq!(stage_badge(STAGE_B), "丨检索中");
        assert_eq!(stage_badge(STAGE_C), "丨作答中");
        assert_eq!(stage_badge(STAGE_SUMMARY), "丨沉淀中");
        assert_eq!(stage_badge("unknown"), "");
    }

    #[test]
    fn auto_roles_carry_their_own_badge_and_footer_slot() {
        // cli 2026-10-04「各控件也没有显示阶段和角色」：角色的 `stage` 是引擎侧
        // `RoleKind::stage_name()` 的 `Auto·Main` / `Auto·Auditor`。以前它落进
        // `stage_badge` 的空串兜底（控件一个标签都没有）、又落进 `human_stage`
        // 的 `other => other`（左下角成了 `Auto·Main · Auto·Main`）。
        assert_eq!(stage_badge(AUTO_MAIN_STAGE), "丨Main");
        assert_eq!(stage_badge(AUTO_AUDITOR_STAGE), "丨Auditor");
        // 动词归位「作答中」（角色跑的就是工作阶段式循环），角色名留在后半。
        assert_eq!(human_stage(AUTO_MAIN_STAGE), "作答中");
        assert_eq!(human_stage(AUTO_AUDITOR_STAGE), "作答中");
        // `/session` 的每阶段计时不再永远停在 `— · — · —`。
        assert_eq!(stage_index(AUTO_MAIN_STAGE), Some(1));
        assert_eq!(stage_index(AUTO_AUDITOR_STAGE), Some(1));
        // 这两个常量必须与引擎侧的 `RoleKind::stage_name()` 一字不差 —— 引擎改名
        // 而这里不改，标签会静默退回空串（正是本轮这个 bug 的形态）。
        assert_eq!(
            lingmiao_engine::autonomous::RoleKind::Main.stage_name(),
            AUTO_MAIN_STAGE
        );
        assert_eq!(
            lingmiao_engine::autonomous::RoleKind::Auditor.stage_name(),
            AUTO_AUDITOR_STAGE
        );
    }

    #[test]
    fn a_trailing_space_is_visible_in_the_input_box() {
        // cli 2026-10-04「按完空格后空格进去了但是不显示，输入下一个字符才显示」：
        // 输入框的渲染行与光标列都走 wrap_cjk，而它会丢掉断行处的尾随空格 —— 所以
        // 刚打的空格既不可见、光标也不前进，直到下一个字符把空格顶到行中间。
        let mut app = App::new("m", "p");
        type_str(&mut app, "hello ");
        assert_eq!(
            app.editor_visual_lines(40),
            vec!["hello "],
            "尾随空格必须保留"
        );
        // 光标在**末尾**（空格的右边）时列 = 6；若换回 `wrap_cjk`（丢尾空），
        // `wrapped` 只剩 "hello" → 列停在 5 —— 光标落后于已输入的文字，正是
        // 「输入下一个字符才显示」的另一半症状。
        assert_eq!(visual_cursor_row_col("hello ", 6, 40), (0, 6));
        assert_eq!(visual_cursor_row_col("hello ", 5, 40), (0, 5));
        // 对话区（wrap_cjk）的丢尾空行为不变：一段以空格结尾的正文末尾不该留空列。
        assert_eq!(wrap_cjk("hello ", 40), vec!["hello"]);
    }

    #[test]
    fn auto_role_answer_streams_into_the_transcript() {
        // cli 2026-10-04「各控件也没有显示阶段和角色」：`content` 只认 STAGE_C，
        // 而角色正文的 stage 是 `Auto·Main`/`Auto·Auditor` → 角色的**回答整段
        // 不显示**。现在两个角色走同一支，正文落成带 `丨Main` 标签的 `●` 块。
        let mut app = App::new("m", "p");
        app.begin_auto_turn("修 calc.py");
        app.on_event(&Event::StageStarted {
            stage: AUTO_MAIN_STAGE.into(),
            ts: "2026-10-04T10:00:00.000+08:00".into(),
        });
        app.on_event(&Event::LlmDelta {
            stage: AUTO_MAIN_STAGE.into(),
            kind: "content".into(),
            text: "已修复 add 函数".into(),
        });
        assert!(
            app.streaming.contains("已修复 add 函数"),
            "正文进入流式缓冲"
        );
        app.on_event(&Event::ToolStarted {
            stage: AUTO_MAIN_STAGE.into(),
            tool: "read_file".into(),
            args: serde_json::json!({"path": "calc.py"}),
            call_id: "c1".into(),
        });
        let sb: String = app
            .take_scrollback()
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(sb.contains("已修复 add 函数"), "角色回答落盘：{sb:?}");
        assert!(sb.contains("丨Main"), "角色正文带角色标签：{sb:?}");
    }

    #[test]
    fn activity_line_names_the_running_auto_role() {
        // 活动行（transcript 里那条橙色 `⠋ … · esc 中断`）在自动模式时必须显示
        // **角色**，否则两条不同角色的工作在屏幕上无从区分。
        let mut app = App::new("m", "p");
        app.begin_auto_turn("修 calc.py");
        app.on_event(&Event::StageStarted {
            stage: AUTO_AUDITOR_STAGE.into(),
            ts: "2026-10-04T10:00:00.000+08:00".into(),
        });
        assert_eq!(app.activity_status(), "Auto·Auditor");
        // 普通回合不受影响，仍是去黑话动词。
        app.on_event(&Event::StageStarted {
            stage: "工作阶段".into(),
            ts: "2026-10-04T10:00:00.000+08:00".into(),
        });
        assert_eq!(app.activity_status(), "作答中");
    }

    #[test]
    fn queued_message_waits_until_the_auto_session_ends() {
        // cli 2026-10-04「插入只能是在不影响原内容的前提下」：自动模式在跑时，
        // 新输入排进队列（`enqueue_turn`，**不** begin_turn），由 run loop 在
        // auto 结束（auto_rx 分支）后才 `dequeue_turn` 发出。
        let mut app = App::new("m", "p");
        app.begin_auto_turn("修 calc.py");
        app.enqueue_turn("顺手也把注释补上".into());
        assert_eq!(app.queued_len(), 1, "排队而不是清空重开");
        // `enqueue_turn` 不动任何流式缓冲：自动模式正在输出的内容一个字都不丢。
        app.on_event(&Event::LlmDelta {
            stage: AUTO_MAIN_STAGE.into(),
            kind: "content".into(),
            text: "正在写".into(),
        });
        app.enqueue_turn("再改一处".into());
        assert!(app.streaming.contains("正在写"), "插入不影响原内容");
        assert_eq!(app.queued_len(), 2);
        // 空文本不排队。
        app.enqueue_turn("   ".into());
        assert_eq!(app.queued_len(), 2);
    }

    #[test]
    fn footer_shows_status_and_role_while_an_auto_role_runs() {
        let mut app = App::new("m", "p");
        app.begin_auto_turn("修 calc.py");
        app.on_event(&Event::StageStarted {
            stage: AUTO_MAIN_STAGE.into(),
            ts: "2026-10-04T10:00:00.000+08:00".into(),
        });
        // 状态（作答中）+ 角色（Auto·Main）各归其位，不再是同一个词抄两遍。
        assert_eq!(app.footer_status(), "作答中 · Auto·Main");
        app.on_event(&Event::StageStarted {
            stage: AUTO_AUDITOR_STAGE.into(),
            ts: "2026-10-04T10:00:00.000+08:00".into(),
        });
        assert_eq!(app.footer_status(), "作答中 · Auto·Auditor");
    }

    #[test]
    fn b_stage_reasoning_and_answer_are_tagged_with_their_stage() {
        // cli 2026-09-24 ⑦: B reasoning is surfaced and the C reply is shown, each
        // carrying its (low-saturation) stage tag.
        let mut app = App::new("m", "p");
        type_str(&mut app, "hi");
        app.on_key(key(KeyCode::Enter));
        app.on_event(&Event::LlmDelta {
            stage: STAGE_B.into(),
            kind: "reasoning".into(),
            text: "先检索记忆".into(),
        });
        app.on_event(&Event::ToolStarted {
            stage: STAGE_B.into(),
            tool: "search_knowledge".into(),
            args: json!({"query": "x"}),
            call_id: "b1".into(),
        });
        app.on_event(&Event::LlmDelta {
            stage: STAGE_C.into(),
            kind: "content".into(),
            text: "答案是 42".into(),
        });
        app.on_turn_done(Ok(SummaryReport::default()));
        let sb = scrollback_text(&mut app);
        assert!(sb.contains("丨检索中"), "B tag shown: {sb}");
        assert!(sb.contains("丨作答中"), "C tag shown: {sb}");
        assert!(
            sb.contains("ctrl+o"),
            "collapsed B-reasoning block hints at ctrl+o: {sb:?}"
        );
        assert!(sb.contains("答案是 42"));
    }

    #[test]
    fn display_width_counts_cjk_as_two_columns() {
        // T5: CJK chars are 2 columns wide (drives the footer/segment layout).
        assert_eq!(display_width("规则底座"), 8);
        assert_eq!(display_width("ab规则"), 6);
    }

    #[test]
    fn ctx_line_shows_after_a_turn() {
        // cli 2026-09-24: the green composition band is gone; a short, right-
        // aligned context total sits at the pane's bottom-right (where CC prints
        // `N tokens`). It collapses before the first injection and appears once a
        // turn reports usage. cli 2026-09-27: that line now shows **one number** —
        // the current turn's provider-measured input tokens.
        let mut app = App::new("m", "p");
        assert_eq!(app.ctx_line_height(), 0, "no line before the first turn");
        app.on_event(&Event::ContextUsage {
            stage: STAGE_C.into(),
            sections: json!([
                {"name": "规则底座", "chars": 8000},
                {"name": "lock", "chars": 220},
                {"name": "当前提问", "chars": 20},
            ]),
            total_chars: 8240,
        });
        app.on_event(&Event::StageResultReported {
            stage: STAGE_C.into(),
            ok: true,
            data: Value::Null,
            fault_type: String::new(),
            fault_detail: String::new(),
            tokens: json!({"input_tokens": 2048, "output_tokens": 5}),
            tool_calls: 0,
            elapsed_ms: 1.0,
        });
        assert_eq!(app.ctx_line_height(), 1, "the line shows after a turn");
        // A single compacted number (2.0K), muted — no total/remainder split.
        let s = app.ctx_summary();
        assert_eq!(s, "2.0K", "one compacted number: {s:?}");
        assert!(display_width(&s) <= 20, "≤20 cols: {s:?}");
        assert!(
            !s.contains('总') && !s.contains('余'),
            "no total/rest labels: {s:?}"
        );
        assert_eq!(
            app.ctx_line_color(),
            TEXT_MUTED,
            "low usage is muted grey, not green"
        );
    }

    #[test]
    fn activity_line_shows_while_busy() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut app = App::new("m", "p");
        type_str(&mut app, "hi");
        app.on_key(key(KeyCode::Enter));
        app.on_event(&Event::StageStarted {
            stage: "组织上下文".into(),
            ts: String::new(),
        });
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|f| app.render(f)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        // cli 2026-09-24: the activity line is CC-shaped — a spinner verb, the
        // running status label, elapsed seconds and the esc hint.
        assert!(text.contains("esc"));
        assert!(
            VERBS.iter().any(|v| text.contains(v)),
            "activity line rotates a CC verb: {text:?}"
        );
        // The running status is a single plain-language label (now ≤4 chars,
        // cli 2026-09-24 ①); CJK is double-width, so match per char.
        assert!(
            text.contains('检') && text.contains('索'),
            "the running status label is shown: {text:?}"
        );
    }

    #[test]
    fn done_commits_answer_and_summary() {
        let mut app = App::new("m", "p");
        type_str(&mut app, "hi");
        app.on_key(key(KeyCode::Enter));
        app.on_event(&Event::LlmDelta {
            stage: STAGE_C.into(),
            kind: "content".into(),
            text: "完整回复".into(),
        });
        app.on_turn_done(Ok(SummaryReport {
            obs_type: "task".into(),
            audit_grade: "good".into(),
            quality_grade: "good".into(),
            ..Default::default()
        }));
        assert!(!app.is_busy());
        let sb = scrollback_text(&mut app);
        assert!(sb.contains("完整回复"));
        // cli 2026-09-24: the `· 总结 type=… audit=…` debug line is gone.
        assert!(!sb.contains("audit=good"));
        assert!(!sb.contains("· 总结"));
    }

    #[test]
    fn failed_turn_surfaces_error() {
        let mut app = App::new("m", "p");
        type_str(&mut app, "hi");
        app.on_key(key(KeyCode::Enter));
        app.on_turn_done(Err(LingmiaoError::llm("boom", 0, 1)));
        assert!(!app.is_busy());
        assert!(app.error.is_some());
        assert!(scrollback_text(&mut app).contains("boom"));
    }

    #[test]
    fn stage_result_fault_surfaces_error() {
        let mut app = App::new("m", "p");
        type_str(&mut app, "hi");
        app.on_key(key(KeyCode::Enter));
        app.on_event(&Event::StageResultReported {
            stage: "C".into(),
            ok: false,
            data: Value::Null,
            fault_type: "llm".into(),
            fault_detail: "timeout".into(),
            tokens: json!(null),
            tool_calls: 0,
            elapsed_ms: 1.0,
        });
        assert_eq!(app.error.as_deref(), Some("timeout"));
    }

    #[test]
    fn stage_fault_is_visible_even_when_the_turn_ends_ok() {
        // cli 2026-09-27: the 2026-09-27「无回答」bug — a C-stage HTTP 400 set
        // `self.error` (nothing rendered it) and the pipeline still finished
        // `Ok`, so the user saw a bare `◜ done` with no answer and no reason.
        // A stage fault must surface in the transcript regardless.
        let mut app = App::new("m", "p");
        type_str(&mut app, "hi");
        app.on_key(key(KeyCode::Enter));
        app.on_event(&Event::StageResultReported {
            stage: STAGE_C.into(),
            ok: false,
            data: Value::Null,
            fault_type: "llm".into(),
            fault_detail: "HTTP 400 [请求无效(400)] —— 请求参数非法\n原因: too many tokens".into(),
            tokens: Value::Null,
            tool_calls: 0,
            elapsed_ms: 1.0,
        });
        // The pipeline ran the consolidation stage and finished `Ok(report)` with
        // an empty `report.error` — the failure must still be visible.
        app.on_turn_done(Ok(SummaryReport::default()));
        let sb = scrollback_text(&mut app);
        assert!(sb.contains('✖'), "the error marker is shown: {sb}");
        assert!(sb.contains("HTTP 400"), "the fault detail is shown: {sb}");
        assert!(
            sb.contains("too many tokens"),
            "the full panel is shown: {sb}"
        );
        // The turn still ends with its tail (it did finish).
        assert!(sb.contains(TAIL_MARK), "the turn tail still prints: {sb}");
    }

    #[test]
    fn wrap_counts_cjk_as_two_columns() {
        let lines = wrap_cjk("一二三四五", 4);
        assert_eq!(
            lines,
            vec!["一二".to_string(), "三四".to_string(), "五".to_string()]
        );
    }

    #[test]
    fn wrapped_line_preserves_per_span_styles() {
        // cli 2026-09-24 (「有个工具调用的标题是全绿」): an over-wide multi-colour
        // line — a tool card header (green `●` + bright name + grey ms) — must keep
        // **each span's** colour when it wraps. The old `wrap_line` re-emitted every
        // segment with the first span's style, so an 80-col terminal turned the
        // whole title green.
        let line = Line::from(vec![
            Span::styled("● ", Style::default().fg(SUCCESS)),
            Span::styled(
                "search_observations(a very long argument list goes here) ",
                Style::default().fg(TEXT),
            ),
            Span::styled("0.9ms", Style::default().fg(TEXT_MUTED)),
        ]);
        let out = wrap_line(&line, 24);
        assert!(out.len() > 1, "the line must wrap: {out:?}");
        let fgs: Vec<Option<Color>> = out
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.style.fg)
            .collect();
        assert!(
            fgs.contains(&Some(SUCCESS)),
            "the ● dot keeps SUCCESS: {fgs:?}"
        );
        assert!(fgs.contains(&Some(TEXT)), "the name keeps TEXT: {fgs:?}");
        assert!(
            fgs.contains(&Some(TEXT_MUTED)),
            "the ms keeps TEXT_MUTED: {fgs:?}"
        );
        // No data is lost: reassembling the segments reproduces the original text
        // (trailing spaces at a break are dropped).
        let joined: String = out
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("");
        assert!(
            joined.contains("search_observations") && joined.contains("0.9ms"),
            "the wrapped text survives: {joined:?}"
        );
    }

    #[test]
    fn wrap_line_keeps_cjk_width_exact_for_the_reverse_bar() {
        // §12.9: the reverse-video user-turn bar pads by `display_width(seg)`, so
        // every segment must report its width exactly (CJK = 2 columns).
        let widths: Vec<usize> = wrap_cjk("你好世界", 4)
            .iter()
            .map(|s| display_width(s))
            .collect();
        assert_eq!(widths, vec![4, 4]);
        // And no segment ever exceeds the requested width.
        let long = "这是一段中英混排 hello world 的长文本，用于测试分行与禁则。";
        for w in [6usize, 8, 10, 12, 20] {
            for seg in wrap_cjk(long, w) {
                assert!(display_width(&seg) <= w, "w={w} seg={seg:?}");
            }
        }
    }

    #[test]
    fn list_hanging_detects_markers() {
        assert_eq!(list_hanging("- item"), Some(2));
        assert_eq!(list_hanging("  - item"), Some(4));
        assert_eq!(list_hanging("1. item"), Some(3));
        assert_eq!(list_hanging("12. item"), Some(4));
        assert_eq!(list_hanging("not a list"), None);
        assert_eq!(list_hanging("-nospace"), None);
        assert_eq!(list_hanging("普通正文"), None);
        // The *rendered* form: a `• ` bullet, after the `● [阶段] ` chrome.
        assert_eq!(list_hanging("• item"), Some(2));
        let rendered = "● 丨作答中 • Rust 的所有权";
        let h = list_hanging(rendered).expect("rendered list marker detected");
        assert_eq!(h, display_width("● 丨作答中 • "));
    }

    #[test]
    fn tool_card_header_never_hangs_on_a_dash_in_its_argument() {
        // cli 2026-09-28 ②: the old `list_hanging` scanned the **whole** line, so a
        // running bash card whose argument contains `--- ` (`echo "--- DISPLAY"`)
        // matched the `- ` of the third hyphen and hung **70** columns — leaving a
        // `width − 70` body, i.e. 8 cells on a 78-column pane: one short word per
        // line (「一个词一行、竖着排」, docs/粘贴的图像 (6).png). A marker now counts
        // only where a list item can begin (line start after the render chrome).
        let live =
            "⠋ 丨作答中 bash(which xdotool scrot Xvfb ffmpeg xterm 2>&1; echo \"--- DISPLAY\")  4s";
        let committed = "● 丨作答中 bash(which xdotool scrot Xvfb ffmpeg xterm 2>&1; echo \"--- DISPLAY\")  1.5ms";
        assert_eq!(list_hanging(live), None, "a card header is not a list item");
        assert_eq!(list_hanging(committed), None);
        for text in [live, committed] {
            let out = wrap_line(&Line::from(text), 78);
            assert!(out.len() > 1, "an 81-column header wraps: {out:?}");
            let lines: Vec<String> = out
                .iter()
                .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
                .collect();
            for s in &lines {
                assert!(display_width(s) <= 78, "fits the pane: {s:?}");
            }
            // The first line uses the **whole** pane — no 70-column hang, so the
            // body is not squeezed into a word-per-line column.
            assert!(display_width(&lines[0]) > 40, "no bogus hang: {lines:?}");
        }
        // … while a real list item still hangs under its text.
        assert_eq!(
            list_hanging("● 丨作答中 • Rust 的所有权"),
            Some(display_width("● 丨作答中 • "))
        );
    }

    #[test]
    fn wrap_line_hangs_a_rendered_list_item() {
        // The real shape: `● [阶段] • <long item>` must wrap with its
        // continuation lines indented under the item text (not flush left).
        let line =
            Line::from("● 丨作答中 • 这是一段很长的列表项内容用来验证悬挂缩进是否与条目文本对齐");
        let out = wrap_line(&line, 24);
        assert!(out.len() > 1, "must wrap: {out:?}");
        let hang = display_width("● 丨作答中 • ");
        for l in out.iter().skip(1) {
            let s = l.spans[0].content.to_string();
            assert_eq!(display_width(&s) - display_width(s.trim_start()), hang);
        }
    }

    #[test]
    fn wrap_line_indents_list_continuation() {
        // §12.9 列表分行: a long list item wraps with its continuation lines
        // aligned under the item text (hanging indent), not flush at the margin.
        let line = Line::from("- 这是一段较长的列表项内容需要折行处理");
        let out = wrap_line(&line, 12);
        assert!(out.len() > 1, "must wrap: {out:?}");
        let first = out[0].spans[0].content.to_string();
        assert!(first.starts_with("- "), "marker kept: {first:?}");
        for l in out.iter().skip(1) {
            let s = l.spans[0].content.to_string();
            assert!(s.starts_with("  "), "continuation indented: {s:?}");
        }
        // A short line is passed through untouched (styles preserved).
        let short = Line::from("短");
        assert_eq!(wrap_line(&short, 12).len(), 1);
    }

    #[test]
    fn token_and_stage_humanising() {
        assert_eq!(fmt_tokens(999), "999");
        assert_eq!(fmt_tokens(1500), "1.5K");
        assert_eq!(human_stage("组织上下文"), "检索中");
        assert_eq!(human_stage("工作阶段"), "作答中");
        assert_eq!(human_stage("沉淀阶段"), "沉淀中");
    }

    #[test]
    fn shift_enter_inserts_newline_without_submitting() {
        let mut app = App::new("m", "p");
        type_str(&mut app, "line1");
        assert_eq!(
            app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)),
            Action::None
        );
        type_str(&mut app, "line2");
        assert_eq!(app.editor.text, "line1\nline2");
        assert!(!app.is_busy());
        // Ctrl+J also inserts a newline (works on terminals without ⇧-Enter).
        assert_eq!(
            app.on_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL)),
            Action::None
        );
        type_str(&mut app, "line3");
        assert_eq!(app.editor.text, "line1\nline2\nline3");
        // A literal `j` (no modifier) is still typed normally.
        type_str(&mut app, "j");
        assert_eq!(app.editor.text, "line1\nline2\nline3j");
        // Plain Enter submits the whole multi-line buffer.
        assert_eq!(
            app.on_key(key(KeyCode::Enter)),
            Action::Submit("line1\nline2\nline3j".into())
        );
    }

    #[test]
    fn progress_tracks_stage_events() {
        let mut app = App::new("m", "p");
        type_str(&mut app, "hi");
        app.on_key(key(KeyCode::Enter));
        assert!(
            app.progress
                .iter()
                .all(|p| p.status == StageStatus::Pending)
        );
        app.on_event(&Event::StageStarted {
            stage: "组织上下文".into(),
            ts: String::new(),
        });
        assert_eq!(app.progress[0].status, StageStatus::Running);
        app.on_event(&Event::StageResultReported {
            stage: "组织上下文".into(),
            ok: true,
            data: Value::Null,
            fault_type: String::new(),
            fault_detail: String::new(),
            tokens: Value::Null,
            tool_calls: 0,
            elapsed_ms: 300.0,
        });
        assert_eq!(app.progress[0].status, StageStatus::Done);
        assert_eq!(app.progress[0].elapsed_ms, 300.0);
        app.on_event(&Event::StageStarted {
            stage: "工作阶段".into(),
            ts: String::new(),
        });
        assert_eq!(app.progress[1].status, StageStatus::Running);
        assert_eq!(app.progress[2].status, StageStatus::Pending);
    }

    #[test]
    fn context_usage_event_populates_segments() {
        let mut app = App::new("m", "p");
        app.on_event(&Event::ContextUsage {
            stage: STAGE_C.into(),
            sections: json!([
                {"name": "规则底座", "chars": 100},
                {"name": "当前提问", "chars": 20},
            ]),
            total_chars: 120,
        });
        assert_eq!(app.ctx_sections.len(), 2);
        assert_eq!(app.ctx_sections[0], ("规则底座".to_string(), 100));
        assert_eq!(app.ctx_total_chars, 120);
        app.on_event(&Event::StageResultReported {
            stage: STAGE_C.into(),
            ok: true,
            data: Value::Null,
            fault_type: String::new(),
            fault_detail: String::new(),
            tokens: json!({"input_tokens": 600, "output_tokens": 10}),
            tool_calls: 0,
            elapsed_ms: 5.0,
        });
        // The line uses the *last injection's* provider tokens, not the sum.
        assert_eq!(app.ctx_tokens, 600);
        // cli 2026-09-27: the bottom-right line is the single current-turn number
        // (600 → "600"), ≤20 columns, with no % / no window figure.
        let text = app.ctx_summary();
        assert_eq!(text, "600", "one number: {text:?}");
        assert!(display_width(&text) <= 20, "≤20 cols: {text:?}");
        assert!(!text.contains('%'), "no % sign: {text:?}");
        assert!(!text.contains('/'), "no window figure: {text:?}");
    }

    #[test]
    fn ctx_line_follows_every_llm_round_trip_of_any_stage() {
        // cli 2026-09-28 (「上下文显示还是不对，你显示当前 llm 的输入 tokens 就行」):
        // the bottom-right figure is **the LLM input of the call that just
        // happened** — every stage, every round-trip:
        //   * 组织上下文's call updates it (the old code ignored every non-工作阶段),
        //   * each 工作阶段 tool-loop round-trip updates it (the old code waited for
        //     the stage to *end*, so the whole turn — 组织上下文's call and the
        //     工作阶段 stream — displayed the **previous** turn's number),
        //   * 沉淀阶段's call updates it too.
        let mut app = App::new("m", "p");
        let trip = |stage: &str, input: u64| Event::LlmResponse {
            stage: stage.into(),
            content: String::new(),
            tool_calls: Vec::new(),
            tool_count: 0,
            tools: Vec::new(),
            reasoning_content: String::new(),
            finish_reason: "stop".into(),
            usage: lingmiao_core::events::Usage {
                input_tokens: input,
                output_tokens: 3,
                total_tokens: input + 3,
                tool_calls: vec![],
            },
        };
        // 组织上下文's call — live, before any 工作阶段 stage result exists.
        app.on_event(&trip("组织上下文", 9_871));
        assert_eq!(app.ctx_summary(), "9.9K", "B's own call is shown live");
        // C's first round-trip…
        app.on_event(&trip(STAGE_C, 9_346));
        assert_eq!(app.ctx_summary(), "9.3K", "C's first round-trip");
        // …then a later round-trip of the same tool loop replaces it.
        app.on_event(&trip(STAGE_C, 9_544));
        assert_eq!(app.ctx_summary(), "9.5K", "the newest round-trip wins");
        // The consolidation stage's call is a real LLM input too.
        app.on_event(&trip(STAGE_SUMMARY, 8_864));
        assert_eq!(app.ctx_summary(), "8.9K", "沉淀阶段's call counts too");
        // A round-trip with no reported usage (a provider that omits it) leaves
        // the line alone rather than blanking it.
        app.on_event(&trip(STAGE_C, 0));
        assert_eq!(app.ctx_summary(), "8.9K", "no usage → unchanged");
        // The session meter is untouched by per-round events (it accumulates the
        // stages' own totals, as before).
        assert_eq!(app.tokens_in, 0);
    }

    #[test]
    fn ctx_line_prefers_the_last_round_trip_over_the_stage_sum() {
        // cli 2026-09-28: the staged `tokens.input_tokens` is a **sum** over every
        // LLM round-trip of the C-stage tool loop (image base64 re-sends push it
        // to absurd figures like 31.5M). The context line must show the *current
        // turn's* real injection size instead — the engine now carries it as
        // `input_tokens_last`; the sum is only a fallback for older payloads.
        let mut app = App::new("m", "p");
        app.on_event(&Event::StageResultReported {
            stage: STAGE_C.into(),
            ok: true,
            data: Value::Null,
            fault_type: String::new(),
            fault_detail: String::new(),
            tokens: json!({
                "input_tokens": 31_528_364_u64,
                "input_tokens_last": 12_800_u64,
                "output_tokens": 10,
            }),
            tool_calls: 0,
            elapsed_ms: 5.0,
        });
        assert_eq!(
            app.ctx_tokens, 12_800,
            "the line shows the last round-trip, not the 31.5M stage sum"
        );
        assert_eq!(app.ctx_summary(), "12.8K");
        // The session meter still accumulates the stage's real total.
        assert_eq!(app.tokens_in, 31_528_364);
    }

    #[test]
    fn ctx_summary_stays_within_20_columns_and_has_no_green() {
        // cli 2026-09-24: the green full-width context band is replaced by a
        // short (≤20 col) right-aligned figure. cli 2026-09-27: that figure is a
        // **single number** — the current turn's context (no `总/余` split), and
        // the colour is always the muted grey, because the system no longer
        // assumes any context-window size (no ratio, no threshold alarm).
        let mut app = App::new("m", "p");
        app.on_event(&Event::ContextUsage {
            stage: STAGE_C.into(),
            sections: json!([
                {"name": "规则底座", "chars": 8000},
                {"name": "lock", "chars": 220},
                {"name": "知识记忆", "chars": 5000},
                {"name": "最近对话", "chars": 2000},
                {"name": "当前提问", "chars": 20},
            ]),
            total_chars: 15240,
        });
        app.on_event(&Event::StageResultReported {
            stage: STAGE_C.into(),
            ok: true,
            data: Value::Null,
            fault_type: String::new(),
            fault_detail: String::new(),
            tokens: json!({"input_tokens": 20480, "output_tokens": 10}),
            tool_calls: 0,
            elapsed_ms: 1.0,
        });
        let s = app.ctx_summary();
        assert_eq!(s, "20.5K", "one compacted number: {s:?}");
        assert!(display_width(&s) <= 20, "≤20 columns: {s:?}");
        assert!(!s.contains('%') && !s.contains('/'), "no % / window: {s:?}");
        assert!(!s.contains('总') && !s.contains('余'), "no labels: {s:?}");
        // No window assumption → the line is always the muted grey, whatever the
        // token count (no green, no amber, no red).
        assert_eq!(
            app.ctx_line_color(),
            TEXT_MUTED,
            "always muted, no window alarm"
        );
    }

    #[test]
    fn no_persistent_header_identity_at_footer_bottom_right() {
        // cli 2026-09-24: CC has no header — the brand identity is printed once
        // at startup (banner, in the conversation) and kept at the footer's
        // bottom-right.
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut app = App::new("deepseek-v4-pro", "deepseek");
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        let rows = |t: &Terminal<TestBackend>| -> Vec<String> {
            let buf = t.backend().buffer();
            (0..buf.area.height)
                .map(|r| {
                    (0..buf.area.width)
                        .map(|c| buf[(c, r)].symbol())
                        .collect::<String>()
                })
                .collect()
        };
        terminal.draw(|f| app.render(f)).unwrap();
        let before = rows(&terminal);
        // Before the banner the top row is the (empty) conversation, not a brand.
        assert!(
            !before[0].contains('灵'),
            "no header band at the top: {:?}",
            before[0]
        );
        // The identity lives on the footer's last line as the right-hand block.
        // (CJK chars occupy two cells, so match them per char.)
        let last = &before[before.len() - 1];
        assert!(
            last.contains('灵') && last.contains('妙'),
            "identity at footer L3: {last:?}"
        );
        assert!(
            last.contains("deepseek-v4-pro"),
            "identity shows the model: {last:?}"
        );
        // cli 2026-09-24 ①: the identity stays at the footer's bottom-right; the
        // shortcut-hint row is gone.
        // The startup banner drops the brand block into the conversation.
        app.push_startup_banner();
        terminal.draw(|f| app.render(f)).unwrap();
        let after = rows(&terminal);
        assert!(
            after[0].contains('灵') && after[0].contains("lingmiao"),
            "banner tops the conversation: {:?}",
            after[0]
        );
    }

    #[test]
    fn header_uses_theme_tokens() {
        // §14-P0: the chrome draws from the `theme::` token layer, never from
        // ad-hoc `Color::` literals — guards against a regression back to
        // scattered colours in the widget layer. Each asserted token has a
        // distinct value, so their presence proves several roles are wired.
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut app = App::new("deepseek-v4-pro", "deepseek");
        app.push_startup_banner();
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|f| app.render(f)).unwrap();
        let buf = terminal.backend().buffer().clone();
        // Every foreground in the frame must be a theme token (or the terminal
        // default) — this is the guard against re-introducing `Color::` literals.
        let allowed = [
            ACCENT,
            LOGO,
            STAGE_TAG,
            crate::theme::CODE_FG,
            crate::theme::LINK,
            crate::theme::SURFACE,
            BORDER,
            crate::theme::RULE,
            TEXT,
            TEXT_MUTED,
            THINKING_KW,
            TOOL_RESULT,
            SUCCESS,
            RUNNING,
            ERROR,
            SELECTION_FG,
            SELECTION_BG,
            DIFF_ADD,
            DIFF_REMOVE,
            DIFF_CONTEXT,
            Color::Reset,
        ];
        for c in &buf.content {
            assert!(
                allowed.contains(&c.fg),
                "chrome used a non-token colour {:?} for {:?}",
                c.fg,
                c.symbol()
            );
        }
        // These tokens carry distinct values, so their presence proves the
        // corresponding role is actually drawn through the token layer.
        let fgs: Vec<Color> = buf.content.iter().map(|c| c.fg).collect();
        assert!(fgs.contains(&ACCENT), "the ❯ prompt draws ACCENT");
        assert!(
            fgs.contains(&LOGO),
            "the startup banner's brand mark draws LOGO"
        );
        // cli 2026-09-28 (「把 logo 颜色改成跟活动行一样的颜色」): the brand mark
        // now wears the **activity line's** ink — the token is aliased to `ACCENT`
        // (kept as its own name so it can diverge later). The live progress line
        // draws `ACCENT` directly, so the two read as one brand.
        assert_eq!(
            LOGO, ACCENT,
            "the logo colour is the activity line's colour"
        );
        assert!(
            fgs.contains(&ACCENT),
            "…which the activity line / ❯ prompt draw"
        );
        assert!(fgs.contains(&TEXT_MUTED), "muted chrome draws TEXT_MUTED");
        assert!(fgs.contains(&BORDER), "input-box border draws BORDER");
        // (cli 2026-09-24: the footer's `● 就绪` bullet was dropped with the status
        // row, so SUCCESS no longer appears in this idle frame — it still draws the
        // tool-call dot, covered by the tool-card tests.)
    }

    #[test]
    fn render_single_column_into_a_test_backend() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut app = App::new("deepseek-v4-pro", "deepseek");
        app.set_whiteboard(vec![
            "[1] TUI 完善讨论".into(),
            "当前：撤侧栏走纯 CC 单栏".into(),
        ]);
        // No persistent header (cli 2026-09-24): the identity is a startup banner
        // in the conversation + the footer's bottom-right.
        app.push_startup_banner();
        type_str(&mut app, "hi");
        app.on_key(key(KeyCode::Enter));
        app.on_event(&Event::ContextUsage {
            stage: STAGE_C.into(),
            sections: json!([
                {"name": "规则底座", "chars": 8000},
                {"name": "知识记忆", "chars": 5000},
                {"name": "最近对话", "chars": 2000},
                {"name": "工具能力", "chars": 1000},
                {"name": "当前提问", "chars": 20},
            ]),
            total_chars: 16020,
        });
        app.on_event(&Event::StageResultReported {
            stage: "工作阶段".into(),
            ok: true,
            data: Value::Null,
            fault_type: String::new(),
            fault_detail: String::new(),
            tokens: json!({"input_tokens": 20480, "output_tokens": 100}),
            tool_calls: 0,
            elapsed_ms: 1200.0,
        });
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|f| app.render(f)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let text: String = buf
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<Vec<_>>()
            .join("");
        // §12.1 (cli 2026-09-24): no permanent header — the brand identity is a
        // startup banner in the conversation (brand + slug per char) + the footer
        // bottom-right.
        for ch in brand::NAME.chars() {
            assert!(text.contains(ch), "banner brand `{ch}`");
        }
        for ch in brand::SLUG.chars() {
            assert!(text.contains(ch), "banner slug `{ch}`");
        }
        assert!(text.contains("deepseek-v4-pro"), "identity shows the model");
        // cli 2026-09-24: the short context figure lives at the pane's
        // bottom-right (where CC prints `N tokens`) — not the old green band.
        // cli 2026-09-27: it is now a **single** number (the current turn's
        // context), so the old `总` / `余` labels are gone.
        assert!(
            text.contains("20.5K"),
            "the context line shows the current-turn tokens: {text:?}"
        );
        assert!(
            !text.contains('总') && !text.contains('余'),
            "no total/rest labels anymore: {text:?}"
        );
        for ch in "系统".chars() {
            assert!(
                !text.contains(ch),
                "the old 系统/lock band is gone (`{ch}`)"
            );
        }
        assert!(
            !text.contains("128.0K"),
            "the context-window figure is gone: {text:?}"
        );
        assert!(!text.contains('%'), "no % sign anywhere in the layout");
        // cli 2026-09-28 (「小白板显示在与上下文数字同一行，左对齐」): the
        // whiteboard's current page now *is* on screen — a one-line summary on the
        // ctx row's left slot. The old per-block band stays gone (the `撤侧栏` block
        // titles and the old `系统` band are still absent), but the note itself is
        // expected. (CJK glyphs are double-width in the test buffer, so every wide
        // char is followed by a blank cell — compare on the space-stripped text.)
        let flat: String = text.chars().filter(|c| *c != ' ').collect();
        assert!(
            flat.contains("TUI完善讨论"),
            "the whiteboard page head is on the ctx row: {text:?}"
        );
        assert!(
            flat.contains("撤侧栏走纯CC单栏"),
            "its first content line rides along: {text:?}"
        );
        // The sidebar block titles are gone (撤侧栏 §12.4).
        assert!(!text.contains("本轮处理"));
        assert!(!text.contains("导航"));
    }

    #[test]
    fn page_keys_scroll_the_conversation() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut app = App::new("m", "p");
        for i in 0..60 {
            app.push_info(&[&format!("line {i}")]);
        }
        // Render once so `max_scroll` is established from the real pane height.
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|f| app.render(f)).unwrap();
        assert!(app.max_scroll > 0, "60 lines must overflow a 40-row pane");
        let max = app.max_scroll;
        assert_eq!(app.scroll, 0);
        assert_eq!(app.on_key(key(KeyCode::PageUp)), Action::None);
        assert_eq!(app.scroll, 10.min(max));
        // §②: Home/End now move the input cursor, so paging to the ends is
        // bound to Ctrl+Home / Ctrl+End.
        assert_eq!(
            app.on_key(KeyEvent::new(KeyCode::Home, KeyModifiers::CONTROL)),
            Action::None
        );
        assert_eq!(app.scroll, max);
        assert_eq!(app.on_key(key(KeyCode::PageDown)), Action::None);
        assert_eq!(app.scroll, max.saturating_sub(10));
        assert_eq!(
            app.on_key(KeyEvent::new(KeyCode::End, KeyModifiers::CONTROL)),
            Action::None
        );
        assert_eq!(app.scroll, 0);
        // Submitting a fresh turn jumps back to the newest line.
        app.on_key(KeyEvent::new(KeyCode::Home, KeyModifiers::CONTROL));
        assert!(app.scroll > 0);
        type_str(&mut app, "hi");
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.scroll, 0);
    }

    #[test]
    fn scrolled_viewport_stays_put_as_new_lines_stream_in() {
        // cli 2026-09-27「滚轮翻动后位置保持」: once the user scrolls up, freshly
        // streamed lines appended below must not push the viewport down.
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut app = App::new("m", "p");
        for i in 0..60 {
            app.push_info(&[&format!("line {i}")]);
        }
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|f| app.render(f)).unwrap();
        assert!(app.max_scroll > 0, "60 lines must overflow the pane");
        app.scroll_up(10);
        assert_eq!(app.scroll, 10);
        let pinned = app.anchor.expect("scrolling up pins the viewport top");
        // A new line streams in below while the user reads the history above.
        app.push_info(&["streamed line"]);
        terminal.draw(|f| app.render(f)).unwrap();
        // The pinned viewport top has not moved…
        assert_eq!(app.anchor, Some(pinned));
        // …and the bottom grew, so the view is now further from it.
        assert!(app.scroll > 10, "scroll grew past 10: {}", app.scroll);
    }

    #[test]
    fn context_command_is_gone() {
        // cli 2026-09-21: the `/context` command was dropped — its content no
        // longer matched the footer and it carried the misleading 128K window.
        assert!(
            !COMMANDS.iter().any(|(n, _)| *n == "context"),
            "/context must be out of the command palette"
        );
        let mut app = App::new("m", "p");
        type_str(&mut app, "/context");
        assert_eq!(app.on_key(key(KeyCode::Enter)), Action::None);
        assert!(!app.is_busy(), "/context must not submit a turn");
        assert!(
            scrollback_text(&mut app).contains("未知命令"),
            "/context is now an unknown command"
        );
        // `/help` no longer advertises it.
        let mut app = App::new("m", "p");
        type_str(&mut app, "/help");
        app.on_key(key(KeyCode::Enter));
        assert!(!scrollback_text(&mut app).contains("/context"));
    }

    #[test]
    fn paste_inserts_multiline_and_normalises_crlf() {
        let mut app = App::new("m", "p");
        app.paste("a\r\nb\nc");
        assert_eq!(app.editor.text, "a\nb\nc");
        // cli 2026-09-29「输入框粘贴不好用」: a *trailing* newline (and the lone
        // `\r\n` an empty Windows clipboard yields) must not leave a blank line.
        let mut app = App::new("m", "p");
        app.paste("one line\n");
        assert_eq!(app.editor.text, "one line");
        app.paste("\r\n");
        assert_eq!(
            app.editor.text, "one line",
            "an empty paste inserts nothing"
        );
        // Interior blank lines are the user's text — kept.
        let mut app = App::new("m", "p");
        app.paste("a\n\nb");
        assert_eq!(app.editor.text, "a\n\nb");
        // **Mid-turn paste is no longer dropped** (cli 2026-09-29): while a turn
        // is in flight the box edits the queued message, so `ctrl+v` must land
        // there like typing does instead of silently doing nothing.
        let mut app = App::new("m", "p");
        type_str(&mut app, "x");
        app.on_key(key(KeyCode::Enter));
        assert!(app.is_busy());
        app.paste("pasted mid-turn");
        assert_eq!(app.editor.text, "pasted mid-turn");
        // … and Enter then queues it, exactly as a typed line does.
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.queued_len(), 1);
    }

    #[test]
    fn idle_input_shows_a_tip_but_yields_to_typing_or_busy() {
        // §14-P3: in the empty, idle input box a tip takes the placeholder slot;
        // it yields to typing and to a turn in flight (the priority rule).
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut app = App::new("m", "p");
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();

        // Idle + empty → the opening tip (which names /help) is shown.
        terminal.draw(|f| app.render(f)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("/help"), "idle input shows a tip: {text:?}");

        // Typing yields the box back to the editor — no tip.
        type_str(&mut app, "x");
        terminal.draw(|f| app.render(f)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(!text.contains("/help"), "typing hides the tip: {text:?}");
        assert!(text.contains('x'), "the typed text is shown: {text:?}");

        // A turn in flight also hides it (busy outranks idle).
        app.editor.clear();
        type_str(&mut app, "hi");
        app.on_key(key(KeyCode::Enter));
        assert!(app.is_busy());
        terminal.draw(|f| app.render(f)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(
            !text.contains("/help"),
            "a busy turn hides the tip: {text:?}"
        );
    }

    #[test]
    fn idle_tips_only_name_real_commands() {
        // §14-P3: every tip names a command or key this TUI actually has. A tip
        // pointing at a removed command is a dead end — `/context` was dropped
        // (cli 2026-09-21) yet its tip lingered in the rotation.
        for tip in crate::motion::TIPS {
            for word in tip.split('/').skip(1) {
                let name: String = word
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric())
                    .collect();
                if name.is_empty() {
                    continue;
                }
                assert!(
                    COMMANDS.iter().any(|(n, _)| *n == name),
                    "tip `{tip}` names an unknown command `/{name}`"
                );
            }
        }
    }

    #[test]
    fn compact_layout_renders_on_a_short_terminal() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut app = App::new("deepseek-v4-pro", "deepseek");
        app.push_startup_banner();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| app.render(f)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let text: String = buf
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<Vec<_>>()
            .join("");
        for ch in brand::NAME.chars() {
            assert!(text.contains(ch), "banner must show `{ch}`");
        }
        assert!(text.contains("deepseek"), "identity shows the model");
        // cli 2026-09-24: the whiteboard band is gone; a short terminal still
        // renders the banner + identity without it.
        assert!(
            !text.contains('白'),
            "no whiteboard band on a short terminal"
        );
    }

    // ── §② input editor ──────────────────────────────────────────

    #[test]
    fn whiteboard_summary_is_one_line_and_skips_the_placeholder() {
        // cli 2026-09-28: the whiteboard now shows as the ctx row's left slot, so
        // it must be a **single line** (head + first content line) and must render
        // as nothing at all when the page is empty — a blank left slot is quieter
        // than the placeholder text.
        let mut app = App::new("m", "p");
        assert_eq!(app.whiteboard_summary(), "", "placeholder → no note");
        app.set_whiteboard(vec![]);
        assert_eq!(app.whiteboard_summary(), "", "empty page → no note");
        app.set_whiteboard(vec!["[3] 部署-示例".into(), String::new(), "已完成".into()]);
        assert_eq!(
            app.whiteboard_summary(),
            "[3] 部署-示例 · 已完成",
            "head + first *non-empty* content line, one line only"
        );
        // A page with a head but no content stays just the head.
        app.set_whiteboard(vec!["[4] 标题".into()]);
        assert_eq!(app.whiteboard_summary(), "[4] 标题");
    }

    #[test]
    fn whiteboard_summary_skips_a_header_echo_line() {
        // cli 2026-09-28: a body line that only repeats the page head must not
        // eat the single-line summary — the two shapes seen in real pages are a
        // copied-back `--- Page N: … ---` divider and the bare title.
        let mut app = App::new("m", "p");
        app.set_whiteboard(vec![
            "[24] 400超限根因+v0.12.18".into(),
            "--- Page 24: 400超限根因+v0.12.18 ---".into(),
            String::new(),
            "状态：✅ 已修复".into(),
        ]);
        assert_eq!(
            app.whiteboard_summary(),
            "[24] 400超限根因+v0.12.18 · 状态：✅ 已修复",
            "the divider is skipped, the real line is shown"
        );
        // A page whose only body line is the title falls back to the head alone.
        app.set_whiteboard(vec!["[6] 修复崩溃".into(), "修复崩溃".into()]);
        assert_eq!(app.whiteboard_summary(), "[6] 修复崩溃");
    }

    #[test]
    fn footer_shows_status_and_stage_then_stands_by() {
        // cli 2026-09-28 ④: the previously empty left slot carries the running
        // status + stage, and `待命` when idle.
        let mut app = App::new("m", "p");
        assert_eq!(app.footer_status(), "待命");
        app.on_event(&Event::StageStarted {
            stage: "工作阶段".into(),
            ts: "2026-09-28T10:00:00.000+08:00".into(),
        });
        assert_eq!(app.footer_status(), "作答中 · 工作阶段");
        app.on_event(&Event::StageStarted {
            stage: "组织上下文".into(),
            ts: "2026-09-28T10:00:00.000+08:00".into(),
        });
        assert_eq!(app.footer_status(), "检索中 · 组织上下文");
    }

    #[test]
    fn handoff_notice_reports_what_went_to_the_next_stage_and_how() {
        // cli 2026-09-30: after 组织上下文 finishes, the transcript gets a short
        // notice saying what it selected, that it went to 工作阶段, and the exact
        // format it takes there. Every value comes off the event (engine-owned),
        // so the wording can never drift from the real injection.
        let mut app = App::new("m", "p");
        type_str(&mut app, "hi");
        app.on_key(key(KeyCode::Enter));
        app.on_event(&Event::ContextHandoff {
            from: STAGE_B.into(),
            to: STAGE_C.into(),
            observations: 4,
            nodes: 1,
            recent_turns: 3,
            chars: 1876,
            prefix: "## 上下文".into(),
            role: "system".into(),
            position: "工作阶段 system 尾部".into(),
        });
        let sb = scrollback_text(&mut app);
        // The fact half: who handed what to whom.
        assert!(sb.contains("组织上下文 › 工作阶段"), "{sb}");
        assert!(sb.contains("4 条观测"), "{sb}");
        assert!(sb.contains("1 个知识节点"), "{sb}");
        assert!(sb.contains("3 轮对话"), "{sb}");
        assert!(sb.contains("共 1876 字"), "{sb}");
        // The format half: the literal prefix, role and position. 2026-10-05 B 项:
        // 上下文块进的是 **system 提示词**（原版 context.py:335），不再是一份 user 消息。
        assert!(sb.contains("## 上下文"), "the real prefix: {sb}");
        assert!(sb.contains("system 提示注入"), "{sb}");
        assert!(sb.contains("工作阶段 system 尾部"), "{sb}");
        // It is chrome, not a reply: the `▸` mark, never a `●` bullet.
        assert!(sb.contains(HANDOFF_MARK), "{sb}");
    }

    #[test]
    fn handoff_notice_says_so_when_nothing_was_selected() {
        // A stage that picks nothing (the 文件操作 quick path) must not claim a
        // handoff of an empty block — the notice states it outright and adds no
        // format line (the injection is skipped in that case).
        let mut app = App::new("m", "p");
        app.on_event(&Event::ContextHandoff {
            from: STAGE_B.into(),
            to: STAGE_C.into(),
            observations: 0,
            nodes: 0,
            recent_turns: 0,
            chars: 0,
            prefix: "## 上下文".into(),
            role: "system".into(),
            position: "工作阶段 system 尾部".into(),
        });
        let sb = scrollback_text(&mut app);
        assert!(sb.contains("未选中上下文"), "{sb}");
        assert!(
            !sb.contains("以「"),
            "no format line for an empty block: {sb}"
        );
    }

    #[test]
    fn handoff_notice_is_white_not_grey() {
        // cli 2026-09-30 (「A阶段结束后显示那段话用白色」): both notice lines draw
        // the **primary** foreground (TEXT = white), not the muted chrome grey
        // (TEXT_MUTED). The reasoning `∴` body and the fold hint stay grey — only
        // the stage-handoff statement is lifted, because it is the one line that
        // reports what crossed from 组织上下文 into 工作阶段.
        let mut app = App::new("m", "p");
        app.on_event(&Event::ContextHandoff {
            from: STAGE_B.into(),
            to: STAGE_C.into(),
            observations: 2,
            nodes: 1,
            recent_turns: 0,
            chars: 512,
            prefix: "## 上下文".into(),
            role: "system".into(),
            position: "工作阶段 system 尾部".into(),
        });
        let lines = app.history_lines();
        let notice: Vec<&Line<'static>> = lines
            .iter()
            .filter(|l| {
                let t: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
                t.contains(HANDOFF_MARK) || t.contains(HANDOFF_FORMAT_MARK)
            })
            .collect();
        assert_eq!(notice.len(), 2, "both notice lines present: {lines:?}");
        for line in notice {
            for span in &line.spans {
                let t: &str = span.content.as_ref();
                if t.trim().is_empty() {
                    continue;
                }
                assert_eq!(
                    span.style.fg,
                    Some(TEXT),
                    "handoff notice is white, not grey: {:?}",
                    line.spans
                        .iter()
                        .map(|s| (s.content.to_string(), s.style.fg))
                        .collect::<Vec<_>>()
                );
            }
        }
    }

    #[test]
    fn narrow_pane_wraps_committed_and_live_blocks_at_the_same_width() {
        // cli 2026-09-28 ②: on a very narrow pane the committed half of a turn was
        // wrapped at `pad.width` while the **live** half used `max(8, pad.width)`
        // — two different widths in the same transcript. Both now share one
        // `wrap_width`, so a card's settled and streaming parts agree (the
        // 「一个词一行、竖着排」garble on a narrow pane).
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut app = App::new("m", "p");
        // A committed card …
        app.on_event(&Event::ToolCalled {
            stage: STAGE_C.into(),
            tool: "bash".into(),
            args: serde_json::json!({"command": "which xdotool scrot Xvfb ffmpeg xterm"}),
            call_id: "c1".into(),
            result_preview: "/usr/bin/xdotool\n/usr/bin/scrot\n/usr/bin/Xvfb".into(),
            ms: 1.0,
            error: false,
            diff: serde_json::json!([]),
        });
        // … plus a live one in flight.
        app.begin_turn("跑一下 which");
        app.on_event(&Event::ToolStarted {
            stage: STAGE_C.into(),
            tool: "bash".into(),
            args: serde_json::json!({"command": "which xdotool scrot Xvfb ffmpeg xterm 2>&1"}),
            call_id: "c2".into(),
        });
        let mut terminal = Terminal::new(TestBackend::new(24, 20)).unwrap();
        terminal.draw(|f| app.render(f)).unwrap();
        // The pane insets one column each side; the cache is wrapped at the same
        // width the live block is.
        assert_eq!(
            app.cache_width,
            24 - 2,
            "committed and live wrap at one width"
        );
        let buf = terminal.backend().buffer().clone();
        let rows: Vec<String> = (0..20)
            .map(|r| (0..24).map(|c| buf[(c, r)].symbol()).collect())
            .collect();
        // The committed card's `● bash(…)` header stays on one row with a real
        // word on it — not one token per row.
        let header = rows
            .iter()
            .find(|r| r.contains("bash("))
            .unwrap_or_else(|| panic!("the card header is on screen: {rows:?}"));
        assert!(
            header.contains("which"),
            "the header keeps a whole word next to the name: {header:?}"
        );
    }

    #[test]
    fn ctx_line_keeps_a_gap_before_the_token_number() {
        // cli 2026-09-28 (「留一点gap，不要完全顶着上下文那里」): the whiteboard
        // note is truncated so `CTX_LINE_GAP` columns always separate it from the
        // right-aligned number — it must never butt up against it.
        assert_eq!(App::ctx_left_budget(80, 6), 80 - 6 - CTX_LINE_GAP);
        // A number wider than the row leaves no room (never a negative budget).
        assert_eq!(App::ctx_left_budget(4, 6), 0);
        assert_eq!(App::ctx_left_budget(10, 6), 2);
    }

    #[test]
    fn ctx_row_shows_whiteboard_left_and_tokens_right() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut app = App::new("m", "p");
        app.set_whiteboard(vec!["[5] 小白板与 ctx 同行".into(), "左对齐 + gap".into()]);
        app.on_event(&Event::LlmResponse {
            stage: STAGE_C.into(),
            content: String::new(),
            tool_calls: Vec::new(),
            tool_count: 0,
            tools: Vec::new(),
            reasoning_content: String::new(),
            finish_reason: "stop".into(),
            usage: lingmiao_core::events::Usage {
                input_tokens: 20_480,
                output_tokens: 1,
                total_tokens: 20_481,
                tool_calls: vec![],
            },
        });
        assert_eq!(
            app.ctx_line_height(),
            1,
            "a note alone keeps the row visible"
        );
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal.draw(|f| app.render(f)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let rows: Vec<String> = (0..24)
            .map(|r| (0..100).map(|c| buf[(c, r)].symbol()).collect())
            .collect();
        // The ctx row is the one carrying the token figure.
        let row = rows
            .iter()
            .find(|r| r.contains("20.5K"))
            .expect("a ctx row with the token figure")
            .clone();
        // CJK glyphs are double-width in the test buffer (each followed by a blank
        // cell), so compare on the space-stripped row.
        let flat: String = row.chars().filter(|c| *c != ' ').collect();
        assert!(
            flat.contains("[5]小白板与ctx同行"),
            "note on the left: {row:?}"
        );
        assert!(flat.contains("左对齐+gap"), "its content line too: {row:?}");
        // Left slot starts in column 1 (the row's 1-column inset).
        assert_eq!(
            row.chars().position(|c| c == '['),
            Some(1),
            "the note is left-aligned: {row:?}"
        );
        // …and a gutter separates the note from the number (the number never
        // immediately follows the note's last glyph).
        let note_end = row.find("gap").expect("the note text") + "gap".len();
        let num_at = row.find("20.5K").expect("the number");
        assert!(num_at > note_end, "number sits right of the note: {row:?}");
        assert!(
            row[note_end..num_at].chars().all(|c| c == ' '),
            "only blank columns between note and number: {row:?}"
        );
    }

    #[test]
    fn ctx_row_hidden_when_there_is_neither_note_nor_tokens() {
        // Before the first turn with an empty whiteboard the row collapses, so the
        // conversation keeps the line (unchanged behaviour for the token half).
        let app = App::new("m", "p");
        assert_eq!(app.ctx_line_height(), 0);
    }

    #[test]
    fn left_right_move_cursor_and_insert_mid_line() {
        let mut app = App::new("m", "p");
        type_str(&mut app, "ab");
        app.on_key(key(KeyCode::Left)); // now between `a` and `b`
        app.on_key(key(KeyCode::Char('X')));
        assert_eq!(app.editor.text, "aXb", "insert lands mid-line");
        assert_eq!(app.editor.cursor, 2);
        app.on_key(key(KeyCode::Right));
        app.on_key(key(KeyCode::Char('Y')));
        assert_eq!(app.editor.text, "aXbY");
        // Clamps at the ends.
        for _ in 0..10 {
            app.on_key(key(KeyCode::Left));
        }
        assert_eq!(app.editor.cursor, 0);
        for _ in 0..10 {
            app.on_key(key(KeyCode::Right));
        }
        assert_eq!(app.editor.cursor, app.editor.char_len());
    }

    #[test]
    fn backspace_deletes_before_cursor() {
        let mut app = App::new("m", "p");
        type_str(&mut app, "abc");
        app.on_key(key(KeyCode::Left)); // before `c`
        app.on_key(key(KeyCode::Backspace));
        assert_eq!(app.editor.text, "ac", "removes the char before the cursor");
        assert_eq!(app.editor.cursor, 1);
        // Delete removes the char *at* the cursor instead (cursor stays).
        app.on_key(key(KeyCode::Home));
        app.on_key(key(KeyCode::Delete));
        assert_eq!(app.editor.text, "c");
        assert_eq!(app.editor.cursor, 0);
    }

    #[test]
    fn home_end_move_within_line() {
        let mut app = App::new("m", "p");
        type_str(&mut app, "abc");
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)); // newline
        type_str(&mut app, "def");
        // Cursor is at the end of line 2; Home → start of *that* line.
        app.on_key(key(KeyCode::Home));
        assert_eq!(app.editor.cursor, 4, "Home → start of the current line");
        app.on_key(key(KeyCode::End));
        assert_eq!(app.editor.cursor, 7, "End → end of the current line");
        // Ctrl+A / Ctrl+E are the emacs equivalents (CC supports both).
        app.on_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
        assert_eq!(app.editor.cursor, 4);
        app.on_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL));
        assert_eq!(app.editor.cursor, 7);
    }

    #[test]
    fn ctrl_u_kills_to_line_start() {
        let mut app = App::new("m", "p");
        type_str(&mut app, "hello world");
        for _ in 0..5 {
            app.on_key(key(KeyCode::Left)); // before `world`
        }
        app.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(app.editor.text, "world");
        assert_eq!(app.editor.cursor, 0);
    }

    #[test]
    fn ctrl_w_deletes_word() {
        let mut app = App::new("m", "p");
        type_str(&mut app, "foo bar");
        app.on_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(app.editor.text, "foo ");
        app.on_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(app.editor.text, "");
    }

    #[test]
    fn undo_restores_previous_text() {
        let mut app = App::new("m", "p");
        type_str(&mut app, "abc");
        app.on_key(key(KeyCode::Char('d')));
        assert_eq!(app.editor.text, "abcd");
        app.on_key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL));
        assert_eq!(app.editor.text, "abc", "Ctrl+Z undoes the last edit");
        assert_eq!(app.editor.cursor, 3);
    }

    #[test]
    fn up_down_moves_within_multiline_then_browses_history() {
        // Multi-line buffer: ↑/↓ move between lines, not history (§②).
        let mut app = App::new("m", "p");
        type_str(&mut app, "one");
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT));
        type_str(&mut app, "two");
        assert_eq!(app.editor.line_col().0, 1);
        app.on_key(key(KeyCode::Up));
        assert_eq!(app.editor.line_col().0, 0, "↑ moves up a line");
        app.on_key(key(KeyCode::Down));
        assert_eq!(app.editor.line_col().0, 1, "↓ moves back down");
        // Single-line draft: ↑ browses submitted history instead.
        let mut fresh = App::new("m", "p");
        type_str(&mut fresh, "cmd1");
        fresh.on_key(key(KeyCode::Enter));
        fresh.on_turn_done(Ok(SummaryReport::default()));
        assert_eq!(fresh.on_key(key(KeyCode::Up)), Action::None);
        assert_eq!(fresh.editor.text, "cmd1", "single-line ↑ recalls history");
    }

    #[test]
    fn input_cursor_is_placed_mid_line() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        // Pure placement: row/col from the char-index cursor (CJK = 2 columns).
        assert_eq!(
            visual_cursor_row_col("aXb", 2, 100),
            (0, 2),
            "cursor mid `aXb`"
        );
        assert_eq!(
            visual_cursor_row_col("ab\ncd", 5, 100),
            (1, 2),
            "line 2, after `cd`"
        );
        assert_eq!(
            visual_cursor_row_col("你好", 1, 100),
            (0, 2),
            "one CJK char = 2 columns"
        );

        // … and the renderer puts the terminal cursor there (measure the shift,
        // which is layout-independent, rather than an absolute cell).
        let mut app = App::new("m", "p");
        type_str(&mut app, "abcd");
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|f| app.render(f)).unwrap();
        let at_end = terminal.get_cursor_position().unwrap();
        app.on_key(key(KeyCode::Left));
        terminal.draw(|f| app.render(f)).unwrap();
        let one_left = terminal.get_cursor_position().unwrap();
        assert_eq!(one_left.y, at_end.y);
        assert_eq!(one_left.x, at_end.x - 1, "← shifts the cursor one column");
        app.on_key(key(KeyCode::Left));
        app.on_key(key(KeyCode::Left));
        terminal.draw(|f| app.render(f)).unwrap();
        assert_eq!(
            terminal.get_cursor_position().unwrap().x,
            at_end.x - 3,
            "cursor tracks the editor mid-line"
        );
    }

    #[test]
    fn long_input_line_wraps_to_visual_rows() {
        // cli 2026-09-27「多行文本框」: a long single line wraps to the box width
        // instead of running off (and being invisible).
        let mut app = App::new("m", "p");
        type_str(&mut app, "abcdefghijklmnopqrstuvwxyz");
        let visual = app.editor_visual_lines(8);
        assert_eq!(visual, vec!["abcdefgh", "ijklmnop", "qrstuvwx", "yz"]);
        assert_eq!(app.editor_visual_line_count(8), 4);
    }

    #[test]
    fn visual_cursor_lands_on_the_wrapped_row() {
        // The cursor sits on the *wrapped* row, not the logical row, when a long
        // line wraps (cli 2026-09-27).
        assert_eq!(visual_cursor_row_col("abcdefghijklmnop", 12, 8), (1, 4));
        assert_eq!(visual_cursor_row_col("abcdef", 0, 8), (0, 0));
        // `ab\ncd` — 5 chars consumed, so the cursor is after `d` on row 1, col 2.
        // (Was 4 → (1, 2): off by one — 4 chars consumed is `ab\nc`, col 1.)
        assert_eq!(visual_cursor_row_col("ab\ncdefgh", 5, 8), (1, 2));
        assert_eq!(visual_cursor_row_col("ab\ncdefgh", 4, 8), (1, 1));
        // CJK width counts double.
        assert_eq!(visual_cursor_row_col("你好世界", 3, 100), (0, 6));
    }

    // ── §12.10 ④ P1 折叠 + 吉祥物 ─────────────────────────────────

    /// Fire a `ToolCalled` whose (single-column) result has 30 distinct lines —
    /// comfortably past [`TOOL_OUTPUT_COLLAPSE_LINES`].
    fn long_tool_call(app: &mut App) {
        let preview = (1..=30)
            .map(|i| format!("row{i:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        app.on_event(&Event::ToolCalled {
            stage: STAGE_C.into(),
            tool: "read_file".into(),
            args: json!({"file_path": "big.rs"}),
            result_preview: preview,
            ms: 5.0,
            error: false,
            call_id: "c1".into(),
            diff: Value::Array(vec![]),
        });
    }

    #[test]
    fn long_tool_output_is_collapsed() {
        // §12.10 P1-a: a long `⎿ result` folds to its first N lines + a
        // `ctrl+o 展开` hint instead of flooding the transcript (cli P1-a).
        let mut app = App::new("m", "p");
        type_str(&mut app, "hi");
        app.on_key(key(KeyCode::Enter));
        long_tool_call(&mut app);
        let sb = scrollback_text(&mut app);
        let body: Vec<&str> = sb.lines().filter(|l| l.contains("row")).collect();
        assert_eq!(
            body.len(),
            TOOL_OUTPUT_HEAD_LINES,
            "only the head lines are shown (CC summary): {body:?}"
        );
        // cli 2026-09-28 (「控件显示行数改为3」): the fold shows **three** lines.
        assert_eq!(TOOL_OUTPUT_HEAD_LINES, 3, "folded tool output = 3 lines");
        assert_eq!(
            TOOL_OUTPUT_COLLAPSE_LINES, TOOL_OUTPUT_HEAD_LINES,
            "threshold and head stay in lockstep (no「还有 0 行」)"
        );
        assert!(sb.contains("row01"), "the head is kept: {sb}");
        assert!(!sb.contains("row30"), "the tail is hidden: {sb}");
        assert!(sb.contains("ctrl+o 展开"), "the fold hints at ctrl+o: {sb}");
        assert!(
            sb.contains("还有"),
            "the hint counts the hidden lines: {sb}"
        );
    }

    #[test]
    fn ctrl_o_expands_collapsed_tool_output() {
        // §12.10: the same ctrl+o that opens the reasoning fold reveals the whole
        // tool result, and a second press re-folds it.
        let mut app = App::new("m", "p");
        type_str(&mut app, "hi");
        app.on_key(key(KeyCode::Enter));
        long_tool_call(&mut app);
        let flat = |app: &App| -> String {
            app.history_lines()
                .iter()
                .map(|l| {
                    l.spans
                        .iter()
                        .map(|s| s.content.as_ref())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        assert!(
            !flat(&app).contains("row30"),
            "folded by default: {}",
            flat(&app)
        );
        app.on_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
        assert!(app.output_expanded);
        assert!(
            flat(&app).contains("row30"),
            "expanded reveals the tail: {}",
            flat(&app)
        );
        app.on_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
        assert!(!app.output_expanded);
        assert!(
            !flat(&app).contains("row30"),
            "collapsed again: {}",
            flat(&app)
        );
    }

    #[test]
    fn short_tool_output_is_not_folded() {
        // A result within the threshold renders in full — no hint, no hidden tail.
        let mut app = App::new("m", "p");
        type_str(&mut app, "hi");
        app.on_key(key(KeyCode::Enter));
        app.on_event(&Event::ToolCalled {
            stage: STAGE_C.into(),
            tool: "read_file".into(),
            args: json!({"file_path": "small.rs"}),
            result_preview: "line1\nline2\nline3".into(),
            ms: 2.0,
            error: false,
            call_id: "c1".into(),
            diff: Value::Array(vec![]),
        });
        let sb = scrollback_text(&mut app);
        assert!(
            sb.contains("line1") && sb.contains("line3"),
            "full body shown: {sb}"
        );
        assert!(!sb.contains("ctrl+o 展开"), "no fold hint: {sb}");
    }

    #[test]
    fn idle_state_shows_mascot() {
        // §12.10 P1-b: the empty input box carries the mascot — smiling at rest,
        // napping mid-turn.
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut app = App::new("m", "p");
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        let screen = |t: &Terminal<TestBackend>| -> String {
            t.backend()
                .buffer()
                .content
                .iter()
                .map(|c| c.symbol())
                .collect()
        };
        terminal.draw(|f| app.render(f)).unwrap();
        let idle = screen(&terminal);
        assert!(
            idle.contains(mascot(false)),
            "idle shows the mascot: {idle:?}"
        );
        assert!(!idle.contains(mascot(true)), "not the busy form: {idle:?}");
        // Mid-turn (the box empties on submit) → the napping mascot.
        type_str(&mut app, "hi");
        app.on_key(key(KeyCode::Enter));
        assert!(app.is_busy());
        terminal.draw(|f| app.render(f)).unwrap();
        let busy = screen(&terminal);
        assert!(
            busy.contains(mascot(true)),
            "busy shows the napping mascot: {busy:?}"
        );
        assert!(!busy.contains(mascot(false)), "idle form is gone: {busy:?}");
    }

    // ── §12.11 ⑤ P2 行距 / 留白 / 配色 ─────────────────────────────

    #[test]
    fn blank_separator_puts_one_gap_between_blocks() {
        // §12.14 (cli 2026-09-24 ③「控件间隔1行」): each block adds exactly one
        // blank line before itself, and none at the very start.
        let mut app = App::new("m", "p");
        app.push_tail(1.0); // first block → no leading gap
        app.push_tail(2.0); // second block → one blank before it
        let lines: Vec<String> = app
            .history_lines()
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        assert_eq!(lines.len(), 3, "tail, blank, tail: {lines:?}");
        assert!(
            lines[0].contains(TAIL_MARK),
            "first block on top: {lines:?}"
        );
        assert!(
            lines[1].trim().is_empty(),
            "one blank between blocks: {lines:?}"
        );
        assert!(
            lines[2].contains(TAIL_MARK),
            "second block below: {lines:?}"
        );
    }

    #[test]
    fn transcript_cache_matches_a_full_rebuild_and_grows_incrementally() {
        // Performance (cli 2026-09-27「翻页很卡」): the committed transcript is
        // flattened + wrapped once and cached. The cache must be *identical* to a
        // full rebuild, and must only append when new items arrive.
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut app = App::new("m", "p");
        app.push_info(&["第一块"]);
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        terminal.draw(|f| app.render(f)).unwrap();
        assert_eq!(app.cache_items, app.scrollback.len());
        let first_len = app.transcript_cache.len();
        assert!(first_len > 0, "the cache covers the committed items");

        // Append a block: the cache grows by appending, not rebuilding.
        app.push_info(&["第二块"]);
        terminal.draw(|f| app.render(f)).unwrap();
        assert_eq!(app.cache_items, app.scrollback.len());
        assert!(
            app.transcript_cache.len() > first_len,
            "new items were appended"
        );
        // Identical to a from-scratch render at the same width + folds.
        let full: Vec<String> = app
            .transcript_lines(&app.scrollback)
            .iter()
            .flat_map(|l| wrap_line(l, app.width as usize - 2))
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        let cached: Vec<String> = app
            .transcript_cache
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        assert_eq!(cached, full, "cache == full rebuild");

        // A width change invalidates and rebuilds (a resize must re-wrap).
        let before = app.transcript_cache.clone().len();
        let mut wide = Terminal::new(TestBackend::new(100, 20)).unwrap();
        wide.draw(|f| app.render(f)).unwrap();
        assert_eq!(app.cache_width, 100 - 2, "rebuilt at the new width");
        assert!(app.transcript_cache.len() <= before || before == 0);

        // `ctrl+o` (a fold toggle) also invalidates — the fold changes the lines.
        app.on_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
        terminal.draw(|f| app.render(f)).unwrap();
        assert_eq!(
            app.cache_folds,
            (app.thinking_expanded, app.output_expanded),
            "cache records the fold state it was built under"
        );
    }

    #[test]
    fn transcript_cache_covers_a_folded_thinking_block() {
        // The expensive case: a long committed reasoning block renders its folded
        // header + live tail, and the cache must hold exactly that (not the whole
        // reasoning) — see [`thinking_lines`]'s tail window.
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut app = App::new("m", "p");
        type_str(&mut app, "hi");
        app.on_key(key(KeyCode::Enter));
        let long = "推理内容 ".repeat(20_000);
        app.on_event(&Event::LlmDelta {
            stage: STAGE_C.into(),
            kind: "reasoning".into(),
            text: long,
        });
        app.on_event(&Event::ToolStarted {
            stage: STAGE_C.into(),
            tool: "bash".into(),
            args: json!({"command": "ls"}),
            call_id: "c1".into(),
        });
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|f| app.render(f)).unwrap();
        // The folded block contributes its header + at most the tail rows.
        assert!(
            app.transcript_cache.len() <= THINKING_TAIL_LINES + 4,
            "a folded 60K-char reasoning yields only its fold rows: {}",
            app.transcript_cache.len()
        );
        let text: String = app
            .transcript_cache
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("思考"), "the fold header is drawn: {text:?}");
        assert!(text.contains("ctrl+o"), "the fold hint is drawn: {text:?}");
    }

    #[test]
    fn tail_window_returns_a_bounded_suffix_at_a_char_boundary() {
        // The folded reasoning wraps only a generous suffix (performance); the
        // helper must be cheap, exact, and never split a UTF-8 char.
        assert_eq!(tail_window("abcdef", 100), "abcdef");
        assert_eq!(tail_window("abcdef", 6), "abcdef");
        assert_eq!(tail_window("abcdef", 2), "ef");
        assert_eq!(tail_window("", 10), "");
        assert_eq!(tail_window("abcdef", 0), "");
        // CJK: 5 chars, keep the last 3.
        assert_eq!(tail_window("你好世界啊", 3), "世界啊");
        // The returned slice is always valid UTF-8 (no panic on any cut).
        let s = "a你好b世界c";
        for n in 0..=s.chars().count() {
            let w = tail_window(s, n);
            assert_eq!(w.chars().count(), n.min(s.chars().count()), "n={n}");
        }
    }

    #[test]
    fn scrolled_hint_names_ctrl_end() {
        // cli 2026-09-27: paging to the bottom is bound to `Ctrl+End` (plain
        // `End` moves the input cursor), so the scrolled-up hint must say so.
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut app = App::new("m", "p");
        for i in 0..80 {
            app.push_info(&[&format!("line {i}")]);
        }
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|f| app.render(f)).unwrap();
        app.scroll_up(10);
        terminal.draw(|f| app.render(f)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        // CJK chars are double-width, so the buffer interleaves blank cells —
        // match per char (as the other render tests do).
        assert!(
            text.contains('已') && text.contains('翻'),
            "indicator shown: {text:?}"
        );
        assert!(
            text.contains("ctrl+end"),
            "the hint names ctrl+end, not End: {text:?}"
        );
        assert!(
            !text.contains("· End "),
            "the old (wrong) `End 回到底部` wording is gone: {text:?}"
        );
    }

    #[test]
    fn blocks_are_separated_by_one_blank_line() {
        // §12.14: every block — user bar, reply, tail — is separated by exactly
        // one blank line, matching CC's rhythm (cli 2026-09-24 ③「控件间隔1行」;
        // page41 :99 同屏实拍). No two blanks ever run together, and the
        // transcript never opens or closes on a blank.
        let mut app = App::new("m", "p");
        for (input, reply) in [("first", "reply one"), ("second", "reply two")] {
            type_str(&mut app, input);
            app.on_key(key(KeyCode::Enter));
            app.on_event(&Event::LlmDelta {
                stage: STAGE_C.into(),
                kind: "content".into(),
                text: reply.into(),
            });
            app.on_turn_done(Ok(SummaryReport::default()));
        }
        let lines: Vec<String> = app
            .history_lines()
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        assert!(!lines.is_empty());
        assert!(!lines[0].trim().is_empty(), "no leading blank: {lines:?}");
        assert!(
            !lines[lines.len() - 1].trim().is_empty(),
            "no trailing blank: {lines:?}"
        );
        for (i, l) in lines.iter().enumerate() {
            if l.trim().is_empty() {
                assert!(
                    i > 0 && i + 1 < lines.len(),
                    "gap has neighbours: {lines:?}"
                );
                assert!(
                    !lines[i - 1].trim().is_empty(),
                    "no double blank above: {lines:?}"
                );
                assert!(
                    !lines[i + 1].trim().is_empty(),
                    "no double blank below: {lines:?}"
                );
            }
        }
        // The 2nd turn's bar is preceded by exactly one blank.
        let second_bar = lines
            .iter()
            .enumerate()
            .filter(|(_, l)| l.contains('❯'))
            .map(|(i, _)| i)
            .nth(1)
            .expect("second turn bar");
        assert!(
            lines[second_bar - 1].trim().is_empty(),
            "blank precedes the 2nd bar: {lines:?}"
        );
        // Both replies survive the spacing change.
        let joined = lines.join("\n");
        assert!(joined.contains("reply one") && joined.contains("reply two"));
    }

    #[test]
    fn muted_text_is_cc_mid_grey_and_structural_lines_stay_mid_grey() {
        // cli 2026-09-24 (「灰色的颜色比 CC 的暗了」): the muted grey is now a
        // **fixed** mid grey (#AAAAAA) matching CC's live muted text (≈ RGB 170);
        // a raw ANSI 90 (`DarkGray`) rendered too dark on cli's terminal. The
        // structural hairlines stay the mid grey CC uses for frames
        // (`border #7a6c52`), and the accents/signals are untouched.
        assert_eq!(BORDER, Color::Gray);
        assert_eq!(crate::theme::RULE, Color::Gray);
        assert_eq!(TEXT_MUTED, Color::Rgb(0xAA, 0xAA, 0xAA));
        // Thinking keywords lift to CC's measured soft blue — a **fixed** RGB
        // (cli 2026-09-24「高亮的处理」: CC draws the code token in periwinkle
        // #B1B9F9, not white + bold).
        assert_eq!(THINKING_KW, Color::Rgb(0xB1, 0xB9, 0xF9));
        // The tool result sits one notch above the reasoning: ANSI 37 light grey
        // (cli 2026-09-24「比思考稍微亮一点点的灰色」).
        assert_eq!(TOOL_RESULT, Color::Gray);
        // Untouched: the brand accent (CC fixed orange) — which the logo now
        // aliases, so the brand mark and the activity line are the same ink
        // (cli 2026-09-28「把 logo 颜色改成跟活动行一样的颜色」) …
        assert_eq!(ACCENT, Color::Rgb(0xD7, 0x77, 0x57));
        assert_eq!(LOGO, ACCENT);
        // … and the diff pair (cli 2026-09-28「代码改动的红绿对比」), which takes
        // CC's **ANSI** mapping (`diffAdded: ansi:green` / `diffRemoved: ansi:red`)
        // because 灵妙 is terminal-native, not CC's fixed RGB palette.
        assert_eq!(DIFF_ADD, Color::Green);
        assert_eq!(DIFF_REMOVE, Color::Red);
        assert_eq!(DIFF_CONTEXT, TEXT_MUTED);
        // … and the settled semantic signals.
        assert_eq!(RUNNING, Color::Yellow);
        assert_eq!(crate::theme::WARN, Color::Yellow);
        assert_eq!(SUCCESS, Color::Green);
        assert_eq!(ERROR, Color::Red);
    }

    // ── message queue (cli 2026-09-24 ③, CC 「排队」) ───────────────

    #[test]
    fn enter_while_busy_queues_instead_of_dropping() {
        let mut app = App::new("m", "p");
        type_str(&mut app, "first");
        assert_eq!(
            app.on_key(key(KeyCode::Enter)),
            Action::Submit("first".into())
        );
        // Mid-turn: a typed line queues on Enter (no engine submit).
        type_str(&mut app, "queued one");
        assert_eq!(app.on_key(key(KeyCode::Enter)), Action::None);
        assert_eq!(app.queued_len(), 1);
        assert!(app.editor.is_empty(), "the box clears into the queue");
        // The turn ends → the queue drains one message.
        app.on_turn_done(Ok(SummaryReport::default()));
        assert_eq!(app.dequeue_turn(), Some("queued one".into()));
        assert!(app.is_busy(), "the queued turn began");
        assert_eq!(app.queued_len(), 0);
        // …and it drew the user bar for the promoted message.
        let sb = scrollback_text(&mut app);
        assert!(
            sb.contains("queued one"),
            "the promoted turn is in the transcript: {sb}"
        );
    }

    #[test]
    fn queued_message_renders_as_a_bar_with_send_now_hint() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut app = App::new("m", "p");
        type_str(&mut app, "hi");
        app.on_key(key(KeyCode::Enter));
        type_str(&mut app, "queued please");
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.queued_len(), 1);
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|f| app.render(f)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(
            text.contains("queued please"),
            "queued text shown: {text:?}"
        );
        assert!(
            text.contains("ctrl+x ctrl+s"),
            "send-now hint shown: {text:?}"
        );
    }

    #[test]
    fn up_while_busy_pulls_a_queued_message_back_for_editing() {
        let mut app = App::new("m", "p");
        type_str(&mut app, "hi");
        app.on_key(key(KeyCode::Enter));
        type_str(&mut app, "queued");
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.queued_len(), 1);
        assert_eq!(app.on_key(key(KeyCode::Up)), Action::None);
        assert_eq!(
            app.editor.text, "queued",
            "the queued message is back in the box"
        );
        assert_eq!(app.queued_len(), 0, "editing removes it from the queue");
    }

    #[test]
    fn ctrl_x_ctrl_s_sends_the_queued_message_now() {
        let mut app = App::new("m", "p");
        type_str(&mut app, "hi");
        app.on_key(key(KeyCode::Enter));
        type_str(&mut app, "queued");
        app.on_key(key(KeyCode::Enter));
        // A lone ctrl+s (never armed) does nothing.
        assert_eq!(
            app.on_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)),
            Action::None
        );
        // ctrl+x arms, then ctrl+s fires.
        assert_eq!(
            app.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL)),
            Action::None
        );
        assert_eq!(
            app.on_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)),
            Action::SendQueuedNow
        );
    }

    #[test]
    fn activity_spinner_cycles_through_the_arc_frames() {
        // cli 2026-09-28: the activity mark is a rotating **arc** — not CC's
        // asterisk family, whose glyphs this terminal had to substitute from a
        // proportional fallback font (that is what made the line jitter).
        assert!(ACTIVITY_SPINNER.contains(&cc_spinner(0)));
        assert_ne!(cc_spinner(0), cc_spinner(120), "the glyph steps");
        for i in 0..ACTIVITY_SPINNER.len() as u64 {
            assert!(ACTIVITY_SPINNER.contains(&cc_spinner(i * 120)), "i={i}");
        }
        assert_eq!(
            cc_spinner(120 * ACTIVITY_SPINNER.len() as u64),
            cc_spinner(0)
        );
        // Every frame must be **one column wide** with no East-Asian ambiguity —
        // that is the whole point of the change (a 2-column or font-substituted
        // frame is exactly what made the line wobble).
        for c in ACTIVITY_SPINNER {
            assert_eq!(display_width(&c.to_string()), 1, "1 column: {c:?}");
            assert_eq!(char_width(c), 1, "narrow: {c:?}");
        }
        // The CC star / flower glyphs are gone.
        for c in ACTIVITY_SPINNER {
            assert!(
                !matches!(c, '·' | '✽' | '✢' | '✳' | '✻' | '✶'),
                "the CC star/flower glyphs are gone: {c:?}"
            );
        }
        // The tail mark is the arc's home frame (so the tail reads as "stopped").
        assert_eq!(TAIL_MARK, ACTIVITY_SPINNER[0]);
    }

    // ── reverse history search (cli 2026-09-27, CC `ctrl+r`) ────────

    #[test]
    fn ctrl_r_opens_history_search_and_filters_newest_first() {
        // cli 2026-09-27 (CC `ctrl+r: history:search`): ctrl+r opens a search
        // overlay over submitted history; typing filters it (substring), newest
        // first.
        let mut app = App::new("m", "p");
        for s in ["alpha one", "beta two", "gamma three"] {
            type_str(&mut app, s);
            app.on_key(key(KeyCode::Enter));
            app.on_turn_done(Ok(SummaryReport::default()));
        }
        let ctrl_r = KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL);
        assert_eq!(app.on_key(ctrl_r), Action::None);
        assert!(app.search_open);
        // Empty query → every entry, newest first.
        let all = app.search_matches();
        assert_eq!(all.len(), 3);
        assert_eq!(app.history[all[0]], "gamma three", "newest first");
        // Substring filter (case-insensitive) narrows the list.
        type_str(&mut app, "TWO");
        let m = app.search_matches();
        assert_eq!(m.len(), 1);
        assert_eq!(app.history[m[0]], "beta two");
        // Backspace widens the filter and resets the highlight.
        app.on_key(key(KeyCode::Backspace));
        assert_eq!(app.search_query, "TW");
        assert_eq!(app.search_selected, 0);
        // `tw` still matches `beta two`; a nonsense filter empties the list.
        assert_eq!(app.search_matches().len(), 1);
        type_str(&mut app, "zz");
        assert!(app.search_matches().is_empty(), "no match for `twzz`");
    }

    #[test]
    fn ctrl_r_enter_fills_the_editor_and_esc_cancels() {
        let mut app = App::new("m", "p");
        for s in ["first", "second"] {
            type_str(&mut app, s);
            app.on_key(key(KeyCode::Enter));
            app.on_turn_done(Ok(SummaryReport::default()));
        }
        let ctrl_r = KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL);
        // Open, move to the older entry, Enter → it lands in the editor.
        app.on_key(ctrl_r);
        assert_eq!(app.on_key(key(KeyCode::Down)), Action::None);
        assert_eq!(app.search_selected, 1);
        assert_eq!(app.on_key(key(KeyCode::Enter)), Action::None);
        assert!(!app.search_open, "Enter closes the overlay");
        assert_eq!(
            app.editor.text, "first",
            "the second-newest entry is filled"
        );

        // Esc cancels without touching the editor.
        app.editor.set_text("keep me");
        app.on_key(ctrl_r);
        type_str(&mut app, "zzz");
        assert_eq!(app.on_key(key(KeyCode::Esc)), Action::None);
        assert!(!app.search_open);
        assert_eq!(app.editor.text, "keep me", "Esc leaves the box as it was");

        // ctrl+r seeds the filter with the current draft (CC `initialQuery`).
        app.editor.set_text("sec");
        app.on_key(ctrl_r);
        assert_eq!(app.search_query, "sec");
        assert_eq!(app.search_matches().len(), 1);
    }

    #[test]
    fn history_search_renders_the_filter_and_matches() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut app = App::new("m", "p");
        for s in ["deploy staging", "deploy prod"] {
            type_str(&mut app, s);
            app.on_key(key(KeyCode::Enter));
            app.on_turn_done(Ok(SummaryReport::default()));
        }
        app.on_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
        type_str(&mut app, "prod");
        let mut terminal = Terminal::new(TestBackend::new(100, 40)).unwrap();
        terminal.draw(|f| app.render(f)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("prod"), "the filter head shows: {text:?}");
        assert!(
            text.contains("deploy prod"),
            "the match is listed: {text:?}"
        );
        assert!(
            text.contains('搜') && text.contains('历'),
            "the overlay title is shown: {text:?}"
        );
    }

    // ── in-app text selection (cli 2026-09-27) ────────────────────

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn ctrl_key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    /// Render `app` once and return it with a populated viewport mapping (the
    /// selection code reads `pane_area` / `viewport_start` / `viewport_lines`,
    /// which only a real `render` fills in).
    fn rendered(app: &mut App) -> ratatui::Terminal<ratatui::backend::TestBackend> {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();
        terminal.draw(|f| app.render(f)).unwrap();
        terminal
    }

    /// The absolute transcript line index whose visible text contains `needle`.
    fn viewport_line_of(app: &App, needle: &str) -> u32 {
        let idx = app
            .viewport_lines
            .iter()
            .position(|l| l.contains(needle))
            .unwrap_or_else(|| panic!("{needle:?} not in viewport: {:?}", app.viewport_lines));
        app.viewport_start + idx as u32
    }

    /// Drag across `needle`'s first `cols` columns, then release. Returns the
    /// row the drag lived on (so a test can assert on the frame buffer).
    fn drag_select(app: &mut App, needle: &str, cols: u16) -> u16 {
        let pane = app.pane_area;
        let line = viewport_line_of(app, needle);
        let row = pane.y + (line - app.viewport_start) as u16;
        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), pane.x, row));
        app.on_mouse(mouse(
            MouseEventKind::Drag(MouseButton::Left),
            pane.x + cols,
            row,
        ));
        app.on_mouse(mouse(
            MouseEventKind::Up(MouseButton::Left),
            pane.x + cols,
            row,
        ));
        row
    }

    #[test]
    fn mouse_drag_selects_without_copying() {
        // cli 2026-09-27 (「不需要马上复制，让用户自己选择，也不要顶掉」): a drag
        // only *selects*. The clipboard write is the explicit `ctrl+y`, so the
        // release must not produce any mouse-driven copy effect at all.
        let mut app = App::new("m", "p");
        app.push_info(&["hello world"]);
        let _ = rendered(&mut app);
        drag_select(&mut app, "hello world", 5);
        // The selection survives the release…
        let sel = app.selection.expect("the release keeps the selection");
        assert_ne!(sel.anchor, sel.head, "the drag actually selected something");
        assert_eq!(app.selection_text_opt(), Some("hello".to_string()));
        // …and nothing was copied / announced by the mouse path.
        assert!(app.copy_note.is_none(), "a drag prints no 已复制 notice");
    }

    #[test]
    fn ctrl_y_copies_the_live_selection_without_clearing_it() {
        // The explicit copy key: `ctrl+y` puts the live selection on the
        // clipboard and leaves the highlight exactly where it was (cli
        // 2026-09-27「也不要顶掉」).
        let mut app = App::new("m", "p");
        app.push_info(&["hello world"]);
        let _ = rendered(&mut app);
        drag_select(&mut app, "hello world", 5);
        let before = app.selection;
        assert_eq!(app.on_key(ctrl_key('y')), Action::Copy("hello".into()));
        assert_eq!(app.selection, before, "the copy keeps the selection");
    }

    #[test]
    fn shift_insert_pastes_and_ctrl_y_is_the_copy_key() {
        // cli 2026-09-28: `shift+insert` is the classic X11/WSL **paste** chord,
        // so it now pastes; copy is the unambiguous `ctrl+y` (plus `ctrl+c` when a
        // selection is live).
        let mut app = App::new("m", "p");
        app.push_info(&["hello world"]);
        let _ = rendered(&mut app);
        drag_select(&mut app, "hello world", 5);
        assert_eq!(
            app.on_key(KeyEvent::new(KeyCode::Insert, KeyModifiers::SHIFT)),
            Action::Paste
        );
        assert_eq!(app.on_key(ctrl_key('y')), Action::Copy("hello".into()));
    }

    #[test]
    fn right_drag_also_selects() {
        // cli 2026-09-27「右键拖动也要能选择」: a right drag has no other meaning
        // in a mouse-capturing transcript, so it selects exactly like the left.
        let mut app = App::new("m", "p");
        app.push_info(&["hello world"]);
        let _ = rendered(&mut app);
        let pane = app.pane_area;
        let line = viewport_line_of(&app, "hello world");
        let row = pane.y + (line - app.viewport_start) as u16;
        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Right), pane.x, row));
        app.on_mouse(mouse(
            MouseEventKind::Drag(MouseButton::Right),
            pane.x + 5,
            row,
        ));
        app.on_mouse(mouse(
            MouseEventKind::Up(MouseButton::Right),
            pane.x + 5,
            row,
        ));
        assert_eq!(app.selection_text_opt(), Some("hello".to_string()));
        assert_eq!(app.on_key(ctrl_key('y')), Action::Copy("hello".into()));
    }

    #[test]
    fn copy_without_a_selection_is_inert() {
        // `ctrl+y` with nothing selected must not fall through to editing (it is
        // not a printable char) and must not copy anything.
        let mut app = App::new("m", "p");
        app.push_info(&["hello world"]);
        let _ = rendered(&mut app);
        type_str(&mut app, "draft");
        assert_eq!(app.on_key(ctrl_key('y')), Action::None);
        assert_eq!(app.editor.text, "draft", "the buffer is untouched");
        assert!(app.copy_note.is_none());
    }

    #[test]
    fn esc_drops_the_selection_before_the_input() {
        // Esc is the escape hatch for an unwanted selection: the first press
        // clears the highlight, the next goes back to its normal meaning.
        let mut app = App::new("m", "p");
        app.push_info(&["hello world"]);
        let _ = rendered(&mut app);
        drag_select(&mut app, "hello world", 5);
        type_str(&mut app, "typed");
        assert_eq!(app.on_key(key(KeyCode::Esc)), Action::None);
        assert!(app.selection.is_none(), "Esc clears the selection");
        assert_eq!(app.editor.text, "typed", "…and leaves the input alone");
        assert_eq!(app.on_key(key(KeyCode::Esc)), Action::None);
        assert!(app.editor.is_empty(), "the next Esc clears the input");
    }

    #[test]
    fn click_outside_the_conversation_pane_starts_no_selection() {
        // A click on the input box or the footer must not select transcript text
        // (the pane is the only selectable region) and drops any old selection.
        let mut app = App::new("m", "p");
        app.push_info(&["hello world"]);
        let _ = rendered(&mut app);
        assert_eq!(app.pane_area.height, 8, "60x12 − footer 1 − input 3");
        drag_select(&mut app, "hello world", 5);
        assert!(app.selection.is_some());
        let outside = app.pane_area.bottom();
        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 3, outside));
        assert!(app.selection.is_none(), "clicks below the pane are inert");
    }

    #[test]
    fn bare_click_leaves_no_highlight_behind() {
        // A press+release that never moved is not a selection (a stray click must
        // not leave a one-cell highlight behind).
        let mut app = App::new("m", "p");
        app.push_info(&["hello"]);
        let _ = rendered(&mut app);
        let pane = app.pane_area;
        let row = pane.y + (viewport_line_of(&app, "hello") - app.viewport_start) as u16;
        app.on_mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            pane.x + 2,
            row,
        ));
        app.on_mouse(mouse(
            MouseEventKind::Up(MouseButton::Left),
            pane.x + 2,
            row,
        ));
        assert!(app.selection.is_none());
        assert_eq!(app.on_key(ctrl_key('y')), Action::None);
    }

    #[test]
    fn selection_tracks_absolute_lines_so_streaming_does_not_shift_it() {
        // The anchor is stored as an *absolute* transcript line, so lines that
        // stream in below during a drag cannot move what is highlighted.
        let mut app = App::new("m", "p");
        app.push_info(&["alpha"]);
        let _ = rendered(&mut app);
        let pane = app.pane_area;
        let line = viewport_line_of(&app, "alpha");
        let row = pane.y + (line - app.viewport_start) as u16;
        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), pane.x, row));
        let anchor = app.selection.expect("selection").anchor;
        // A new line lands *below* while the user is still dragging.
        app.push_info(&["streamed below"]);
        app.on_mouse(mouse(
            MouseEventKind::Drag(MouseButton::Left),
            pane.x + 3,
            row,
        ));
        assert_eq!(
            app.selection.expect("selection").anchor,
            anchor,
            "the anchor does not move when the transcript grows"
        );
        assert_eq!(app.selection_text_opt(), Some("alp".to_string()));
    }

    #[test]
    fn selection_spans_multiple_lines_in_reading_order() {
        // A drag across rows copies whole lines, top-to-bottom, joined by `\n` —
        // regardless of whether the drag ran downward or upward.
        let mut app = App::new("m", "p");
        app.push_info(&["first line", "second line"]);
        let _ = rendered(&mut app);
        let pane = app.pane_area;
        let a = viewport_line_of(&app, "first line");
        let b = viewport_line_of(&app, "second line");
        let row_a = pane.y + (a - app.viewport_start) as u16;
        let row_b = pane.y + (b - app.viewport_start) as u16;
        // Drag **upward**: start at the bottom line, end at the top one.
        app.on_mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            pane.x + 4,
            row_b,
        ));
        app.on_mouse(mouse(
            MouseEventKind::Drag(MouseButton::Left),
            pane.x,
            row_a,
        ));
        app.on_mouse(mouse(MouseEventKind::Up(MouseButton::Left), pane.x, row_a));
        assert_eq!(
            app.selection_text_opt(),
            Some("first line\nseco".to_string()),
            "reading order, not drag order"
        );
    }

    #[test]
    fn display_col_to_char_idx_snaps_inside_a_wide_glyph() {
        // Columns land on *character* boundaries: a column inside a CJK glyph
        // (which is 2 columns wide) resolves to that glyph, never into the middle
        // of it.
        assert_eq!(display_col_to_char_idx("abc", 0), 0);
        assert_eq!(display_col_to_char_idx("abc", 2), 2);
        assert_eq!(display_col_to_char_idx("abc", 99), 3, "clamps past the end");
        // `中` = 2 columns → columns 0 and 1 both resolve to char 0, column 2 → 1.
        assert_eq!(display_col_to_char_idx("中文", 0), 0);
        assert_eq!(display_col_to_char_idx("中文", 1), 0, "inside the glyph");
        assert_eq!(display_col_to_char_idx("中文", 2), 1);
        assert_eq!(display_col_to_char_idx("中文", 4), 2);
    }

    #[test]
    fn drag_highlights_the_selected_cells_inverse() {
        // A live selection is painted **inverse** on the frame buffer (reusing the
        // §11 selection colour contract), so the user sees what will be copied.
        let mut app = App::new("m", "p");
        app.push_info(&["hello world"]);
        let _ = rendered(&mut app);
        let pane = app.pane_area;
        let row = drag_select(&mut app, "hello world", 5);
        let terminal = rendered(&mut app);
        let buf = terminal.backend().buffer().clone();
        let reversed = |x: u16| buf[(x, row)].modifier.contains(Modifier::REVERSED);
        assert!(reversed(pane.x), "the first selected cell is inverse");
        assert!(reversed(pane.x + 4), "… through the last one");
        assert!(
            !reversed(pane.x + 5),
            "and stops there (space not selected)"
        );
    }

    #[test]
    fn selection_stays_highlighted_after_release() {
        // cli 2026-09-27「也不要顶掉」: releasing must not repaint over the
        // selection — the old `已复制` banner was drawn across the pane's bottom
        // row, which is exactly where a drag usually ends.
        let mut app = App::new("m", "p");
        app.push_info(&["hello world"]);
        let _ = rendered(&mut app);
        let pane = app.pane_area;
        let row = drag_select(&mut app, "hello world", 5);
        // …and it is *still* reversed on the next frame after the release.
        let terminal = rendered(&mut app);
        let buf = terminal.backend().buffer().clone();
        assert!(
            buf[(pane.x, row)].modifier.contains(Modifier::REVERSED),
            "the highlight survives the release"
        );
        // The bottom row of the pane carries no overpainted banner.
        let bottom: String = (0..60)
            .map(|c| buf[(c, pane.bottom().saturating_sub(1))].symbol())
            .collect();
        assert!(
            !bottom.contains('复') && !bottom.contains('已'),
            "no copy banner over the pane: {bottom:?}"
        );
    }

    #[test]
    fn copy_note_renders_on_the_footer_not_over_the_pane() {
        // The confirmation lives on the **footer** row (right-aligned identity +
        // left-aligned notice), never over the conversation pane.
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut app = App::new("m", "p");
        app.push_info(&["hello world"]);
        let mut terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();
        terminal.draw(|f| app.render(f)).unwrap();
        app.set_copy_note("已复制 5 字符".to_string());
        terminal.draw(|f| app.render(f)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let rows: Vec<String> = (0..24)
            .map(|r| (0..120).map(|c| buf[(c, r)].symbol()).collect())
            .collect();
        let footer = rows.last().expect("a footer row");
        for ch in "已复制".chars() {
            assert!(
                footer.contains(ch),
                "the notice is on the footer: {footer:?}"
            );
        }
        // The pane rows stay free of it.
        for r in &rows[..rows.len() - 1] {
            assert!(
                !r.contains('已') && !r.contains('复'),
                "no banner inside the pane: {r:?}"
            );
        }
    }

    #[test]
    fn wheel_still_pages_the_conversation() {
        // The mouse handler replaced the old inline wheel match — paging must be
        // intact (§11 / §16.3).
        let mut app = App::new("m", "p");
        for i in 0..80 {
            app.push_info(&[&format!("line {i}")]);
        }
        let _ = rendered(&mut app);
        assert!(app.max_scroll > 0);
        app.on_mouse(mouse(MouseEventKind::ScrollUp, 10, 4));
        assert_eq!(app.scroll, WHEEL_LINES);
        app.on_mouse(mouse(MouseEventKind::ScrollDown, 10, 4));
        assert_eq!(
            app.scroll, 0,
            "scrolling back down returns to the newest line"
        );
    }

    // ── un-submitted-input snapshot (cli 2026-09-28「队列里的提示词还能看到吗？」) ──

    #[test]
    fn unsubmitted_captures_both_the_queue_and_the_live_draft() {
        let mut app = App::new("m", "p");
        // Open a turn so the box is in queue mode…
        type_str(&mut app, "hi");
        app.on_key(key(KeyCode::Enter));
        assert!(app.is_busy());
        // …queue one message mid-turn…
        type_str(&mut app, "queued one");
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.queued_len(), 1);
        // …then start typing the *next* prompt, still un-submitted.
        type_str(&mut app, "half typed");
        let snap = app.unsubmitted();
        assert_eq!(snap.queue, vec!["queued one".to_string()]);
        assert_eq!(snap.draft, "half typed");
    }

    #[test]
    fn nothing_un_submitted_snapshots_as_empty() {
        // The common case: idle, empty box, empty queue — the snapshot file is
        // removed rather than written, so a stale draft can never come back.
        let app = App::new("m", "p");
        assert_eq!(app.unsubmitted(), SessionInput::default());
        assert!(app.unsubmitted().is_empty());
    }

    #[test]
    fn restore_puts_the_queue_back_in_order_and_refills_the_box() {
        let mut app = App::new("m", "p");
        let snap = SessionInput {
            queue: vec!["一".into(), "二".into()],
            draft: "草稿".into(),
        };
        app.restore_unsubmitted(&snap);
        assert_eq!(app.queued_len(), 2);
        assert_eq!(app.editor.text, "草稿");
        // The queue drains front-first, exactly as if it had never been lost.
        assert_eq!(app.dequeue_turn(), Some("一".into()));
        assert_eq!(app.dequeue_turn(), Some("二".into()));
    }

    #[test]
    fn a_restored_draft_that_the_user_submits_leaves_no_stale_snapshot() {
        // The behaviour that matters after recovery: the restored text is a
        // normal draft — submitting it must not come back on the next start.
        let mut app = App::new("m", "p");
        app.restore_unsubmitted(&SessionInput {
            queue: Vec::new(),
            draft: "restored".into(),
        });
        assert_eq!(
            app.on_key(key(KeyCode::Enter)),
            Action::Submit("restored".into())
        );
        assert_eq!(app.unsubmitted(), SessionInput::default());
    }
}

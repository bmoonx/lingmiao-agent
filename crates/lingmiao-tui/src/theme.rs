//! Design tokens (UX §5 主题令牌层) — one definition, referenced everywhere.
//!
//! The TUI is *terminal-native*: it never paints a background (the terminal's
//! own palette shows through), colour is a **signal** (state / thresholds), and
//! a single accent is reserved for the few key actions (the `❯` prompt, the
//! completion palette, markdown headings). Centralising the tokens here keeps that
//! contract enforceable — the alternative (each widget picking its own colours)
//! is what reads as cheap.

use ratatui::style::Color;

// ── Layered palette (UX §14 P0) ─────────────────────────────────────────
//
// Source: the CC **light** theme captured in page24 / KG —— `bg #fffdf8 ·
// surface #f4efe4 · border #7a6c52 · line #8a7f6d · text #42392e` (dark:
// `#1f232b / #262b34 / #9aa4b8 / #a8adb8 / #f2f3f5`). 灵妙 is *terminal-native*
// (§2 theme decision, reaffirmed by the 2026-09-16 拍板「跟随系统终端主题」): it
// **never paints a background**, so these structural roles are mapped onto the
// terminal's *own* palette instead of fixed RGB — that is exactly how CC itself
// behaves (CC follows ANSI, obs「CC 不自绘背景」), and it keeps 灵妙 legible on
// both light and dark terminals. The one exception is the brand accent below,
// which CC fixes at RGB orange.

/// **Surface** — the top-level panel role. Reserved: the TUI is terminal-native
/// and deliberately does **not** paint a background, so this exists so callers
/// never reach for a raw colour when they mean "a surface".
pub const SURFACE: Color = Color::Reset;
/// **Border** — frames and the rounded input box (§13).
///
/// §12.11 P2-b: lifted one ANSI notch (`DarkGray` → `Gray`) so the frame reads
/// as a soft, *visible* grey — matching CC's mid-grey border (`#7a6c52`) —
/// instead of sinking into a dark terminal. (`DarkGray` is ANSI bright-black,
/// i.e. dimmer than mid-grey.)
pub const BORDER: Color = Color::Gray;
/// **Rule** — the full-width `─` hairline separators (§13).
///
/// §12.11 P2-b: same one-notch lift as [`BORDER`] (`DarkGray` → `Gray`), so the
/// hairline is a legible soft line (CC's `line #8a7f6d`) rather than nearly
/// invisible on a dark background.
pub const RULE: Color = Color::Gray;
/// **Text** — primary body text (model line, whiteboard title, tool args).
/// `Reset` = the terminal's *default* foreground, so it is the most prominent
/// colour on either a light or a dark terminal (a fixed grey would sink into a
/// light background).
pub const TEXT: Color = Color::Reset;
/// **Text (muted)** — secondary / metadata text: reasoning (`∴`), the `⎿`
/// **glyph** (its *body* is [`TEXT`], the bright data — §12.15), the `◜` turn
/// tail, footer hints, timings, version, cwd. The workhorse of the chrome.
///
/// cli 2026-09-24 (「灰色的颜色比 CC 的暗了」): the previous value was ANSI 90
/// (`Color::DarkGray` — CC's own ANSI `gray` table entry, `gray:[90,39]`), but
/// on cli's terminal that renders ≈ RGB 90, visibly **darker** than CC's live
/// muted text (measured ≈ RGB 170 in the same :99 side-by-side screenshot). The
/// token is therefore a **fixed** mid grey (#AAAAAA) matching what CC actually
/// draws, independent of the terminal's ANSI-90 mapping. It stays darker than
/// the tool result ([`TOOL_RESULT`], ANSI 37 ≈ RGB 192) and the primary [`TEXT`]
/// (Reset), so the body-vs-chrome hierarchy still holds.
pub const TEXT_MUTED: Color = Color::Rgb(0xAA, 0xAA, 0xAA);
/// **Tool result** — the `⎿ result` body of a tool card. cli 2026-09-24
/// (「工具调用返回值显示为比思考稍微亮一点点的灰色」): the result is a *slightly*
/// brighter grey than the reasoning ([`TEXT_MUTED`], fixed #AAAAAA) — ANSI
/// 37 light grey — so the two-level hierarchy reads clearly without competing
/// with the primary [`TEXT`] answer. §12.15 had made it the brightest
/// foreground; cli pulled it back to this middle grey so a raw tool dump
/// registers as data, one notch above the muted reasoning.
pub const TOOL_RESULT: Color = Color::Gray;
/// **Selection** — the reverse-video scroll-up indicator (§11). Fore/background
/// pair so the bar reads as a solid block on any terminal palette.
pub const SELECTION_FG: Color = Color::Black;
pub const SELECTION_BG: Color = Color::Gray;

/// Semantic status colours (colour is a **signal**, §8.4): the tool dot, the
/// idle bullet, the threshold bar, error/notice lines.
pub const SUCCESS: Color = Color::Green;
pub const WARN: Color = Color::Yellow;
pub const ERROR: Color = Color::Red;
/// **Running** — the in-flight *activity* signal: the header status line
/// (`正在作答…`) and the running tool spinner (`⠋ … · Ns`). Kept distinct from
/// [`WARN`], which is reserved for *real* warnings (the context-threshold bar,
/// fault notices): painting normal, healthy work in the warning colour was a
/// semantic inversion (§14-P2). Same amber value, but a different *meaning*, so
/// the two roles can diverge later without touching call sites.
pub const RUNNING: Color = Color::Yellow;
/// **Logo** — the brand mark beside the startup banner.
///
/// cli 2026-09-24 (「灵妙的标志换个桔黄色好看的」): the mark was a warm **amber**
/// orange (#F5A623). cli 2026-09-28 (「把 logo 的颜色改成跟输入框提示这里一样的
/// 颜色」): it then took the input-box placeholder grey ([`TEXT_MUTED`], #AAAAAA).
///
/// cli 2026-09-28 (「把 logo 颜色改成跟活动行一样的颜色」): the mark now wears
/// exactly the **activity line's** colour — the in-flight `◜ {动词}… Ns · esc 中断`
/// bar — which is [`ACCENT`] (CC's brand orange, #D77757). So the brand mark and
/// the live progress line are *the same ink*: the two places 灵妙 signs its own
/// work read as one brand, and the mark is lifted back out of the grey chrome.
///
/// Kept as its own token, value aliased to [`ACCENT`], so the brand colour can
/// diverge again later without touching call sites; the banner guard asserts
/// `LOGO == ACCENT` (and the activity line keeps drawing [`ACCENT`] directly).
pub const LOGO: Color = ACCENT;

/// **Stage tag** — the muted `丨检索记忆 / 丨正在作答 / 丨沉淀整理` badge that
/// labels each conversation item (cli 2026-09-24: 「每个思考工具调用和正文输出
/// 都要有阶段标志，饱和度拉低降低存在感」). Deliberately **low-saturation** — a
/// desaturated slate grey — so it is present for attribution but never competes
/// with the body text (colour stays a signal, §8.4).
pub const STAGE_TAG: Color = Color::Rgb(0x6B, 0x74, 0x80);

/// Single accent — Claude Code's brand orange. Used for the `❯` prompt (input
/// box + user-turn bar), the completion palette, and markdown headings.
///
/// NB page40 (cli 2026-09-24「对话历史栏目里的颜色和排版跟 CC 还有很大差距」):
/// CC is 「少即是多」 —— a red logo + white text + grey chrome (2–3 colours) ——
/// so the accent was **pulled back off** the reply `●` bullet and the header
/// brand mark (they now use [`TEXT`] / [`LOGO`]). It marks only the *interactive*
/// affordances, never the transcript body.
pub const ACCENT: Color = Color::Rgb(0xD7, 0x77, 0x57);
/// **Thinking keyword** — the emphasis inside a reasoning block (cli
/// 2026-09-24: 「CC 支持在 thinking 里高亮关键词，咱们也要有」; the same ask was
/// already made 2026-09-21 but never landed).
///
/// Reasoning is rendered in [`TEXT_MUTED`] (grey, upright — cli 2026-09-21:
/// CC's thought is grey, **not** cyan/italic); a *keyword* (a code/API token —
/// see `app::thinking_keyword_runs`) is lifted to this **soft blue**, regular
/// weight. cli 2026-09-24 (「高亮的处理你看看」) showed a CC/灵妙 side-by-side:
/// CC draws the code token (`` `ai` ``) in a periwinkle blue — measured
/// **#B1B9F9** on cli's terminal (:99 pixel probe) — while 灵妙 was lifting it
/// to the terminal default foreground (white) **+ bold**, which read as
/// plain/white body text (and clashed with the 灰=思考 rule) rather than a
/// highlight. The token now matches CC's measured value, and the `BOLD` is
/// dropped (CC's lift is colour only).
pub const THINKING_KW: Color = Color::Rgb(0xB1, 0xB9, 0xF9);
/// Soft blue for code (inline + fenced) in rendered markdown replies.
pub const CODE_FG: Color = Color::Rgb(0x9C, 0xDC, 0xFE);
/// Link colour in rendered markdown.
pub const LINK: Color = Color::Rgb(0x7F, 0xB6, 0xE6);

/// **Diff: added line** — the `+` lines of a file mutation's unified diff shown
/// in a tool card (cli 2026-09-28「参照 CC 实现代码改动时的红绿对比格式显示样式，
/// 包括写入的时候也是」).
///
/// CC names these tokens `diffAdded` / `diffRemoved` (truecolor dark:
/// `rgb(105,219,124)` / `rgb(255,168,180)`; its **ANSI** theme maps them to
/// `ansi:green` / `ansi:red`). 灵妙 is terminal-native and never paints a
/// background (see the module header), so it takes CC's *ANSI* mapping: the
/// terminal's own green / red stay legible on both a light and a dark palette,
/// whereas CC's fixed RGB pair is tuned only for a dark one.
pub const DIFF_ADD: Color = Color::Green;
/// **Diff: removed line** — the `-` lines of a file mutation's unified diff (see
/// [`DIFF_ADD`]; CC's `diffRemoved`).
pub const DIFF_REMOVE: Color = Color::Red;
/// **Diff: context line** — an unchanged line carried along for readability
/// (CC renders these in its `subtle` grey, the same weight as chrome).
pub const DIFF_CONTEXT: Color = TEXT_MUTED;

/// Braille spinner frames — the exact set CC/lingmiao animate while work runs.
pub const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// **Activity mark** — the frames the running *activity line* spins through
/// (cli 2026-09-24 ④「渐变色防焦虑那里也跟 CC 保持一致，快速截图可以看到动效」,
/// revised cli 2026-09-28).
///
/// CC 2.1.281 animates `· ✢ ✳ ✶ ✻ ✽` (asterisk/star family) in its status line.
/// That set was adopted verbatim, but the 2026-09-28 report (「橙色的动态行不太
/// 稳定…去掉里边 claude 那几个花/星星的 emoji」) exposed two defects, both
/// reproduced on :99 with a real process:
///
/// 1. **Font coverage** — `fc-list :charset=` shows `✢`(U+2722) `✳`(U+2733)
///    `✻`(U+273B) `✽`(U+273D) are **absent from every monospace font here**
///    (Noto Sans Mono has none of them; only DejaVu Sans Mono carries them), and
///    `·`(U+00B7) / `✽`(U+273D) are East-Asian **Ambiguous** (2 columns under
///    some locales). The terminal therefore substituted look-alike glyphs from a
///    proportional fallback, so the six "distinct" frames collapsed into
///    near-identical asterisks at a *varying* advance — the mark visibly wiggled
///    as it cycled (a screenshot burst showed the same star for `✶`/`✻`/`✽`).
/// 2. **Star/flower semantics** — the asterisk family reads as CC's brand
///    glyphs, which cli asked to drop.
///
/// The replacement is a **rotating arc**: all four frames are East-Asian
/// **Narrow** (exactly 1 column, no ambiguity) *and* present in both monospace
/// fonts installed here (`fc-list :charset=` verified per codepoint), so every
/// frame advances identically and no fallback font is ever consulted. Distinct
/// from the braille [`SPINNER`] used on a running **tool** card.
pub const ACTIVITY_SPINNER: [char; 4] = ['◜', '◝', '◞', '◟'];

/// The **resting** leading mark of a finished turn's tail (`◜ {动词} for Ns ·
/// done HH:MM:SS`) — the activity arc frozen at its home frame, so the tail reads
/// as "the spinner stopped".
///
/// It is deliberately the *same glyph family* as [`ACTIVITY_SPINNER`] (same font
/// coverage, same 1-column advance) and replaces the old `✻`, which had the same
/// missing-glyph/substitution problem as the spinner.
pub const TAIL_MARK: char = '◜';

/// The leading mark of an **A→B handoff notice** (cli 2026-09-30) — the
/// small solid right-pointing triangle `▸` (U+25B8).
///
/// Chosen under the same constraint as the spinner/tail marks: `fc-list
/// :charset=25b8` hits **both** monospace fonts installed here (Noto Sans Mono +
/// DejaVu Sans Mono) and its East-Asian Width is **Narrow** (exactly 1 column,
/// never ambiguous), so it never falls back to a proportional font or shifts the
/// line. Distinct from `●` (a reply/tool card) and `∴` (reasoning) so the notice
/// reads as *chrome*, not as model output.
pub const HANDOFF_MARK: char = '▸';

/// The leading mark of a handoff notice's **second** line — the one that states
/// the injection *format*. The small **white** triangle `▹` (U+25B9): the same
/// coverage/width guarantees as [`HANDOFF_MARK`], and visually its lighter
/// sibling, so the pair reads as "the fact, then the detail under it".
pub const HANDOFF_FORMAT_MARK: char = '▹';

/// The leading mark of a **wait-poll heartbeat** line (cli 2026-10-05
/// 「要进界面的，这是核心体验」) — the clock face `◷` (U+25F7).
///
/// Chosen under the same constraint as the marks above: `fc-list :charset=25f7`
/// hits the monospace face installed here (`DejaVu Sans Mono`) and its
/// East-Asian Width is **Narrow** (exactly 1 column), so it never falls back to
/// a proportional font or shifts the line. Semantically it is the one glyph in
/// this set that means *elapsed time* — which is exactly what the heartbeat
/// reports (`已等 40s · 静默 35s`). Deliberately not the braille [`SPINNER`] (a
/// tool card's own progress) nor the [`TAIL_MARK`] arc (a finished turn), so the
/// three running states stay distinguishable at a glance.
pub const WAIT_MARK: char = '◷';

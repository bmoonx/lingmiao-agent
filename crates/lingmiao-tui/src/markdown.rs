//! Markdown → ratatui lines (T12 富文本, self-rendered).
//!
//! We used to lean on `tui-markdown`, but it deliberately *keeps* the syntax
//! markers (headings emit their own `#`s, fenced code emits its ```` ``` ````)
//! and cannot emit tables or horizontal rules at all — so a reply still read as
//! raw markdown on screen. Here we parse with `pulldown-cmark` and lay the tree
//! out ourselves, which gives exactly what a reader expects:
//!
//! * headings → bold accent text, `#`s stripped;
//! * fenced code → an indented, **syntax-highlighted** block (via `syntect`),
//!   fences dropped;
//! * lists → real `•` / `1.` bullets, nested by two columns per level;
//! * tables → box-drawn with aligned columns (CJK-aware width);
//! * block quotes → a `│ ` bar; rules → a `─` line; links → underlined.

use std::sync::OnceLock;

use pulldown_cmark::{Alignment, CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;

use crate::app::display_width;
use crate::theme::{ACCENT, CODE_FG, LINK, TEXT_MUTED};
use crate::wrap::wrap_cjk;

/// Dim colour for structural chrome (quote bars, table rules, list bullets).
/// Routed through the `theme::` token layer — nobody in the widget layer picks a
/// raw `Color::` (§14-P0); it resolves to CC's muted grey, same as the reasoning.
const DIM: Color = TEXT_MUTED;
/// Width of a rendered thematic break (`---`).
const RULE_W: usize = 30;
/// Upper bound for a rendered table column's **natural** width, so one long cell
/// cannot make the table needlessly wide. It is a soft cap only: a cell that
/// exceeds it is wrapped onto extra rows, never cut — the old code truncated at
/// this width and silently dropped the rest of the text (cli 2026-10-19
/// 「表格排版会吞掉字」).
const MAX_COL: usize = 24;
/// A column never shrinks below this when squeezing a table into the pane —
/// narrower than this a cell wraps to one glyph per line, which reads worse than
/// the definition-list fallback.
const MIN_COL: usize = 4;

/// Parse `md` and render it into owned ratatui lines (tables at natural width).
pub fn markdown_lines(md: &str) -> Vec<Line<'static>> {
    markdown_lines_width(md, None)
}

/// Like [`markdown_lines`], but a **table** is sized to fit a conversation pane
/// `width` columns wide: its widest columns shrink (never below [`MIN_COL`]) and
/// a table that still cannot fit — too many columns, or the pane is very narrow —
/// degrades to a definition list instead of being shredded by the outer wrap.
///
/// `width == None` (tests, previews) renders tables at their natural width.
pub fn markdown_lines_width(md: &str, width: Option<usize>) -> Vec<Line<'static>> {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    let mut r = Renderer {
        pane_width: width,
        ..Default::default()
    };
    for ev in Parser::new_ext(md, opts) {
        r.event(ev);
    }
    r.finish()
}

/// One unordered/ordered list currently open (innermost last).
#[derive(Clone, Copy)]
struct ListState {
    ordered: bool,
    next: u64,
}

/// A fenced/indented code block being accumulated.
struct CodeBlock {
    lang: String,
    text: String,
}

/// A GFM table being accumulated before it is drawn as a grid.
#[derive(Default)]
struct Table {
    head: Vec<Vec<String>>,
    body: Vec<Vec<String>>,
    cur_row: Option<Vec<String>>,
    cur_cell: Option<String>,
    in_head: bool,
    _aligns: Vec<Alignment>,
}

#[derive(Default)]
struct Renderer {
    lines: Vec<Line<'static>>,
    /// Spans of the line currently being built.
    spans: Vec<Span<'static>>,
    /// Inline style stack (innermost last), patched cumulatively.
    style_stack: Vec<Style>,
    lists: Vec<ListState>,
    quotes: usize,
    /// Bullet/`1.` prefix awaiting the first span of the next list item line.
    pending_prefix: Option<String>,
    code: Option<CodeBlock>,
    table: Option<Table>,
    /// The conversation pane width a table must fit into (`None` = natural
    /// width, the tests / preview case).
    pane_width: Option<usize>,
}

impl Renderer {
    fn event(&mut self, ev: Event<'_>) {
        match ev {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(t) => self.text(t.as_ref()),
            Event::Code(t) => self.inline_code(t.as_ref()),
            Event::SoftBreak => self.soft_break(),
            Event::HardBreak => self.flush(),
            Event::Rule => {
                self.blank();
                self.lines.push(Line::from(Span::styled(
                    "─".repeat(RULE_W),
                    Style::default().fg(DIM),
                )));
            }
            Event::TaskListMarker(done) => self.push_span(Span::styled(
                if done { "☑ " } else { "☐ " }.to_string(),
                Style::default().fg(DIM),
            )),
            Event::InlineMath(t) | Event::DisplayMath(t) => self.text(t.as_ref()),
            // Raw HTML and footnote refs have no useful terminal rendering.
            Event::Html(_) | Event::InlineHtml(_) | Event::FootnoteReference(_) => {}
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {
                if self.lists.is_empty() {
                    self.blank();
                }
            }
            Tag::Heading { .. } => {
                self.blank();
                self.style_stack
                    .push(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD));
            }
            Tag::BlockQuote(_) => {
                self.blank();
                self.quotes += 1;
            }
            Tag::CodeBlock(kind) => {
                self.blank();
                let lang = match kind {
                    CodeBlockKind::Fenced(l) => l.to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                self.code = Some(CodeBlock {
                    lang,
                    text: String::new(),
                });
            }
            Tag::List(start) => {
                if self.lists.is_empty() {
                    self.blank();
                }
                self.lists.push(ListState {
                    ordered: start.is_some(),
                    next: start.unwrap_or(1),
                });
            }
            Tag::Item => {
                self.flush();
                self.pending_prefix = Some(self.list_marker());
            }
            Tag::Table(aligns) => {
                self.blank();
                self.table = Some(Table {
                    _aligns: aligns,
                    ..Default::default()
                });
            }
            Tag::TableHead => {
                if let Some(t) = &mut self.table {
                    t.in_head = true;
                    t.cur_row = Some(Vec::new());
                }
            }
            Tag::TableRow => {
                if let Some(t) = &mut self.table {
                    t.cur_row = Some(Vec::new());
                }
            }
            Tag::TableCell => {
                if let Some(t) = &mut self.table {
                    t.cur_cell = Some(String::new());
                }
            }
            Tag::Emphasis => self
                .style_stack
                .push(Style::default().add_modifier(Modifier::ITALIC)),
            Tag::Strong => self
                .style_stack
                .push(Style::default().add_modifier(Modifier::BOLD)),
            Tag::Strikethrough => self
                .style_stack
                .push(Style::default().add_modifier(Modifier::CROSSED_OUT)),
            Tag::Link { .. } => self
                .style_stack
                .push(Style::default().fg(LINK).add_modifier(Modifier::UNDERLINED)),
            Tag::Image { .. } => self
                .style_stack
                .push(Style::default().fg(LINK).add_modifier(Modifier::ITALIC)),
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.flush(),
            TagEnd::Heading(_) => {
                self.style_stack.pop();
                self.flush();
            }
            TagEnd::BlockQuote(_) => {
                self.flush();
                self.quotes = self.quotes.saturating_sub(1);
            }
            TagEnd::CodeBlock => self.end_code(),
            TagEnd::List(_) => {
                self.flush();
                self.lists.pop();
            }
            TagEnd::Item => self.flush(),
            TagEnd::Table => {
                if let Some(t) = self.table.take() {
                    self.render_table(&t);
                }
            }
            TagEnd::TableHead => {
                if let Some(t) = &mut self.table {
                    t.in_head = false;
                    if let Some(row) = t.cur_row.take() {
                        t.head.push(row);
                    }
                }
            }
            TagEnd::TableRow => {
                if let Some(t) = &mut self.table {
                    if let Some(row) = t.cur_row.take() {
                        t.body.push(row);
                    }
                }
            }
            TagEnd::TableCell => {
                if let Some(t) = &mut self.table {
                    if let (Some(cell), Some(row)) = (t.cur_cell.take(), t.cur_row.as_mut()) {
                        row.push(cell);
                    }
                }
            }
            TagEnd::Emphasis
            | TagEnd::Strong
            | TagEnd::Strikethrough
            | TagEnd::Link
            | TagEnd::Image => {
                self.style_stack.pop();
            }
            _ => {}
        }
    }

    /// A `Text` node — inside a table cell or code block it is buffered, else it
    /// is a styled span on the current line.
    fn text(&mut self, t: &str) {
        if let Some(tb) = &mut self.table {
            if let Some(cell) = &mut tb.cur_cell {
                cell.push_str(t);
                return;
            }
        }
        if let Some(cb) = &mut self.code {
            cb.text.push_str(t);
            return;
        }
        let style = self.inline_style();
        self.push_span(Span::styled(t.to_string(), style));
    }

    fn inline_code(&mut self, t: &str) {
        if let Some(tb) = &mut self.table {
            if let Some(cell) = &mut tb.cur_cell {
                cell.push_str(t);
                return;
            }
        }
        self.push_span(Span::styled(t.to_string(), Style::default().fg(CODE_FG)));
    }

    /// A soft break joins with a space (CommonMark); hard breaks flush.
    fn soft_break(&mut self) {
        if !self.spans.is_empty() {
            self.spans.push(Span::raw(" "));
        }
    }

    fn inline_style(&self) -> Style {
        let mut s = Style::default();
        for st in &self.style_stack {
            s = s.patch(*st);
        }
        s
    }

    /// Push a span, first emitting any pending list-item prefix.
    fn push_span(&mut self, span: Span<'static>) {
        if self.spans.is_empty() {
            if let Some(p) = self.pending_prefix.take() {
                self.spans.push(Span::styled(p, Style::default().fg(DIM)));
            }
        }
        self.spans.push(span);
    }

    /// Close the current line (prefixed with any block-quote bar) into `lines`.
    fn flush(&mut self) {
        if self.spans.is_empty() {
            return;
        }
        let mut spans = std::mem::take(&mut self.spans);
        if self.quotes > 0 {
            let mut pre = vec![Span::styled(
                "│ ".repeat(self.quotes),
                Style::default().fg(DIM),
            )];
            pre.append(&mut spans);
            spans = pre;
        }
        self.lines.push(Line::from(spans));
    }

    /// Ensure a blank separator line sits between two blocks (never leading).
    fn blank(&mut self) {
        self.flush();
        let last_blank = self
            .lines
            .last()
            .map(|l| l.spans.iter().all(|s| s.content.trim().is_empty()))
            .unwrap_or(true);
        if !self.lines.is_empty() && !last_blank {
            self.lines.push(Line::default());
        }
    }

    fn list_marker(&mut self) -> String {
        let depth = self.lists.len().saturating_sub(1);
        let indent = "  ".repeat(depth);
        match self.lists.last_mut() {
            Some(l) if l.ordered => {
                let m = format!("{}. ", l.next);
                l.next += 1;
                format!("{indent}{m}")
            }
            _ => format!("{indent}• "),
        }
    }

    fn end_code(&mut self) {
        let Some(cb) = self.code.take() else {
            return;
        };
        let text = cb.text.trim_end_matches('\n');
        if text.is_empty() {
            return;
        }
        self.lines.extend(highlight_code(&cb.lang, text));
    }

    /// Draw the accumulated table as a box-drawn grid with aligned columns.
    ///
    /// Two hard rules, both learned from cli 2026-10-19「表格排版会吞掉字」:
    ///
    /// 1. **Nothing is ever dropped.** A cell wider than its column is *wrapped*
    ///    onto extra grid rows (the old code ran `truncate(cell, width)` and threw
    ///    the remainder away — a long Chinese cell lost half its text with no
    ///    ellipsis, so the table silently lied).
    /// 2. **The table fits the pane.** Columns are sized from the content, capped
    ///    at [`MAX_COL`], then squeezed proportionally until the whole grid fits
    ///    `pane_width` (never below [`MIN_COL`]). If it still cannot fit — too
    ///    many columns for the pane, or a very narrow pane — the table degrades to
    ///    a definition list (`head: value` lines) rather than being shredded by
    ///    the outer line wrap into unreadable fragments.
    fn render_table(&mut self, t: &Table) {
        let rows: Vec<&Vec<String>> = t.head.iter().chain(t.body.iter()).collect();
        if rows.is_empty() {
            return;
        }
        let cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
        if cols == 0 {
            return;
        }
        // Natural width per column: the widest cell. The cap is the pane itself
        // (a cell may not be wider than the pane allows), so a wide terminal lets a
        // long cell stay on one row instead of being folded at an arbitrary
        // [`MAX_COL`]; the squeezing below then shares the pane fairly between the
        // columns. Without a pane (tests / previews) the fixed [`MAX_COL`] cap
        // keeps an essay cell from making the grid absurdly wide.
        let cap = self.pane_width.unwrap_or(MAX_COL);
        let mut widths = vec![0usize; cols];
        for row in &rows {
            for (i, c) in row.iter().enumerate() {
                widths[i] = widths[i].max(display_width(c).min(cap));
            }
        }
        for w in widths.iter_mut() {
            *w = (*w).max(1);
        }
        // Fit the pane. The grid is `left │ c │ c │ right`: `1 + Σ(w + 2) + cols`
        // columns, i.e. `Σ(w) + 3*cols + 1` — every cell carries one space of
        // padding either side *and* the separators are their own columns.
        if let Some(pane) = self.pane_width {
            let chrome = 3 * cols + 1;
            let available = pane.saturating_sub(chrome);
            if available < MIN_COL * cols || available == 0 {
                self.render_table_as_list(t, &rows);
                return;
            }
            let sum: usize = widths.iter().sum();
            if sum > available {
                widths = squeeze(&widths, available, MIN_COL);
            }
        }
        let border = Style::default().fg(DIM);
        self.lines.push(Line::from(Span::styled(
            hline('┌', '┬', '┐', &widths),
            border,
        )));
        let head_n = t.head.len();
        for (ri, row) in rows.iter().enumerate() {
            if ri == head_n && head_n > 0 {
                self.lines.push(Line::from(Span::styled(
                    hline('├', '┼', '┤', &widths),
                    border,
                )));
            }
            // Wrap every cell to its column width; the row is as tall as its
            // tallest cell, and short cells pad out on their own lines.
            let cells: Vec<Vec<String>> = widths
                .iter()
                .enumerate()
                .map(|(i, w)| {
                    let raw = row.get(i).map(String::as_str).unwrap_or("");
                    // A cell's own newlines would break the grid — flatten them.
                    let flat = raw.split('\n').collect::<Vec<_>>().join(" ");
                    wrap_cjk(flat.trim(), *w)
                })
                .collect();
            let height = cells.iter().map(Vec::len).max().unwrap_or(1).max(1);
            for li in 0..height {
                let mut spans = vec![Span::styled("│", border)];
                for (i, w) in widths.iter().enumerate() {
                    let text = cells[i].get(li).map(String::as_str).unwrap_or("");
                    let padded = pad(text, *w);
                    let style = if ri < head_n {
                        Style::default().add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    };
                    spans.push(Span::styled(format!(" {padded} "), style));
                    spans.push(Span::styled("│", border));
                }
                self.lines.push(Line::from(spans));
            }
        }
        self.lines.push(Line::from(Span::styled(
            hline('└', '┴', '┘', &widths),
            border,
        )));
    }

    /// The degraded rendering for a table too wide for the pane: each row becomes
    /// a definition list — `**{head}**：{cell}` — so **every** value survives as
    /// readable text (wrapped by the outer `wrap_line`) instead of the grid being
    /// torn into fragments (`cli 2026-10-19` 吞字).
    fn render_table_as_list(&mut self, t: &Table, rows: &[&Vec<String>]) {
        let heads: Vec<String> = (0..t.head.first().map(Vec::len).unwrap_or(0))
            .map(|i| {
                t.head
                    .first()
                    .and_then(|h| h.get(i))
                    .cloned()
                    .unwrap_or_default()
            })
            .collect();
        let body_start = t.head.len();
        for row in rows.iter().skip(body_start) {
            if row.iter().all(|c| c.trim().is_empty()) {
                continue;
            }
            for (i, cell) in row.iter().enumerate() {
                let cell = cell.trim();
                if cell.is_empty() {
                    continue;
                }
                let label = heads.get(i).map(String::as_str).unwrap_or("").trim();
                let mut spans: Vec<Span<'static>> = Vec::new();
                if label.is_empty() {
                    spans.push(Span::styled("• ", Style::default().fg(DIM)));
                } else {
                    spans.push(Span::styled("• ", Style::default().fg(DIM)));
                    spans.push(Span::styled(
                        format!("{label}："),
                        Style::default().add_modifier(Modifier::BOLD),
                    ));
                }
                spans.push(Span::raw(cell.to_string()));
                self.lines.push(Line::from(spans));
            }
        }
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        self.flush();
        self.lines
    }
}

/// A box-drawing horizontal rule for a table (`left─┬─…─right`).
fn hline(left: char, sep: char, right: char, widths: &[usize]) -> String {
    let mut s = String::new();
    s.push(left);
    for (i, w) in widths.iter().enumerate() {
        s.push_str(&"─".repeat(w + 2));
        s.push(if i + 1 == widths.len() { right } else { sep });
    }
    s
}

/// Shrink `widths` so their sum is at most `available`, taking the excess from
/// the widest columns first and never dropping a column below `min`.
///
/// Proportional squeezing alone would starve a narrow column (one CJK glyph per
/// line); taking one column at a time from the current widest keeps the columns
/// balanced, so every cell still wraps at a readable width.
fn squeeze(widths: &[usize], available: usize, min: usize) -> Vec<usize> {
    let mut out = widths.to_vec();
    // Shrink in display columns; a column cannot go under `min`, so first give
    // back the "slack" above `min`, widest column first.
    let mut excess = out.iter().sum::<usize>().saturating_sub(available);
    while excess > 0 {
        let Some(fattest) = out
            .iter()
            .enumerate()
            .filter(|(_, w)| **w > min)
            .max_by_key(|(i, w)| (**w, std::cmp::Reverse(*i)))
            .map(|(i, _)| i)
        else {
            break;
        };
        out[fattest] -= 1;
        excess -= 1;
    }
    out
}

/// Right-pad `s` to `width` *display* columns (CJK counted as 2).
fn pad(s: &str, width: usize) -> String {
    let w = display_width(s);
    if w >= width {
        s.to_string()
    } else {
        format!("{s}{}", " ".repeat(width - w))
    }
}

/// Syntax-highlight a code block into indented ratatui lines.
///
/// Falls back to a plain code-coloured block when the language is unknown or
/// `syntect` errors, so nothing is ever lost.
fn highlight_code(lang: &str, code: &str) -> Vec<Line<'static>> {
    let ss = syntaxes();
    let syntax = if lang.is_empty() {
        ss.find_syntax_plain_text()
    } else {
        ss.find_syntax_by_token(lang)
            .or_else(|| ss.find_syntax_by_extension(lang))
            .unwrap_or_else(|| ss.find_syntax_plain_text())
    };
    let mut highlighter = HighlightLines::new(syntax, theme());
    let mut out = Vec::new();
    for line in LinesWithEndings::from(code) {
        let ranges = match highlighter.highlight_line(line, ss) {
            Ok(r) => r,
            Err(_) => {
                out.push(plain_code(line));
                continue;
            }
        };
        let mut spans = vec![Span::styled("  ".to_string(), Style::default())];
        for (st, seg) in ranges {
            let seg = seg.trim_end_matches(['\n', '\r']);
            if seg.is_empty() {
                continue;
            }
            spans.push(Span::styled(
                seg.to_string(),
                Style::default().fg(Color::Rgb(
                    st.foreground.r,
                    st.foreground.g,
                    st.foreground.b,
                )),
            ));
        }
        out.push(Line::from(spans));
    }
    if out.is_empty() {
        out.push(plain_code(code));
    }
    out
}

fn plain_code(line: &str) -> Line<'static> {
    Line::from(Span::styled(
        format!("  {}", line.trim_end_matches(['\n', '\r'])),
        Style::default().fg(CODE_FG),
    ))
}

/// The embedded default syntax set (built once per process).
fn syntaxes() -> &'static SyntaxSet {
    static SS: OnceLock<SyntaxSet> = OnceLock::new();
    SS.get_or_init(SyntaxSet::load_defaults_newlines)
}

/// A dark default theme (built once); we only read its foreground colours, so
/// the terminal's own background still shows through.
fn theme() -> &'static Theme {
    static TS: OnceLock<Theme> = OnceLock::new();
    TS.get_or_init(|| {
        let set = ThemeSet::load_defaults();
        set.themes
            .get("base16-ocean.dark")
            .or_else(|| set.themes.values().next())
            .cloned()
            .expect("syntect ships at least one default theme")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every span foreground in a rendered reply is either the default (`None`,
    /// the bright body) or a colour that came **through the `theme::` token
    /// layer** (§14-P0) — no widget-layer code picks a raw `Color::`. The
    /// structural *chrome* (list bullets, block-quote bars, rules) is routed to
    /// [`crate::theme::TEXT_MUTED`] and inline code to [`crate::theme::CODE_FG`].
    #[test]
    fn markdown_chrome_uses_theme_tokens_not_raw_colours() {
        use crate::theme::{ACCENT, CODE_FG, LINK, TEXT_MUTED};
        let md = "- one\n- two\n\n> quoted\n\n`inline`\n\n---\n";
        let lines = markdown_lines(md);
        let allowed = [TEXT_MUTED, CODE_FG, LINK, ACCENT];
        let fgs: Vec<Color> = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .filter_map(|s| s.style.fg)
            .collect();
        for fg in &fgs {
            assert!(
                allowed.contains(fg),
                "raw colour leaked into markdown chrome: {fg:?} (all: {fgs:?})"
            );
        }
        assert!(
            fgs.contains(&TEXT_MUTED),
            "bullets / quote bar / rule are muted"
        );
        assert!(fgs.contains(&CODE_FG), "inline code is CODE_FG");
    }

    /// All rendered text with the grid chrome (`│ ┌ ┬ ┐ ├ ┼ ┤ └ ┴ ┘ ─`) removed,
    /// so a cell can be searched for even when it wrapped across grid rows.
    fn table_text(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect::<String>()
            .chars()
            .filter(|c| !"│┌┬┐├┼┤└┴┘─".contains(*c))
            .collect()
    }

    /// A cell wider than its column is **wrapped**, never cut: the old code ran
    /// `truncate(cell, width)` and threw the tail away with no ellipsis, so a long
    /// Chinese cell silently lost half its text (cli 2026-10-19「表格排版会吞掉字」).
    #[test]
    fn table_cells_wrap_instead_of_being_truncated() {
        let md = "| 项目 | 说明 |\n|---|---|\n| 引擎硬拦 | 工具白名单、先读后写、路径沙箱三道物理拦截 |\n";
        let joined = table_text(&markdown_lines(md));
        assert!(
            joined.contains("工具白名单、先读后写、路"),
            "cell keeps its first line: {joined:?}"
        );
        assert!(
            joined.contains("径沙箱三道物理拦截"),
            "the overflow is wrapped onto a second row, not dropped: {joined:?}"
        );
    }

    /// A table is laid out to fit the conversation pane: no rendered line is
    /// wider than the width it was given (a wider grid used to be torn apart by
    /// the outer `wrap_line`, splitting cells mid-glyph).
    #[test]
    fn a_table_fits_the_pane_it_is_given() {
        let md = "| 阶段 | 工具数 | 说明 | 备注 | 来源 | 时间 | 状态 |\n|---|---|---|---|---|---|---|\n| 组织上下文 | 14 | 检索记忆与约束，每轮都跑一遍 | 见设计文档第八节 | 流水线 | 2026-10-19 | 已完成 |\n";
        for width in [60usize, 80, 120] {
            for line in markdown_lines_width(md, Some(width)) {
                let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
                let w = display_width(&text);
                assert!(w <= width, "line {w} > pane {width}: {text:?}");
            }
        }
    }

    /// When the grid genuinely cannot fit — more columns than the pane can hold —
    /// the table degrades to a definition list so every value stays readable
    /// instead of the grid being shredded into fragments.
    #[test]
    fn a_too_wide_table_degrades_to_a_list() {
        let md = "| 一 | 二 | 三 | 四 | 五 | 六 | 七 | 八 | 九 | 十 | 十一 | 十二 |\n|---|---|---|---|---|---|---|---|---|---|---|---|\n| 甲 | 乙 | 丙 | 丁 | 戊 | 己 | 庚 | 辛 | 壬 | 癸 | 子 | 丑 |\n";
        let lines = markdown_lines_width(md, Some(40));
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
        assert!(!joined.contains('┌'), "no grid when it cannot fit");
        assert!(joined.contains("• 一：甲"), "head: value list: {joined:?}");
        assert!(joined.contains("• 十二：丑"), "last column kept too");
    }

    /// Every character of every cell survives even when the pane is narrow enough
    /// to wrap the grid hard — the old truncation lost text in exactly this case.
    #[test]
    fn a_narrow_pane_still_keeps_every_cell_character() {
        let md = "| 阶段 | 工具数 | 说明 | 备注 |\n|---|---|---|---|\n| 组织上下文 | 14 | 检索记忆 | 见文档 |\n";
        let joined = table_text(&markdown_lines_width(md, Some(30)));
        for piece in ["组", "织", "上", "下", "文", "检索", "记忆", "见文", "档"] {
            assert!(joined.contains(piece), "{piece} kept: {joined:?}");
        }
    }

    /// A block quote dims only its `│ ` bar — the quoted *text* stays primary.
    /// This is the §12.15 "chrome is grey, body is bright" split applied to
    /// markdown: the structural marker is muted, the data a reader reads is not.
    #[test]
    fn blockquote_text_is_primary_not_dimmed() {
        use crate::theme::TEXT_MUTED;
        let lines = markdown_lines("> hello\n");
        let fg_of = |needle: &str| {
            lines
                .iter()
                .flat_map(|l| l.spans.iter())
                .find(|s| s.content.contains(needle))
                .and_then(|s| s.style.fg)
        };
        assert_eq!(fg_of("│"), Some(TEXT_MUTED), "quote bar is chrome (muted)");
        assert_eq!(fg_of("hello"), None, "quoted text stays primary (default)");
    }
}

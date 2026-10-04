//! CJK-aware line breaking (§12.9).
//!
//! ratatui's own `Wrap` breaks at *character* granularity — it splits ASCII
//! words mid-token and knows nothing about CJK 禁则 (避头尾), so `。`/`，` can land
//! at the start of a line and `（`/`「` at its end. This module implements the
//! small rule set our text actually needs (CJK + ASCII), with zero extra
//! dependencies:
//!
//! * **Token-based**: a run of ASCII non-space characters is a single
//!   unbreakable token (a word); every other non-space character is its own
//!   token. A break prefers whitespace and only cuts a word when the word alone
//!   is wider than the line.
//! * **CJK 禁则**: a *closing* punctuation ([`CLOSING`]) must never start a line
//!   and an *opening* one ([`OPENING`]) never ends one — on a would-be violation
//!   the break backtracks one token, so the punctuation keeps a neighbour.
//! * **Hanging indent**: [`wrap_cjk_hanging`] indents a list item's continuation
//!   lines under its text (the markdown-list case, §12.9 列表分行).
//!
//! Pure and dependency-free (it reuses [`crate::app::display_width`]), so the
//! breaking rules are unit-testable without a terminal — the same reasoning as
//! [`crate::editor`] and [`crate::motion`].

use crate::app::{char_width, display_width};

/// Closing punctuation (行首禁则): must never *start* a line.
pub const CLOSING: &str = "。，、；：！？）」』】》〉”’…—～%）]}｝>,.!?;:";

/// Opening punctuation (行尾禁则): must never *end* a line.
pub const OPENING: &str = "（「『【《〈“‘([{｛<";

/// CJK-aware hard wrap — the replacement for the old char-granular
/// `app::wrap_text` (§12.9). Trailing spaces at a break are dropped, so every
/// returned segment is exactly what should be printed.
pub fn wrap_cjk(text: &str, width: usize) -> Vec<String> {
    wrap_raw(text, width)
        .into_iter()
        .map(|s| s.trim_end_matches(' ').to_string())
        .collect()
}

/// Like [`wrap_cjk`] but keeps a break's **trailing spaces** (cli 2026-10-04
/// 「按完空格后空格进去了但是不显示，输入下一个字符才显示」).
///
/// `wrap_cjk` drops trailing spaces, which is right for prose (invisible anyway,
/// and it keeps a wrapped paragraph from looking ragged) but wrong for the
/// **input box**: a space the user just typed is real content. The editor's
/// render path ([`crate::app::App::editor_visual_lines`]) and its cursor path
/// ([`crate::app::visual_cursor_row_col`]) both went through `wrap_cjk`, so
/// trailing the text with a space changed *nothing* on screen — neither the
/// glyph nor the caret column — until the next character arrived and separated
/// the space from the line end.
pub fn wrap_cjk_keep(text: &str, width: usize) -> Vec<String> {
    wrap_raw(text, width)
}

/// Wrap a **list item** so its continuation lines are indented (`hanging`
/// columns) under the item text instead of starting flush at the margin (§12.9
/// 列表分行). `text` is the whole line (marker + content).
pub fn wrap_cjk_hanging(text: &str, width: usize, hanging: usize) -> Vec<String> {
    // The marker occupies `hanging` columns on line 0 and the continuation indent
    // occupies the same width on later lines — so every line's *content* has the
    // same capacity.
    let content_width = width.saturating_sub(hanging).max(1);
    let mut segs = wrap_cjk(text, content_width);
    let indent = " ".repeat(hanging);
    for (i, s) in segs.iter_mut().enumerate() {
        if i > 0 {
            *s = format!("{indent}{s}");
        }
    }
    segs
}

/// Raw hard wrap that keeps explicit newlines and trailing spaces.
fn wrap_raw(text: &str, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    for paragraph in text.split('\n') {
        if paragraph.is_empty() {
            out.push(String::new());
            continue;
        }
        out.extend(wrap_paragraph(paragraph, width));
    }
    out
}

/// Greedy token fill of one paragraph (no explicit newlines).
fn wrap_paragraph(paragraph: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut tokens = tokenize(paragraph);
    let mut lines: Vec<String> = Vec::new();
    let mut cur: Vec<String> = Vec::new();
    let mut cur_w = 0usize;
    let mut i = 0usize;
    while i < tokens.len() {
        let tw = display_width(&tokens[i]);
        if cur_w + tw <= width {
            cur_w += tw;
            cur.push(tokens[i].clone());
            i += 1;
            continue;
        }
        if cur.is_empty() {
            // A word wider than the whole line has to be cut (no break point).
            let (head, rest) = split_at_width(&tokens[i], width);
            lines.push(head);
            if rest.is_empty() {
                i += 1;
            } else {
                tokens[i] = rest;
            }
            continue;
        }
        // The next token does not fit. A closing punctuation must not start a
        // line and an opening one must not end one: backtrack a token so the
        // punctuation keeps a neighbour (禁则, §12.9). Only possible when the
        // current line has a token to spare.
        let pull = (is_closing_punct(&tokens[i]) || is_opening_punct(cur.last().unwrap()))
            && cur.len() >= 2;
        if pull {
            let last = cur.pop().unwrap();
            lines.push(cur.join(""));
            cur.clear();
            cur_w = display_width(&last);
            cur.push(last);
            continue; // re-examine the same token on the fresh line
        }
        lines.push(std::mem::take(&mut cur).join(""));
        cur_w = 0;
        // `i` unchanged — the token starts the next line.
    }
    if !cur.is_empty() {
        lines.push(cur.join(""));
    }
    lines
}

/// Split a paragraph into layout tokens: an ASCII non-space run is one word;
/// every other character (CJK, full-width punctuation, emoji, space) is its own
/// token. Spaces are individual tokens so they act as preferred break points.
fn tokenize(paragraph: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut word = String::new();
    for ch in paragraph.chars() {
        if ch.is_ascii() && ch != ' ' && ch != '\t' {
            word.push(ch);
        } else {
            if !word.is_empty() {
                out.push(std::mem::take(&mut word));
            }
            out.push(ch.to_string());
        }
    }
    if !word.is_empty() {
        out.push(word);
    }
    out
}

/// A single-character token drawn from `set` (the punctuation tables are all
/// single characters, so a multi-char token — an ASCII word — never matches).
fn is_single_in(tok: &str, set: &str) -> bool {
    let mut chars = tok.chars();
    matches!((chars.next(), chars.next()), (Some(c), None) if set.contains(c))
}

fn is_closing_punct(tok: &str) -> bool {
    is_single_in(tok, CLOSING)
}

fn is_opening_punct(tok: &str) -> bool {
    is_single_in(tok, OPENING)
}

/// Split `s` into `(head, rest)` where `head` is the longest prefix whose display
/// width is ≤ `width`.
fn split_at_width(s: &str, width: usize) -> (String, String) {
    let mut head = String::new();
    let mut w = 0usize;
    let mut rest = String::new();
    let mut full = false;
    for ch in s.chars() {
        if !full {
            let cw = char_width(ch);
            if w + cw <= width {
                head.push(ch);
                w += cw;
                continue;
            }
            full = true;
        }
        rest.push(ch);
    }
    (head, rest)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The set of characters that must never open a line.
    fn starts_with_closing(s: &str) -> bool {
        s.chars().next().is_some_and(|c| CLOSING.contains(c))
    }

    fn ends_with_opening(s: &str) -> bool {
        s.chars().next_back().is_some_and(|c| OPENING.contains(c))
    }

    #[test]
    fn does_not_split_ascii_word() {
        // `hello` (5) + space + `world` (5) does not fit 8 → break at the space,
        // never inside a word.
        assert_eq!(wrap_cjk("hello world", 8), vec!["hello", "world"]);
        assert_eq!(wrap_cjk("abcdefg hi", 8), vec!["abcdefg", "hi"]);
    }

    #[test]
    fn closing_punct_never_starts_a_line() {
        // `。` may not open a line → the break backtracks, keeping `界` with it.
        let lines = wrap_cjk("你好世界。", 8);
        assert!(lines.iter().all(|l| !starts_with_closing(l)), "{lines:?}");
        assert_eq!(lines, vec!["你好世", "界。"]);
        // … and with more text after the punctuation.
        let lines = wrap_cjk("一二三四五。六七", 6);
        assert!(lines.iter().all(|l| !starts_with_closing(l)), "{lines:?}");
    }

    #[test]
    fn opening_punct_never_ends_a_line() {
        // `（` may not close a line → it is carried down to the next line.
        let lines = wrap_cjk("一（二", 4);
        assert!(lines.iter().all(|l| !ends_with_opening(l)), "{lines:?}");
        assert_eq!(lines, vec!["一", "（二"]);
    }

    #[test]
    fn long_unbreakable_word_hard_breaks() {
        let lines = wrap_cjk("abcdefghijklmnopqrst", 8);
        assert_eq!(lines, vec!["abcdefgh", "ijklmnop", "qrst"]);
        assert!(lines.iter().all(|l| display_width(l) <= 8));
    }

    #[test]
    fn every_segment_fits_width() {
        let samples = [
            "你好世界。",
            "hello world 你好",
            "一二三四五六七八九十",
            "ab cd ef gh ij",
            "末尾标点。，",
        ];
        for w in [4usize, 6, 8, 12] {
            for s in samples {
                for seg in wrap_cjk(s, w) {
                    assert!(
                        display_width(&seg) <= w,
                        "{s:?} wrapped at {w} produced an over-wide segment {seg:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn list_continuation_is_indented() {
        let lines = wrap_cjk_hanging("  - 这是一段很长的列表项内容", 12, 4);
        assert!(lines.len() > 1, "the item must wrap: {lines:?}");
        assert!(lines[0].starts_with("  - "), "marker kept: {lines:?}");
        for l in lines.iter().skip(1) {
            assert!(l.starts_with("    "), "continuation indented 4: {l:?}");
            assert!(display_width(l) <= 12, "indented line fits: {l:?}");
        }
    }
}

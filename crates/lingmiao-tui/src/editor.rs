//! A small multi-line **line editor** for the input box (TUI 打磨 v2 · ②).
//!
//! `App` holds one of these instead of a bare `String`: the cursor is a
//! **character index** (never a byte offset), so CJK / emoji can't land the
//! cursor mid-codepoint and panic a `String::insert` (§② design note 8), and
//! every edit path — insert / backspace / delete / kill-line / kill-word — is a
//! pure method that is unit-tested here, leaving `app.rs` to do only the
//! display. This mirrors the `motion.rs` split (§② design note 9): pure logic
//! lives beside its tests; the render/event layer just calls in.

/// Undo depth. Deep enough that a mis-typed paragraph is recoverable, shallow
/// enough that a long session can't grow without bound.
const UNDO_CAP: usize = 100;

/// A multi-line text buffer with a character-index cursor and a bounded undo
/// stack (§② 撤销栈).
#[derive(Debug, Clone, Default)]
pub struct Editor {
    /// The buffer text; may contain `\n` (Shift+Enter / Ctrl+J split lines).
    pub text: String,
    /// Cursor position as a **character** index into [`Self::text`]
    /// (`0..=text.chars().count()`). Character-indexed on purpose — a byte
    /// offset would let a CJK edit split a code point.
    pub cursor: usize,
    /// Undo snapshots `(text, cursor)`, oldest first, capped at [`UNDO_CAP`].
    undo: Vec<(String, usize)>,
    /// Preferred column while moving vertically (in chars): ↑/↓ keep the column
    /// across shorter lines and return to it on the way back. Cleared by any
    /// horizontal move or edit.
    col_hint: Option<usize>,
}

impl Editor {
    /// An empty editor with the cursor at 0.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the buffer is empty (drives the empty-box tip, §14-P3).
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Number of characters in the buffer (the cursor's upper bound).
    pub fn char_len(&self) -> usize {
        self.text.chars().count()
    }

    /// Number of lines (newline count + 1) — the input box grows with this.
    pub fn line_count(&self) -> usize {
        self.text.matches('\n').count() + 1
    }

    /// Byte offset of the `n`-th character (`text.len()` when `n` is past the
    /// end) — the bridge from the char-index cursor to `String` slicing.
    fn byte_at(&self, n: usize) -> usize {
        self.text
            .char_indices()
            .nth(n)
            .map(|(b, _)| b)
            .unwrap_or(self.text.len())
    }

    /// Snapshot the current state onto the undo stack (called *before* a
    /// mutation). Drops the oldest entry past [`UNDO_CAP`].
    fn snapshot(&mut self) {
        self.undo.push((self.text.clone(), self.cursor));
        if self.undo.len() > UNDO_CAP {
            self.undo.remove(0);
        }
    }

    /// Replace the whole buffer and put the cursor at the end (history recall /
    /// draft restore). Not itself undoable — it is a navigation, not an edit.
    pub fn set_text(&mut self, s: impl Into<String>) {
        self.text = s.into();
        self.cursor = self.char_len();
        self.col_hint = None;
    }

    /// Empty the buffer and forget its undo history.
    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.col_hint = None;
        self.undo.clear();
    }

    /// Undo the last edit; returns whether anything was restored.
    pub fn undo(&mut self) -> bool {
        match self.undo.pop() {
            Some((text, cursor)) => {
                self.text = text;
                self.cursor = cursor.min(self.char_len());
                self.col_hint = None;
                true
            }
            None => false,
        }
    }

    /// Insert one character at the cursor and advance past it.
    pub fn insert_char(&mut self, c: char) {
        self.snapshot();
        let at = self.byte_at(self.cursor);
        self.text.insert(at, c);
        self.cursor += 1;
        self.col_hint = None;
    }

    /// Insert a string at the cursor (bracketed paste) and advance past it.
    pub fn insert_str(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        self.snapshot();
        let at = self.byte_at(self.cursor);
        self.text.insert_str(at, s);
        self.cursor += s.chars().count();
        self.col_hint = None;
    }

    /// Delete the character **before** the cursor (Backspace). No-op at 0.
    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.snapshot();
        let start = self.byte_at(self.cursor - 1);
        let end = self.byte_at(self.cursor);
        self.text.replace_range(start..end, "");
        self.cursor -= 1;
        self.col_hint = None;
    }

    /// Delete the character **at** the cursor (Delete); the cursor stays put.
    pub fn delete(&mut self) {
        if self.cursor >= self.char_len() {
            return;
        }
        self.snapshot();
        let start = self.byte_at(self.cursor);
        let end = self.byte_at(self.cursor + 1);
        self.text.replace_range(start..end, "");
        self.col_hint = None;
    }

    /// Move the cursor one character left (clamped at 0).
    pub fn move_left(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
        }
        self.col_hint = None;
    }

    /// Move the cursor one character right (clamped at the end).
    pub fn move_right(&mut self) {
        if self.cursor < self.char_len() {
            self.cursor += 1;
        }
        self.col_hint = None;
    }

    /// `(row, col)` of the cursor: `row` = newlines before it, `col` = characters
    /// from the current line's start (the *display* width is the caller's job).
    pub fn line_col(&self) -> (usize, usize) {
        let before = &self.text[..self.byte_at(self.cursor)];
        let row = before.matches('\n').count();
        let col = before.chars().rev().take_while(|&c| c != '\n').count();
        (row, col)
    }

    /// Character length of line `row`.
    fn line_len(&self, row: usize) -> usize {
        self.text
            .split('\n')
            .nth(row)
            .map(|l| l.chars().count())
            .unwrap_or(0)
    }

    /// Move to the start of the current line (Home / Ctrl+A).
    pub fn move_home(&mut self) {
        let (_, col) = self.line_col();
        self.cursor -= col;
        self.col_hint = None;
    }

    /// Move to the end of the current line (End / Ctrl+E).
    pub fn move_end(&mut self) {
        let (row, col) = self.line_col();
        self.cursor += self.line_len(row) - col;
        self.col_hint = None;
    }

    /// Move up one line, preserving the column preference across shorter lines.
    pub fn move_up(&mut self) {
        let (row, col) = self.line_col();
        if row == 0 {
            return;
        }
        let hint = *self.col_hint.get_or_insert(col);
        let line_start = self.cursor - col;
        let prev_len = self.line_len(row - 1);
        let prev_start = line_start - 1 - prev_len;
        self.cursor = prev_start + hint.min(prev_len);
    }

    /// Move down one line, preserving the column preference across shorter lines.
    pub fn move_down(&mut self) {
        let (row, col) = self.line_col();
        if row + 1 >= self.line_count() {
            return;
        }
        let hint = *self.col_hint.get_or_insert(col);
        let line_start = self.cursor - col;
        let next_start = line_start + self.line_len(row) + 1;
        let next_len = self.line_len(row + 1);
        self.cursor = next_start + hint.min(next_len);
    }

    /// Move to the start of the previous word (Alt/Ctrl+←).
    pub fn move_word_left(&mut self) {
        let chars: Vec<char> = self.text.chars().collect();
        let mut i = self.cursor;
        while i > 0 && chars[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !chars[i - 1].is_whitespace() {
            i -= 1;
        }
        self.cursor = i;
        self.col_hint = None;
    }

    /// Move to the end of the next word (Alt/Ctrl+→).
    pub fn move_word_right(&mut self) {
        let chars: Vec<char> = self.text.chars().collect();
        let n = chars.len();
        let mut i = self.cursor.min(n);
        while i < n && chars[i].is_whitespace() {
            i += 1;
        }
        while i < n && !chars[i].is_whitespace() {
            i += 1;
        }
        self.cursor = i;
        self.col_hint = None;
    }

    /// Delete from the line start to the cursor (Ctrl+U).
    pub fn kill_to_line_start(&mut self) {
        let (_, col) = self.line_col();
        if col == 0 {
            return;
        }
        self.snapshot();
        let start = self.byte_at(self.cursor - col);
        let end = self.byte_at(self.cursor);
        self.text.replace_range(start..end, "");
        self.cursor -= col;
        self.col_hint = None;
    }

    /// Delete from the cursor to the line end (Ctrl+K).
    pub fn kill_to_line_end(&mut self) {
        let (row, col) = self.line_col();
        let len = self.line_len(row);
        if col >= len {
            return;
        }
        self.snapshot();
        let start = self.byte_at(self.cursor);
        let end = self.byte_at(self.cursor + (len - col));
        self.text.replace_range(start..end, "");
        self.col_hint = None;
    }

    /// Delete the word before the cursor (Ctrl+W).
    pub fn delete_word_before(&mut self) {
        let chars: Vec<char> = self.text.chars().collect();
        let mut i = self.cursor;
        while i > 0 && chars[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !chars[i - 1].is_whitespace() {
            i -= 1;
        }
        if i == self.cursor {
            return;
        }
        self.snapshot();
        let start = self.byte_at(i);
        let end = self.byte_at(self.cursor);
        self.text.replace_range(start..end, "");
        self.cursor = i;
        self.col_hint = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an editor with the cursor placed at the start of the `n`-th char.
    fn at(text: &str, cursor: usize) -> Editor {
        let mut e = Editor::new();
        e.text = text.to_string();
        e.cursor = cursor;
        e
    }

    #[test]
    fn insert_and_move_cursor() {
        let mut e = Editor::new();
        for c in "ab".chars() {
            e.insert_char(c);
        }
        assert_eq!(e.text, "ab");
        assert_eq!(e.cursor, 2);
        e.move_left();
        assert_eq!(e.cursor, 1, "← moves one char left");
        e.insert_char('X');
        assert_eq!(e.text, "aXb", "insert lands at the cursor, not the end");
        assert_eq!(e.cursor, 2, "cursor advances past the inserted char");
        // Clamps: can't move left of 0 or right of the end.
        for _ in 0..10 {
            e.move_left();
        }
        assert_eq!(e.cursor, 0);
        for _ in 0..10 {
            e.move_right();
        }
        assert_eq!(e.cursor, e.char_len());
    }

    #[test]
    fn backspace_deletes_before_cursor() {
        let mut e = at("abc", 2);
        e.backspace();
        assert_eq!(e.text, "ac", "removes the char before the cursor");
        assert_eq!(e.cursor, 1);
        // No-op at the start.
        let mut e = at("abc", 0);
        e.backspace();
        assert_eq!(e.text, "abc");
        assert_eq!(e.cursor, 0);
    }

    #[test]
    fn delete_removes_char_at_cursor() {
        let mut e = at("abc", 1);
        e.delete();
        assert_eq!(e.text, "ac");
        assert_eq!(e.cursor, 1, "cursor stays put on Delete");
        // No-op at the end.
        let mut e = at("ab", 2);
        e.delete();
        assert_eq!(e.text, "ab");
    }

    #[test]
    fn home_end_move_within_line() {
        // Multi-line: Home/End operate on the *current* line only.
        let mut e = at("abc\ndefg", 6); // in "defg", after 'd'
        e.move_home();
        assert_eq!(e.cursor, 4, "Home → start of the current line");
        e.move_end();
        assert_eq!(e.cursor, 8, "End → end of the current line");
    }

    #[test]
    fn ctrl_u_kills_to_line_start() {
        let mut e = at("hello world", 6); // after "hello "
        e.kill_to_line_start();
        assert_eq!(e.text, "world");
        assert_eq!(e.cursor, 0);
        // Only the current line is affected.
        let mut e = at("ab\ncd", 5); // end of "cd"
        e.kill_to_line_start();
        assert_eq!(e.text, "ab\n");
        assert_eq!(e.cursor, 3);
    }

    #[test]
    fn ctrl_k_kills_to_line_end() {
        let mut e = at("hello world", 5);
        e.kill_to_line_end();
        assert_eq!(e.text, "hello");
        assert_eq!(e.cursor, 5);
    }

    #[test]
    fn ctrl_w_deletes_word() {
        let mut e = at("foo bar baz", 11);
        e.delete_word_before();
        assert_eq!(e.text, "foo bar ");
        assert_eq!(e.cursor, 8);
        // A trailing run of spaces is consumed with the word.
        let mut e = at("foo bar   ", 10);
        e.delete_word_before();
        assert_eq!(e.text, "foo ");
    }

    #[test]
    fn word_movement_steps_by_words() {
        let mut e = at("foo bar baz", 11);
        e.move_word_left();
        assert_eq!(e.cursor, 8, "← word → start of `baz`");
        e.move_word_left();
        assert_eq!(e.cursor, 4, "← word → start of `bar`");
        e.move_word_right();
        assert_eq!(e.cursor, 7, "→ word → end of `bar`");
    }

    #[test]
    fn cjk_edits_stay_on_char_boundaries() {
        // The whole point of a char-index cursor: inserting mid-CJK must not
        // panic on a byte offset.
        let mut e = at("你好世界", 2); // between 好 and 世
        e.insert_char('、');
        assert_eq!(e.text, "你好、世界");
        assert_eq!(e.cursor, 3);
        e.backspace();
        assert_eq!(e.text, "你好世界");
    }

    #[test]
    fn up_down_keep_the_column_preference() {
        // Longer→shorter→longer: the column is clamped on the short line, then
        // restored on the way back to a wide one.
        let mut e = at("abcdef\nxy\nghijkl", 5); // line 0, col 5
        e.move_down(); // line 1 is only 2 wide → clamp to col 2
        assert_eq!(e.line_col(), (1, 2));
        e.move_down(); // line 2 → back to the preferred col 5
        assert_eq!(e.line_col(), (2, 5));
    }

    #[test]
    fn undo_restores_previous_text() {
        let mut e = Editor::new();
        for c in "abc".chars() {
            e.insert_char(c);
        }
        e.insert_char('d');
        assert_eq!(e.text, "abcd");
        assert!(e.undo(), "an edit was undone");
        assert_eq!(e.text, "abc");
        assert_eq!(e.cursor, 3);
        // Undo past the beginning is a no-op.
        let mut e = Editor::new();
        assert!(!e.undo());
    }
}

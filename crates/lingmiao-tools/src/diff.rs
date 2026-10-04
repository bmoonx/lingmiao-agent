//! Display-only **unified diff** for file mutations (CC `structuredPatch`).
//!
//! cli 2026-09-28 (「参照 CC 实现代码改动时的红绿对比格式显示样式，包括写入的时候
//! 也是」): a `edit` / `write_file` call must show *what changed*, line by line,
//! with the added lines green and the removed lines red — the same shape CC's
//! `Edit` / `Write` tool cards use. CC builds a `structuredPatch` (`{oldStart,
//! oldLines, newStart, newLines, lines:[{type:"+"/"-"/" ", content}]}`) with its
//! own `NAe()` helper and renders it in the transcript; the model-facing tool
//! result stays a short sentence (`The file X has been updated successfully.`).
//!
//! This module is that authoring half: a **pure** `old → new` line diff with an
//! `@@` hunk header and 3 lines of context, capped so a megabyte file cannot
//! flood the event stream or the frame. It is display-only — the tool's textual
//! result (which the model reads) is untouched.
//!
//! Design notes:
//!
//! * **Prefix/suffix trim + LCS on the remainder.** A real DP over the whole file
//!   is O(n·m) — a 5 000-line file would cost 25 M cells per edit. Trimming the
//!   shared head/tail first (every edit keeps most of the file) leaves a small
//!   middle, which a DP handles exactly; if even that is too big (a rewrite), the
//!   middle degrades to `-all +all` — still a valid diff, just not minimal.
//! * **Counts are the *full* change**, not the truncated display: the card header
//!   shows `+N -M` honestly even when the body folds or truncates.
//! * **One hunk.** CC's renderer walks `structuredPatch` hunks; our transcript
//!   card is a single block, so the lines are flattened into one hunk (the fold +
//!   `ctrl+o` already handles length, §12.10).

use serde_json::json;

/// Context lines kept before / after the change (git's default).
pub const CONTEXT: usize = 3;

/// Hard cap on the diff lines carried for display. A pathological result (a
/// minified bundle rewritten) must not blow up the event or the frame; the
/// remainder is replaced by a single note line.
pub const MAX_DIFF_LINES: usize = 200;

/// Per-line character cap — the same "bound the column width" rule the ripgrep
/// formatter uses, so one minified line cannot dominate the card.
pub const MAX_LINE_CHARS: usize = 240;

/// Cells the DP is allowed to allocate for the *middle* (after trimming). Above
/// this the middle degrades to remove-all/add-all.
const LCS_CELL_BUDGET: usize = 250_000;

/// What a diff line is (`+` / `-` / context / `@@` hunk header).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffKind {
    /// A line present only in the new text.
    Add,
    /// A line present only in the old text.
    Remove,
    /// An unchanged line shown for context.
    Context,
    /// The `@@ -a,b +c,d @@` hunk header.
    Hunk,
}

impl DiffKind {
    /// The one-column marker CC/git print in front of the line text.
    pub fn marker(self) -> &'static str {
        match self {
            DiffKind::Add => "+",
            DiffKind::Remove => "-",
            // A context line keeps a leading space (git's convention); the hunk
            // header carries its own `@@`, so it gets no extra marker.
            DiffKind::Context => " ",
            DiffKind::Hunk => "",
        }
    }

    /// The stable wire name used in the event payload.
    pub fn as_str(self) -> &'static str {
        match self {
            DiffKind::Add => "add",
            DiffKind::Remove => "remove",
            DiffKind::Context => "context",
            DiffKind::Hunk => "hunk",
        }
    }
}

/// One display line of a [`FileDiff`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    /// Which bucket the line belongs to.
    pub kind: DiffKind,
    /// The line text (no marker, no newline).
    pub text: String,
}

/// A display-ready diff of one file mutation (CC `structuredPatch` + counts).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileDiff {
    /// The rendered lines, in order (hunk header first when non-empty).
    pub lines: Vec<DiffLine>,
    /// **Full** count of added lines (before any display truncation).
    pub added: usize,
    /// **Full** count of removed lines (before any display truncation).
    pub removed: usize,
}

impl FileDiff {
    /// Whether nothing changed (an identical rewrite / no-op edit).
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// The `+N -M` stat CC's diff header shows.
    pub fn stat(&self) -> String {
        match (self.added, self.removed) {
            (0, 0) => String::new(),
            (a, 0) => format!("+{a}"),
            (0, r) => format!("-{r}"),
            (a, r) => format!("+{a} -{r}"),
        }
    }

    /// The event-payload shape: `[{ "kind": "add", "text": "…" }, …]`.
    ///
    /// Deliberately a flat `Vec<Value>` — `Event` is loosely typed everywhere
    /// (`args` / `tokens` / `sections` are all `Value`), and the wire dict must
    /// stay JSON-serialisable for the golden-file parity (Q8).
    pub fn to_json_lines(&self) -> Vec<serde_json::Value> {
        self.lines
            .iter()
            .map(|l| json!({ "kind": l.kind.as_str(), "text": l.text }))
            .collect()
    }
}

/// Split text into diff lines, ignoring a single trailing newline.
///
/// `"a\nb\n"` and `"a\nb"` are the same file for diff purposes (every editor
/// writes the final newline); `""` is zero lines, so creating a file diffs as
/// pure additions rather than one empty line.
fn split_lines(s: &str) -> Vec<&str> {
    let s = s.strip_suffix('\n').unwrap_or(s);
    if s.is_empty() {
        Vec::new()
    } else {
        s.split('\n').collect()
    }
}

/// Truncate a line to [`MAX_LINE_CHARS`], appending `…` when cut.
fn cap_line(line: &str) -> String {
    if line.chars().count() <= MAX_LINE_CHARS {
        return line.to_string();
    }
    let mut out: String = line.chars().take(MAX_LINE_CHARS).collect();
    out.push('…');
    out
}

/// Build a unified diff of `old` → `new`.
pub fn unified_diff(old: &str, new: &str) -> FileDiff {
    let a = split_lines(old);
    let b = split_lines(new);

    // Shared head / tail: every edit keeps most of the file, so this leaves a
    // small middle for the exact DP.
    let mut prefix = 0usize;
    while prefix < a.len() && prefix < b.len() && a[prefix] == b[prefix] {
        prefix += 1;
    }
    let mut suffix = 0usize;
    while suffix < a.len() - prefix
        && suffix < b.len() - prefix
        && a[a.len() - 1 - suffix] == b[b.len() - 1 - suffix]
    {
        suffix += 1;
    }

    let mid_a = &a[prefix..a.len() - suffix];
    let mid_b = &b[prefix..b.len() - suffix];
    let middle = if mid_a.is_empty() {
        mid_b.iter().map(|l| (DiffKind::Add, *l)).collect()
    } else if mid_b.is_empty() {
        mid_a.iter().map(|l| (DiffKind::Remove, *l)).collect()
    } else if mid_a.len().saturating_mul(mid_b.len()) <= LCS_CELL_BUDGET {
        lcs_ops(mid_a, mid_b)
    } else {
        // Too large to diff exactly: a valid (if coarse) replacement.
        mid_a
            .iter()
            .map(|l| (DiffKind::Remove, *l))
            .chain(mid_b.iter().map(|l| (DiffKind::Add, *l)))
            .collect()
    };

    let added = middle.iter().filter(|(k, _)| *k == DiffKind::Add).count();
    let removed = middle
        .iter()
        .filter(|(k, _)| *k == DiffKind::Remove)
        .count();

    if added == 0 && removed == 0 {
        return FileDiff::default();
    }

    // Context: up to `CONTEXT` lines from the trimmed head / tail.
    let before = &a[prefix.saturating_sub(CONTEXT)..prefix];
    let after = &a[a.len() - suffix..(a.len() - suffix + suffix.min(CONTEXT))];

    let mut lines: Vec<DiffLine> =
        Vec::with_capacity(middle.len() + before.len() + after.len() + 1);
    let old_start = prefix.saturating_sub(CONTEXT) + 1;
    let old_count = before.len() + removed + after.len();
    let new_start = old_start;
    let new_count = before.len() + added + after.len();
    // git prints start `0` for an **empty** side (a pure create / delete):
    // `@@ -0,0 +1,3 @@` for a new file, `@@ -1,2 +0,0 @@` for a deletion.
    let (old_shown, new_shown) = (
        if old_count == 0 { 0 } else { old_start },
        if new_count == 0 { 0 } else { new_start },
    );
    lines.push(DiffLine {
        kind: DiffKind::Hunk,
        text: format!("@@ -{old_shown},{old_count} +{new_shown},{new_count} @@"),
    });
    lines.extend(before.iter().map(|l| DiffLine {
        kind: DiffKind::Context,
        text: cap_line(l),
    }));
    lines.extend(middle.iter().map(|(kind, l)| DiffLine {
        kind: *kind,
        text: cap_line(l),
    }));
    lines.extend(after.iter().map(|l| DiffLine {
        kind: DiffKind::Context,
        text: cap_line(l),
    }));

    // Display cap: keep the head, report the rest. Counts above stay full.
    if lines.len() > MAX_DIFF_LINES {
        let hidden = lines.len() - (MAX_DIFF_LINES - 1);
        lines.truncate(MAX_DIFF_LINES - 1);
        lines.push(DiffLine {
            kind: DiffKind::Context,
            text: format!("… （diff 过长：以下还有 {hidden} 行未显示）"),
        });
    }

    FileDiff {
        lines,
        added,
        removed,
    }
}

/// Line-level LCS of the (already small) middle → ordered diff ops.
///
/// Classic DP table; the middle is bounded by [`LCS_CELL_BUDGET`], so this stays
/// cheap. Ties prefer removal first, which reads as a replacement (git's shape).
fn lcs_ops<'a>(a: &[&'a str], b: &[&'a str]) -> Vec<(DiffKind, &'a str)> {
    let n = a.len();
    let m = b.len();
    let mut dp = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] = if a[i] == b[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }
    let mut ops: Vec<(DiffKind, &'a str)> = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);
    while i < n && j < m {
        if a[i] == b[j] {
            ops.push((DiffKind::Context, a[i]));
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            ops.push((DiffKind::Remove, a[i]));
            i += 1;
        } else {
            ops.push((DiffKind::Add, b[j]));
            j += 1;
        }
    }
    ops.extend(a[i..].iter().map(|l| (DiffKind::Remove, *l)));
    ops.extend(b[j..].iter().map(|l| (DiffKind::Add, *l)));
    ops
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(d: &FileDiff) -> Vec<String> {
        d.lines
            .iter()
            .filter(|l| l.kind != DiffKind::Hunk)
            .map(|l| format!("{}{}", l.kind.marker(), l.text))
            .collect()
    }

    #[test]
    fn identical_text_has_no_diff() {
        let d = unified_diff("a\nb\n", "a\nb\n");
        assert!(d.is_empty());
        assert_eq!((d.added, d.removed), (0, 0));
        assert_eq!(d.stat(), "");
    }

    #[test]
    fn a_trailing_newline_is_not_a_change() {
        // "a\n" and "a" are the same file (every editor writes the final \n).
        assert!(unified_diff("a\n", "a").is_empty());
    }

    #[test]
    fn one_line_replacement_is_remove_then_add() {
        let d = unified_diff("x = 1\ny = 2\n", "x = 2\ny = 2\n");
        assert_eq!((d.added, d.removed), (1, 1));
        assert_eq!(d.stat(), "+1 -1");
        assert_eq!(d.lines[0].kind, DiffKind::Hunk);
        assert!(d.lines[0].text.starts_with("@@ -1,2 +1,2 @@"));
        assert_eq!(body(&d), vec!["-x = 1", "+x = 2", " y = 2"]);
    }

    #[test]
    fn creating_a_file_is_all_additions_from_zero() {
        let d = unified_diff("", "fn main() {}\n");
        assert_eq!((d.added, d.removed), (1, 0));
        assert_eq!(d.stat(), "+1");
        assert_eq!(d.lines[0].text, "@@ -0,0 +1,1 @@");
        assert_eq!(body(&d), vec!["+fn main() {}"]);
    }

    #[test]
    fn deleting_everything_is_all_removals() {
        let d = unified_diff("a\nb\n", "");
        assert_eq!((d.added, d.removed), (0, 2));
        assert_eq!(d.stat(), "-2");
        assert_eq!(body(&d), vec!["-a", "-b"]);
    }

    #[test]
    fn context_is_trimmed_to_three_lines_either_side() {
        let old = "1\n2\n3\n4\n5\n6\n7\n8\n9\n";
        let new = "1\n2\n3\n4\nX\n6\n7\n8\n9\n";
        let d = unified_diff(old, new);
        assert_eq!(d.stat(), "+1 -1");
        assert_eq!(
            d.lines[0].text, "@@ -2,7 +2,7 @@",
            "3 context lines either side"
        );
        assert_eq!(
            body(&d),
            vec![" 2", " 3", " 4", "-5", "+X", " 6", " 7", " 8"]
        );
    }

    #[test]
    fn two_separate_edits_keep_the_untouched_middle_as_context() {
        let old = "a\nb\nc\nd\ne\nf\ng\n";
        let new = "A\nb\nc\nd\ne\nf\nG\n";
        let d = unified_diff(old, new);
        assert_eq!((d.added, d.removed), (2, 2));
        let kept = d
            .lines
            .iter()
            .filter(|l| l.kind == DiffKind::Context)
            .count();
        assert!(
            kept >= 3,
            "the untouched middle stays as context: {:?}",
            body(&d)
        );
        assert!(body(&d).contains(&"-a".to_string()));
        assert!(body(&d).contains(&"+A".to_string()));
        assert!(body(&d).contains(&"-g".to_string()));
        assert!(body(&d).contains(&"+G".to_string()));
    }

    #[test]
    fn a_huge_rewrite_is_capped_but_counts_stay_full() {
        let old = (0..400)
            .map(|i| format!("old{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let new = (0..400)
            .map(|i| format!("new{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let d = unified_diff(&old, &new);
        assert_eq!(
            (d.added, d.removed),
            (400, 400),
            "counts are the full change"
        );
        assert_eq!(d.lines.len(), MAX_DIFF_LINES, "display is capped");
        assert!(
            d.lines.last().unwrap().text.contains("未显示"),
            "the cap is reported: {:?}",
            d.lines.last()
        );
    }

    #[test]
    fn a_minified_line_is_column_capped() {
        let long = "x".repeat(MAX_LINE_CHARS * 3);
        let d = unified_diff("", &long);
        assert_eq!(d.lines[1].text.chars().count(), MAX_LINE_CHARS + 1);
        assert!(d.lines[1].text.ends_with('…'));
    }

    #[test]
    fn json_lines_carry_kind_and_text() {
        let d = unified_diff("a\n", "b\n");
        let v = d.to_json_lines();
        assert_eq!(v[0]["kind"], "hunk");
        assert_eq!(v[1]["kind"], "remove");
        assert_eq!(v[1]["text"], "a");
        assert_eq!(v[2]["kind"], "add");
        assert_eq!(v[2]["text"], "b");
    }
}

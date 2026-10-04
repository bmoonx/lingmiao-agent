//! Builtin file tools (Q6): read_file / write_file / edit / glob / grep /
//! list_directory / delete_file / copy_file / bash.
//!
//! Every path goes through the [`FileGuard`] sandbox; mutations go through
//! [`GuardedWrite`] so the read-before-write + stale rules hold uniformly.

use std::fs;
use std::io::ErrorKind;
use std::sync::Arc;

use async_trait::async_trait;
use glob::Pattern;
use serde::Deserialize;
use serde_json::Value;

use crate::file_guard::FileGuard;
use crate::ripgrep;
use crate::tool::{Tool, ToolError, ToolOutput, ToolRegistry, json_schema};

/// Directory names never descended into by `glob` / `grep`.
///
/// The VCS metadata dirs (`.git`/`.svn`/`.hg`/… — CC's
/// `VCS_DIRECTORIES_TO_EXCLUDE`) plus the internal state / memory dirs
/// (`.memory`, `.cache`): the latter hold logs,
/// trajectories and the memory DBs, whose JSONL lines can be hundreds of KB
/// each. Scanning them both floods the result and can blow the model's input
/// budget (the 2026-09-27 「无回答」 bug), so they are never searched. Passed to
/// ripgrep as `--glob !<name>` (gitignore-style: a bare name matches at any
/// depth) and used by the in-process fallback.
const SKIP_DIRS: [&str; 14] = [
    ".git",
    ".hg",
    ".svn",
    ".bzr",
    ".jj",
    ".sl",
    ".memory",
    ".cache",
    ".venv",
    "__pycache__",
    "node_modules",
    "target",
    ".playwright-mcp",
    ".fastembed_cache",
];

fn should_skip_dir(name: &str) -> bool {
    SKIP_DIRS.contains(&name)
}

/// ripgrep `--glob !<dir>` args excluding every [`SKIP_DIRS`] entry.
fn exclusion_args() -> Vec<String> {
    SKIP_DIRS
        .iter()
        .flat_map(|d| [String::from("--glob"), format!("!{d}")])
        .collect()
}

/// Per-search ripgrep timeout (seconds). CC uses 20s (60s on WSL); a hung
/// search is reported as a timeout, never silently treated as "no matches".
const RG_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Resolve a search base directory: the tool's `path` (sandboxed) or the root.
fn search_base(guard: &FileGuard, path: &str) -> Result<std::path::PathBuf, ToolError> {
    if path.is_empty() {
        Ok(guard.root().to_path_buf())
    } else {
        guard.resolve(path)
    }
}

/// Relativise the leading path of a `rg` output line (`path:line:content`) to
/// the project root, saving tokens (CC's `toRelativePath`). Lines without a
/// leading colon (e.g. `-A`/`-B` context lines, which use `-` separators) are
/// returned unchanged.
fn relativize_line(line: &str, root: &std::path::Path) -> String {
    match line.find(':') {
        Some(i) => {
            let p = std::path::Path::new(&line[..i]);
            let rel = p.strip_prefix(root).unwrap_or(p);
            format!("{}{}", rel.display(), &line[i..])
        }
        None => line.to_string(),
    }
}

/// Apply `head_limit` / `offset` (CC's `applyHeadLimit`). `head = None` → the
/// default cap; `head = Some(0)` → unlimited. Returns the kept slice length and
/// whether truncation actually occurred (so the caller can advise pagination).
fn apply_limit(total: usize, offset: usize, head: Option<usize>) -> (usize, usize, Option<usize>) {
    match head {
        Some(0) => {
            let start = offset.min(total);
            (start, total, None)
        }
        other => {
            let limit = other.unwrap_or(DEFAULT_HEAD_LIMIT);
            let start = offset.min(total);
            let end = (start + limit).min(total);
            let truncated = total - start > limit;
            (start, end, truncated.then_some(limit))
        }
    }
}

/// Default output cap when `head_limit` is unspecified (CC's `DEFAULT_HEAD_LIMIT`).
const DEFAULT_HEAD_LIMIT: usize = 250;

fn fmt_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    if bytes >= MB {
        format!("{:.1}MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1}KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes}B")
    }
}

// ── argument schemas ───────────────────────────────────────────

#[derive(Deserialize, schemars::JsonSchema)]
struct ReadFileArgs {
    /// Path to the file to read (relative to the project root, or absolute).
    file_path: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct WriteFileArgs {
    /// Path to write (relative to the project root, or absolute).
    file_path: String,
    /// Full content to write. Overwrites the file (read it first).
    content: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct EditArgs {
    /// File to edit. Must have been read first.
    file_path: String,
    /// Exact text to replace. Must occur exactly once.
    old_string: String,
    /// Replacement text.
    new_string: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct GlobArgs {
    /// Glob pattern, e.g. `**/*.rs` or `src/**/*.ts`.
    pattern: String,
    /// Directory to search in (default: project root).
    #[serde(default)]
    path: String,
}

/// `grep` arguments — the CC `GrepTool` schema (ripgrep-backed, full regex).
#[derive(Deserialize, schemars::JsonSchema)]
struct GrepArgs {
    /// Regular expression pattern to search for in file contents.
    pattern: String,
    /// File or directory to search in (default: project root).
    #[serde(default)]
    path: String,
    /// Glob pattern to filter files, e.g. `*.rs` or `*.{ts,tsx}` (rg --glob).
    #[serde(default)]
    glob: String,
    /// Output mode: `content` (matching lines), `files_with_matches` (default,
    /// file paths), or `count` (match counts).
    #[serde(default)]
    output_mode: String,
    /// Lines to show before each match (rg -B). Content mode only.
    #[serde(default, rename = "-B")]
    context_before: Option<usize>,
    /// Lines to show after each match (rg -A). Content mode only.
    #[serde(default, rename = "-A")]
    context_after: Option<usize>,
    /// Lines to show before and after each match (rg -C). Content mode only.
    #[serde(default, rename = "-C")]
    context_c: Option<usize>,
    /// Show line numbers (rg -n). Content mode only. Defaults to true.
    #[serde(default, rename = "-n")]
    show_line_numbers: Option<bool>,
    /// Case-insensitive search (rg -i).
    #[serde(default, rename = "-i")]
    case_insensitive: Option<bool>,
    /// File type to search (rg --type), e.g. `rust`, `js`, `py`.
    #[serde(default, rename = "type")]
    file_type: String,
    /// Limit output to the first N lines/entries (`| head -N`). Defaults to 250;
    /// pass 0 for unlimited (use sparingly).
    #[serde(default)]
    head_limit: Option<usize>,
    /// Skip the first N lines/entries before applying `head_limit`. Defaults to 0.
    #[serde(default)]
    offset: Option<usize>,
    /// Enable multiline mode (rg -U --multiline-dotall). Default: false.
    #[serde(default)]
    multiline: bool,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct ListDirArgs {
    /// Directory to list (default: project root).
    #[serde(default)]
    dir_path: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct DeleteFileArgs {
    /// File to delete. Must have been read first.
    file_path: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct CopyFileArgs {
    /// Source path (file or directory), relative to the project root or absolute.
    src: String,
    /// Destination path. Files are overwritten; directories are merged.
    dst: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct BashArgs {
    /// Shell command to run via `sh -c`, in the project root working directory.
    command: String,
    /// How long this command may go **without producing any output** before the
    /// wait is reconsidered, in seconds (default 30, capped at 300).
    ///
    /// Not a hard kill: while the command keeps writing output it is never
    /// interrupted, however long it runs; once it falls silent for this long the
    /// stage's wait judge decides whether to keep waiting or cut it short.
    #[serde(default)]
    timeout: u64,
}

/// Hard cap on captured `bash` output (bytes), keeping tool results bounded.
const BASH_OUTPUT_CAP: usize = 64 * 1024;
/// Default `bash` timeout (seconds).
const BASH_DEFAULT_TIMEOUT: u64 = 30;
/// Maximum `bash` timeout (seconds).
const BASH_MAX_TIMEOUT: u64 = 300;

/// Maximum hit lines `grep` returns.
const GREP_MAX_HITS: usize = 200;
/// Maximum characters kept from any **one** hit line. A defensive bound: a
/// single JSONL / minified line can be hundreds of KB, and returning it verbatim
/// (×200 hits) is exactly how a search blows the whole input budget.
const GREP_MAX_LINE: usize = 500;
/// Hard cap on the total bytes `grep` returns (the context-safety ceiling).
const GREP_OUTPUT_CAP: usize = 64 * 1024;

/// Truncate `s` to at most `max` characters (char-safe), appending `…` when cut.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    format!("{head}…")
}

// ── tools ──────────────────────────────────────────────────────

/// `read_file` — read a text file, recording it for the write guard.
pub struct ReadFileTool {
    guard: Arc<FileGuard>,
}

/// `write_file` — write a file (read-before-write enforced).
pub struct WriteFileTool {
    guard: Arc<FileGuard>,
}

/// `edit` — exact single-occurrence string replacement.
pub struct EditTool {
    guard: Arc<FileGuard>,
}

/// `glob` — recursive pattern search.
pub struct GlobTool {
    guard: Arc<FileGuard>,
}

/// `grep` — literal content search.
pub struct GrepTool {
    guard: Arc<FileGuard>,
}

/// `list_directory` — single-level listing.
pub struct ListDirectoryTool {
    guard: Arc<FileGuard>,
}

/// `delete_file` — delete a previously-read file.
pub struct DeleteFileTool {
    guard: Arc<FileGuard>,
}

/// `copy_file` — sandboxed file/directory copy.
pub struct CopyFileTool {
    guard: Arc<FileGuard>,
}

/// `bash` — run a shell command with a timeout and output cap.
pub struct BashTool {
    guard: Arc<FileGuard>,
}

fn read_args<T: for<'de> Deserialize<'de>>(tool: &str, arguments: Value) -> Result<T, ToolError> {
    serde_json::from_value(arguments).map_err(|e| ToolError::invalid(tool, e.to_string()))
}

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }
    fn description(&self) -> &str {
        "Read a file as text, or (for png/jpg/jpeg/gif/webp/bmp) as an image marker. Use list_directory to confirm the path exists first."
    }
    fn parameters(&self) -> Value {
        json_schema::<ReadFileArgs>()
    }
    async fn execute(&self, arguments: Value) -> Result<ToolOutput, ToolError> {
        let args: ReadFileArgs = read_args("read_file", arguments)?;
        let path = self.guard.resolve(&args.file_path)?;
        let meta = fs::metadata(&path).map_err(|e| ToolError::io(self.guard.relative(&path), e))?;
        if !meta.is_file() {
            return Err(ToolError::io(
                self.guard.relative(&path),
                std::io::Error::new(ErrorKind::InvalidInput, "not a file"),
            ));
        }
        // ⑥ vision: an image is returned as a marker JSON (path/mime/size/data_url)
        // rather than garbled text. The engine's stage agent turns the marker into
        // a multipart image attachment when the model supports vision. The size
        // cap (≤10 MiB) is enforced inside `to_data_url`.
        if lingmiao_core::image::is_image_path(&path) {
            let data_url = lingmiao_core::image::to_data_url(&path)
                .map_err(|e| ToolError::invalid("read_file", e.message().to_string()))?;
            let marker = lingmiao_core::image::build_marker(
                &self.guard.relative(&path),
                lingmiao_core::image::mime_for(&path),
                meta.len(),
                &data_url,
            );
            return Ok(ToolOutput::ok(marker.to_string()));
        }
        let content =
            fs::read_to_string(&path).map_err(|e| ToolError::io(self.guard.relative(&path), e))?;
        self.guard.record_read(&path, &content);
        Ok(ToolOutput::ok(content))
    }
}

#[async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> &str {
        "write_file"
    }
    fn description(&self) -> &str {
        "Write content to a file. Existing files must be read first (enforced)."
    }
    fn parameters(&self) -> Value {
        json_schema::<WriteFileArgs>()
    }
    fn is_mutating(&self) -> bool {
        true
    }
    async fn execute(&self, arguments: Value) -> Result<ToolOutput, ToolError> {
        let args: WriteFileArgs = read_args("write_file", arguments)?;
        // The pre-write content (read-before-write is enforced *inside*
        // `GuardedWrite`, so an existing file is guaranteed to have been read).
        // Captured here purely for the **display** diff — the model still gets
        // the short `Wrote N bytes to …` sentence CC returns.
        let old = self
            .guard
            .resolve(&args.file_path)
            .ok()
            .and_then(|p| fs::read_to_string(&p).ok())
            .unwrap_or_default();
        let path = self.guard.write().write(&args.file_path, &args.content)?;
        let diff = crate::diff::unified_diff(&old, &args.content);
        Ok(ToolOutput::ok(format!(
            "Wrote {} bytes to {}",
            args.content.len(),
            self.guard.relative(&path)
        ))
        .with_diff(diff.lines))
    }
}

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }
    fn description(&self) -> &str {
        "Exact string replacement. `old_string` must match exactly once; the file must be read first."
    }
    fn parameters(&self) -> Value {
        json_schema::<EditArgs>()
    }
    fn is_mutating(&self) -> bool {
        true
    }
    async fn execute(&self, arguments: Value) -> Result<ToolOutput, ToolError> {
        let args: EditArgs = read_args("edit", arguments)?;
        let path = self.guard.resolve(&args.file_path)?;
        self.guard.require_read(&path)?;
        let current =
            fs::read_to_string(&path).map_err(|e| ToolError::io(self.guard.relative(&path), e))?;
        self.guard.check_fresh(&path, &current)?;
        let hits = current.matches(&args.old_string).count();
        if hits != 1 {
            return Err(ToolError::invalid(
                "edit",
                format!("`old_string` must match exactly once, found {hits} occurrence(s)"),
            ));
        }
        let updated = current.replacen(&args.old_string, &args.new_string, 1);
        self.guard.write().write(&args.file_path, &updated)?;
        // CC's `Edit` returns the short sentence *and* a `structuredPatch`; the
        // card renders the patch (red/green), the model reads the sentence.
        // `current` → `updated` is the exact line diff of this one replacement.
        let diff = crate::diff::unified_diff(&current, &updated);
        Ok(ToolOutput::ok(format!(
            "Edited {} (1 replacement)",
            self.guard.relative(&path)
        ))
        .with_diff(diff.lines))
    }
}

fn collect_files(
    root: &std::path::Path,
    pattern: Option<&Pattern>,
    out: &mut Vec<std::path::PathBuf>,
) {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if should_skip_dir(&name) {
                    continue;
                }
                stack.push(path);
            } else if file_type.is_file() {
                let matches = match pattern {
                    None => true,
                    Some(p) => {
                        let rel = path.strip_prefix(root).unwrap_or(&path);
                        p.matches_path(rel) || p.matches(&entry.file_name().to_string_lossy())
                    }
                };
                if matches {
                    out.push(path);
                }
            }
        }
    }
}

/// Maximum files `glob` returns (CC's GlobTool limit).
const GLOB_MAX_RESULTS: usize = 100;

/// Format a `glob` result from ripgrep `--files` output: relativise to the root,
/// cap at [`GLOB_MAX_RESULTS`], and note truncation.
fn format_glob(lines: &[String], base: &std::path::Path, root: &std::path::Path) -> String {
    if lines.is_empty() {
        return "No files matched.".to_string();
    }
    let shown: Vec<String> = lines
        .iter()
        .take(GLOB_MAX_RESULTS)
        .map(|p| {
            let p = std::path::Path::new(p);
            let abs = if p.is_absolute() {
                p.to_path_buf()
            } else {
                base.join(p)
            };
            abs.strip_prefix(root).unwrap_or(&abs).display().to_string()
        })
        .collect();
    let mut out = shown.join("\n");
    if lines.len() > GLOB_MAX_RESULTS {
        out.push_str(
            "\n(Results are truncated to 100 files. Consider a more specific path or pattern.)",
        );
    }
    out
}

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &str {
        "glob"
    }
    fn description(&self) -> &str {
        "Fast file pattern matching (ripgrep-backed). Supports glob patterns like `**/*.rs` or `src/**/*.ts`; returns matching paths sorted by modification time, capped at 100. Skips VCS and internal state dirs."
    }
    fn parameters(&self) -> Value {
        json_schema::<GlobArgs>()
    }
    async fn execute(&self, arguments: Value) -> Result<ToolOutput, ToolError> {
        let args: GlobArgs = read_args("glob", arguments)?;
        let base = search_base(&self.guard, &args.path)?;
        // CC's `glob()` runs ripgrep: `--files --glob <pattern> --sort=modified`.
        // We keep `--hidden` (dotfiles findable) but respect `.gitignore`
        // (CC's `--no-ignore` would resurface `target/` / `.memory/`), and add
        // explicit exclusions for the internal state dirs.
        let mut rg = vec![
            "--files".to_string(),
            "--glob".to_string(),
            args.pattern.clone(),
            "--sort=modified".to_string(),
            "--hidden".to_string(),
        ];
        rg.extend(exclusion_args());
        match ripgrep::run(&rg, &base, RG_TIMEOUT).await {
            Ok(run) if !run.timed_out => Ok(ToolOutput::ok(format_glob(
                &run.lines,
                &base,
                self.guard.root(),
            ))),
            Ok(_) => Ok(ToolOutput::ok(format!(
                "…[glob 超时（{}s）]",
                RG_TIMEOUT.as_secs()
            ))),
            Err(e) => {
                tracing::debug!("glob: ripgrep unavailable ({e}); using in-process fallback");
                self.glob_fallback(&base, &args.pattern)
            }
        }
    }
}

impl GlobTool {
    /// In-process fallback used only when ripgrep is unavailable.
    fn glob_fallback(
        &self,
        base: &std::path::Path,
        pattern: &str,
    ) -> Result<ToolOutput, ToolError> {
        let pat = Pattern::new(pattern)
            .map_err(|e| ToolError::invalid("glob", format!("bad pattern: {e}")))?;
        let mut files = Vec::new();
        collect_files(base, Some(&pat), &mut files);
        files.sort();
        if files.is_empty() {
            return Ok(ToolOutput::ok("No files matched."));
        }
        let lines: Vec<String> = files
            .iter()
            .take(GLOB_MAX_RESULTS)
            .map(|p| {
                p.strip_prefix(self.guard.root())
                    .unwrap_or(p)
                    .display()
                    .to_string()
            })
            .collect();
        let mut out = lines.join("\n");
        if files.len() > GLOB_MAX_RESULTS {
            out.push_str(
                "\n(Results are truncated to 100 files. Consider a more specific path or pattern.)",
            );
        }
        Ok(ToolOutput::ok(out))
    }
}

/// Normalise the `output_mode` argument to one of the three known modes
/// (default `files_with_matches`, CC's default).
fn normalize_mode(raw: &str) -> &'static str {
    match raw {
        "content" => "content",
        "count" => "count",
        _ => "files_with_matches",
    }
}

/// Split a `glob` filter argument into individual patterns. Mirrors CC: split
/// on whitespace, but keep `{a,b}` brace groups intact; split the rest on commas.
fn split_glob_list(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    for tok in raw.split_whitespace() {
        if tok.contains('{') && tok.contains('}') {
            out.push(tok.to_string());
        } else {
            out.extend(tok.split(',').filter(|s| !s.is_empty()).map(str::to_string));
        }
    }
    out
}

/// Build the ripgrep argument vector for a `grep` call (CC's `GrepTool.call`).
fn build_grep_args(args: &GrepArgs, mode: &str) -> Vec<String> {
    let mut rg = vec!["--hidden".to_string()];
    rg.extend(exclusion_args());
    // Bound column width so a minified / base64 line cannot flood the result.
    rg.push("--max-columns".to_string());
    rg.push("500".to_string());
    if args.multiline {
        rg.push("-U".to_string());
        rg.push("--multiline-dotall".to_string());
    }
    if args.case_insensitive.unwrap_or(false) {
        rg.push("-i".to_string());
    }
    match mode {
        "content" => {
            if args.show_line_numbers.unwrap_or(true) {
                rg.push("-n".to_string());
            }
            if let Some(c) = args.context_c {
                rg.push("-C".to_string());
                rg.push(c.to_string());
            } else {
                if let Some(b) = args.context_before {
                    rg.push("-B".to_string());
                    rg.push(b.to_string());
                }
                if let Some(a) = args.context_after {
                    rg.push("-A".to_string());
                    rg.push(a.to_string());
                }
            }
        }
        "count" => rg.push("-c".to_string()),
        _ => rg.push("-l".to_string()),
    }
    // A pattern starting with `-` must be passed via `-e` so rg does not read
    // it as a flag (CC does the same).
    if args.pattern.starts_with('-') {
        rg.push("-e".to_string());
        rg.push(args.pattern.clone());
    } else {
        rg.push(args.pattern.clone());
    }
    if !args.file_type.is_empty() {
        rg.push("--type".to_string());
        rg.push(args.file_type.clone());
    }
    for g in split_glob_list(&args.glob) {
        rg.push("--glob".to_string());
        rg.push(g);
    }
    rg
}

/// Format a `grep` result from ripgrep stdout for the requested `mode`.
fn format_grep(
    lines: &[String],
    mode: &str,
    base: &std::path::Path,
    root: &std::path::Path,
    args: &GrepArgs,
) -> String {
    if lines.is_empty() {
        return "No matches found.".to_string();
    }
    let offset = args.offset.unwrap_or(0);
    match mode {
        "content" => {
            let rel: Vec<String> = lines.iter().map(|l| relativize_line(l, root)).collect();
            let (start, end, truncated) = apply_limit(rel.len(), offset, args.head_limit);
            let mut out = rel[start..end].join("\n");
            if let Some(limit) = truncated {
                out.push_str(&format!(
                    "\n\n[Showing results with pagination = limit: {limit}, offset: {offset}]"
                ));
            }
            out
        }
        "count" => {
            let rel: Vec<String> = lines.iter().map(|l| relativize_line(l, root)).collect();
            let (start, end, truncated) = apply_limit(rel.len(), offset, args.head_limit);
            let shown = &rel[start..end];
            let mut matches = 0usize;
            let mut files = 0usize;
            for l in shown {
                if let Some(i) = l.rfind(':') {
                    if let Ok(n) = l[i + 1..].trim().parse::<usize>() {
                        matches += n;
                        files += 1;
                    }
                }
            }
            let mut out = shown.join("\n");
            out.push_str(&format!(
                "\n\nFound {matches} total occurrences across {files} files."
            ));
            if let Some(limit) = truncated {
                out.push_str(&format!(" (limit: {limit}, offset: {offset})"));
            }
            out
        }
        _ => {
            // files_with_matches: sort by modification time (newest first), then
            // relativise + apply the head limit (CC's behaviour).
            let mut files: Vec<(String, std::time::SystemTime)> = lines
                .iter()
                .map(|p| {
                    let p = std::path::Path::new(p);
                    let abs = if p.is_absolute() {
                        p.to_path_buf()
                    } else {
                        base.join(p)
                    };
                    let mtime = fs::metadata(&abs)
                        .and_then(|m| m.modified())
                        .unwrap_or(std::time::UNIX_EPOCH);
                    (
                        abs.strip_prefix(root).unwrap_or(&abs).display().to_string(),
                        mtime,
                    )
                })
                .collect();
            files.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            let (start, end, truncated) = apply_limit(files.len(), offset, args.head_limit);
            let shown: Vec<&str> = files[start..end].iter().map(|(p, _)| p.as_str()).collect();
            let mut out = format!("Found {} files", lines.len());
            if let Some(limit) = truncated {
                out.push_str(&format!(" (limit: {limit}, offset: {offset})"));
            }
            out.push('\n');
            out.push_str(&shown.join("\n"));
            out
        }
    }
}

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }
    fn description(&self) -> &str {
        "A powerful search tool built on ripgrep. Full regex syntax (e.g. `log.*Error`); filter files with the `glob` or `type` parameter; output modes: `content` (matching lines), `files_with_matches` (default), `count`. Skips VCS and internal state dirs."
    }
    fn parameters(&self) -> Value {
        json_schema::<GrepArgs>()
    }
    async fn execute(&self, arguments: Value) -> Result<ToolOutput, ToolError> {
        let args: GrepArgs = read_args("grep", arguments)?;
        if args.pattern.is_empty() {
            return Err(ToolError::invalid("grep", "`pattern` must not be empty"));
        }
        let base = search_base(&self.guard, &args.path)?;
        let mode = normalize_mode(&args.output_mode);
        let rg = build_grep_args(&args, mode);
        match ripgrep::run(&rg, &base, RG_TIMEOUT).await {
            Ok(run) if run.timed_out => Ok(ToolOutput::ok(format!(
                "…[grep 超时（{}s）：搜索未完成，可能仍有匹配。请缩小 path 或加 glob/type 过滤]",
                RG_TIMEOUT.as_secs()
            ))),
            Ok(run) => Ok(ToolOutput::ok(format_grep(
                &run.lines,
                mode,
                &base,
                self.guard.root(),
                &args,
            ))),
            Err(e) => {
                tracing::debug!("grep: ripgrep unavailable ({e}); using in-process fallback");
                self.grep_fallback(&base, &args)
            }
        }
    }
}

impl GrepTool {
    /// In-process fallback used only when ripgrep is unavailable. Literal
    /// substring match (not regex) + per-line / total output bounds.
    fn grep_fallback(
        &self,
        base: &std::path::Path,
        args: &GrepArgs,
    ) -> Result<ToolOutput, ToolError> {
        let filter = if args.glob.is_empty() {
            None
        } else {
            Some(
                Pattern::new(&args.glob)
                    .map_err(|e| ToolError::invalid("grep", format!("bad glob: {e}")))?,
            )
        };
        let mut files = Vec::new();
        collect_files(base, filter.as_ref(), &mut files);
        files.sort();
        let mut hits = Vec::new();
        let mut total = 0usize;
        let mut truncated = false;
        'outer: for file in files {
            let Ok(text) = fs::read_to_string(&file) else {
                continue;
            };
            for (idx, line) in text.lines().enumerate() {
                if line.contains(&args.pattern) {
                    // Per-line bound + running total bound: a single pathological
                    // line, or an accumulation of many, can never balloon the
                    // result past `GREP_OUTPUT_CAP`.
                    let shown = truncate_chars(line.trim_end(), GREP_MAX_LINE);
                    let hit = format!(
                        "{}:{}: {}",
                        file.strip_prefix(self.guard.root())
                            .unwrap_or(&file)
                            .display(),
                        idx + 1,
                        shown
                    );
                    if total + hit.len() + 1 > GREP_OUTPUT_CAP || hits.len() >= GREP_MAX_HITS {
                        truncated = true;
                        break 'outer;
                    }
                    total += hit.len() + 1;
                    hits.push(hit);
                }
            }
        }
        if hits.is_empty() {
            return Ok(ToolOutput::ok("No matches."));
        }
        let mut body = hits.join("\n");
        if truncated {
            body.push_str(&format!(
                "\n…[结果已截断：上限 {GREP_MAX_HITS} 行 / {}KB。缩小 path 或加 glob 过滤]",
                GREP_OUTPUT_CAP / 1024
            ));
        }
        Ok(ToolOutput::ok(body))
    }
}

#[async_trait]
impl Tool for ListDirectoryTool {
    fn name(&self) -> &str {
        "list_directory"
    }
    fn description(&self) -> &str {
        "List files and subdirectories with sizes. Single level only."
    }
    fn parameters(&self) -> Value {
        json_schema::<ListDirArgs>()
    }
    async fn execute(&self, arguments: Value) -> Result<ToolOutput, ToolError> {
        let args: ListDirArgs = read_args("list_directory", arguments)?;
        let base = if args.dir_path.is_empty() {
            self.guard.root().to_path_buf()
        } else {
            self.guard.resolve(&args.dir_path)?
        };
        let entries =
            fs::read_dir(&base).map_err(|e| ToolError::io(self.guard.relative(&base), e))?;
        let mut lines: Vec<String> = Vec::new();
        for entry in entries.flatten() {
            let meta = entry.metadata().ok();
            let is_dir = meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);
            let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
            let name = entry.file_name().to_string_lossy().into_owned();
            if is_dir {
                lines.push(format!("{name}/"));
            } else {
                lines.push(format!("{name} ({})", fmt_size(size)));
            }
        }
        lines.sort();
        if lines.is_empty() {
            return Ok(ToolOutput::ok("(empty directory)"));
        }
        Ok(ToolOutput::ok(lines.join("\n")))
    }
}

#[async_trait]
impl Tool for DeleteFileTool {
    fn name(&self) -> &str {
        "delete_file"
    }
    fn description(&self) -> &str {
        "Delete a file. It must have been read first (enforced)."
    }
    fn parameters(&self) -> Value {
        json_schema::<DeleteFileArgs>()
    }
    fn is_mutating(&self) -> bool {
        true
    }
    async fn execute(&self, arguments: Value) -> Result<ToolOutput, ToolError> {
        let args: DeleteFileArgs = read_args("delete_file", arguments)?;
        let path = self.guard.write().delete(&args.file_path)?;
        Ok(ToolOutput::ok(format!(
            "Deleted {}",
            self.guard.relative(&path)
        )))
    }
}

/// Recursively copy a directory tree (files only; creates parents as needed).
fn copy_dir_recursive(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)?.flatten() {
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else {
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

#[async_trait]
impl Tool for CopyFileTool {
    fn name(&self) -> &str {
        "copy_file"
    }
    fn description(&self) -> &str {
        "Copy a file or directory (recursive). Overwrites the destination file if it exists."
    }
    fn parameters(&self) -> Value {
        json_schema::<CopyFileArgs>()
    }
    fn is_mutating(&self) -> bool {
        true
    }
    async fn execute(&self, arguments: Value) -> Result<ToolOutput, ToolError> {
        let args: CopyFileArgs = read_args("copy_file", arguments)?;
        let src = self.guard.resolve(&args.src)?;
        let dst = self.guard.resolve(&args.dst)?;
        let meta = fs::metadata(&src).map_err(|e| ToolError::io(self.guard.relative(&src), e))?;
        if meta.is_dir() {
            copy_dir_recursive(&src, &dst)
                .map_err(|e| ToolError::io(self.guard.relative(&dst), e))?;
        } else {
            if let Ok(content) = fs::read_to_string(&src) {
                self.guard.record_read(&src, &content);
            }
            if let Some(parent) = dst.parent() {
                fs::create_dir_all(parent)
                    .map_err(|e| ToolError::io(self.guard.relative(parent), e))?;
            }
            fs::copy(&src, &dst).map_err(|e| ToolError::io(self.guard.relative(&dst), e))?;
        }
        Ok(ToolOutput::ok(format!(
            "Copied {} -> {}",
            self.guard.relative(&src),
            self.guard.relative(&dst)
        )))
    }
}

/// First line of a command, trimmed — the human label a wait reports.
///
/// F 项: the poll's `what` field names *what* is being waited on, and a full
/// multi-line shell script would swamp the judge prompt. One line is enough for
/// a person (and the model) to recognise the operation.
fn first_line(command: &str) -> String {
    let line = command.lines().next().unwrap_or("").trim();
    truncate_chars(line, 80)
}

/// Cap a captured byte stream at [`BASH_OUTPUT_CAP`], appending a note when cut.
fn cap_output(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    if text.len() <= BASH_OUTPUT_CAP {
        return text.into_owned();
    }
    let mut cut = text[..BASH_OUTPUT_CAP].to_string();
    cut.push_str("\n…[output truncated]");
    cut
}

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }
    fn description(&self) -> &str {
        "Run a shell command via `sh -c` in the project root. A silent command (no output for 30s by default, max 300s) is handed to the wait judge, which decides whether to keep waiting; a command that keeps printing is never interrupted. Output cap enforced."
    }
    fn parameters(&self) -> Value {
        json_schema::<BashArgs>()
    }
    fn is_mutating(&self) -> bool {
        true
    }
    async fn execute(&self, arguments: Value) -> Result<ToolOutput, ToolError> {
        let args: BashArgs = read_args("bash", arguments)?;
        if args.command.trim().is_empty() {
            return Err(ToolError::invalid("bash", "`command` must not be empty"));
        }
        let secs = match args.timeout {
            0 => BASH_DEFAULT_TIMEOUT,
            n => n.min(BASH_MAX_TIMEOUT),
        };
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c")
            .arg(&args.command)
            .current_dir(self.guard.root())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        crate::wait_exec::isolate_process_group(&mut cmd);
        let out =
            // F 项（cli 2026-10-05 拍板）: 改为统一轮询等待。`secs` 仍是本处的限额
            // —— 范围里没有裁判时，它就是原来那个硬超时，行为一字不变；有裁判时
            // 它变成「安静多久才值得问一句」的门槛，决定权交给裁判，并且命令持续
            // 吐输出时（`Progress::touch`）根本不会被问（长 cargo build 不会误杀）。
            match crate::wait_exec::run_polled(
                lingmiao_core::polling::WaitClass::Command,
                format!("bash: {}", first_line(&args.command)),
                std::time::Duration::from_secs(secs),
                BASH_OUTPUT_CAP,
                &mut cmd,
            )
            .await
            {
                Ok(o) => o,
                Err(e) => return Err(ToolError::io("bash", e)),
            };
        if let Some(aborted) = out.aborted {
            return Ok(ToolOutput::error(format!(
                "{}\n命令行：{}",
                aborted.message(),
                args.command
            )));
        }
        if out.timed_out {
            return Ok(ToolOutput::error(format!(
                "command timed out after {secs}s: {}",
                args.command
            )));
        }
        let mut body = cap_output(&out.stdout);
        let stderr = cap_output(&out.stderr);
        if !stderr.is_empty() {
            if !body.is_empty() {
                body.push('\n');
            }
            body.push_str("[stderr]\n");
            body.push_str(&stderr);
        }
        let status = out.status;
        let code = status.and_then(|s| s.code()).unwrap_or(-1);
        let content = format!("exit={code}\n{body}");
        // A shell command produces no line diff; build via the constructors so
        // the `diff` field is always initialised explicitly.
        Ok(if status.map(|s| s.success()).unwrap_or(false) {
            ToolOutput::ok(content)
        } else {
            ToolOutput::error(content)
        })
    }
}

/// Bundle that registers every file tool against one shared guard.
pub struct FileTools {
    guard: Arc<FileGuard>,
}

impl FileTools {
    /// Bind to a guard.
    pub fn new(guard: Arc<FileGuard>) -> Self {
        Self { guard }
    }

    /// Register all file tools into `registry`.
    pub fn register_all(&self, registry: &mut ToolRegistry) {
        let g = &self.guard;
        registry.register(ReadFileTool { guard: g.clone() });
        registry.register(WriteFileTool { guard: g.clone() });
        registry.register(EditTool { guard: g.clone() });
        registry.register(GlobTool { guard: g.clone() });
        registry.register(GrepTool { guard: g.clone() });
        registry.register(ListDirectoryTool { guard: g.clone() });
        registry.register(DeleteFileTool { guard: g.clone() });
        registry.register(CopyFileTool { guard: g.clone() });
        registry.register(BashTool { guard: g.clone() });
    }
}

/// Convenience: names of tools in this group.
pub const TOOL_NAMES: [&str; 9] = [
    "read_file",
    "write_file",
    "edit",
    "glob",
    "grep",
    "list_directory",
    "delete_file",
    "copy_file",
    "bash",
];

/// Small helper used by tests and the registry builder.
pub fn register(registry: &mut ToolRegistry, guard: Arc<FileGuard>) {
    FileTools::new(guard).register_all(registry);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_root(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("lingmiao-ft-{tag}-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::canonicalize(&dir).unwrap()
    }

    fn registry(root: &std::path::Path) -> ToolRegistry {
        let mut reg = ToolRegistry::new();
        register(&mut reg, Arc::new(FileGuard::new(root)));
        reg
    }

    #[tokio::test]
    async fn write_then_read_roundtrip() {
        let root = temp_root("rw");
        let reg = registry(&root);
        reg.execute(
            "write_file",
            json!({"file_path": "n.txt", "content": "hello"}),
        )
        .await
        .unwrap();
        let out = reg
            .execute("read_file", json!({"file_path": "n.txt"}))
            .await
            .unwrap();
        assert_eq!(out.content, "hello");
        fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn overwrite_requires_prior_read() {
        let root = temp_root("ovr");
        fs::write(root.join("a.txt"), "v1").unwrap();
        let reg = registry(&root);
        let err = reg
            .execute("write_file", json!({"file_path": "a.txt", "content": "v2"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::ReadRequired(_)));
        // read then write
        reg.execute("read_file", json!({"file_path": "a.txt"}))
            .await
            .unwrap();
        reg.execute("write_file", json!({"file_path": "a.txt", "content": "v2"}))
            .await
            .unwrap();
        assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "v2");
        fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn edit_requires_single_match() {
        let root = temp_root("edit");
        fs::write(root.join("a.txt"), "one one").unwrap();
        let reg = registry(&root);
        reg.execute("read_file", json!({"file_path": "a.txt"}))
            .await
            .unwrap();
        let err = reg
            .execute(
                "edit",
                json!({"file_path": "a.txt", "old_string": "one", "new_string": "two"}),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
        reg.execute(
            "edit",
            json!({"file_path": "a.txt", "old_string": "one one", "new_string": "two"}),
        )
        .await
        .unwrap();
        assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "two");
        fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn edit_and_write_return_a_display_diff() {
        // cli 2026-09-28 (「参照 CC 实现代码改动时的红绿对比格式显示样式，包括写入
        // 的时候也是」): a mutation carries a **display-only** unified diff (CC's
        // `structuredPatch`) while the model-facing content stays the short
        // sentence — the same split CC uses.
        let root = temp_root("diff");
        fs::write(root.join("e.rs"), "let x = 1;\nkeep\n").unwrap();
        let reg = registry(&root);
        reg.execute("read_file", json!({"file_path": "e.rs"}))
            .await
            .unwrap();
        let out = reg
            .execute(
                "edit",
                json!({"file_path": "e.rs", "old_string": "let x = 1;", "new_string": "let x = 2;"}),
            )
            .await
            .unwrap();
        assert!(out.content.starts_with("Edited e.rs"), "{}", out.content);
        assert!(
            !out.content.contains("@@") && !out.content.contains("+let x"),
            "the model still gets the sentence, not the patch: {}",
            out.content
        );
        let kinds: Vec<crate::diff::DiffKind> = out.diff.iter().map(|l| l.kind).collect();
        assert!(kinds.contains(&crate::diff::DiffKind::Add));
        assert!(kinds.contains(&crate::diff::DiffKind::Remove));
        assert!(kinds.contains(&crate::diff::DiffKind::Context));
        let added: Vec<&str> = out
            .diff
            .iter()
            .filter(|l| l.kind == crate::diff::DiffKind::Add)
            .map(|l| l.text.as_str())
            .collect();
        assert_eq!(added, vec!["let x = 2;"], "the added line is the new text");

        // `write_file` on a brand-new file diffs as pure additions.
        let w = reg
            .execute(
                "write_file",
                json!({"file_path": "new.rs", "content": "fn main() {}\n"}),
            )
            .await
            .unwrap();
        assert!(w.content.starts_with("Wrote "), "{}", w.content);
        assert_eq!(w.diff.len(), 2, "hunk + one add: {:?}", w.diff);
        assert_eq!(w.diff[0].kind, crate::diff::DiffKind::Hunk);
        assert_eq!(w.diff[1].kind, crate::diff::DiffKind::Add);
        assert_eq!(w.diff[1].text, "fn main() {}");

        // An identical rewrite changes nothing → an empty diff (the card falls
        // back to its plain shape).
        reg.execute("read_file", json!({"file_path": "new.rs"}))
            .await
            .unwrap();
        let same = reg
            .execute(
                "write_file",
                json!({"file_path": "new.rs", "content": "fn main() {}\n"}),
            )
            .await
            .unwrap();
        assert!(same.diff.is_empty(), "no change → no diff: {:?}", same.diff);

        // A non-mutating tool never carries one.
        let r = reg
            .execute("read_file", json!({"file_path": "e.rs"}))
            .await
            .unwrap();
        assert!(r.diff.is_empty());
        fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn read_file_on_an_image_returns_a_marker() {
        let root = temp_root("img");
        // Extension drives detection (contents need not be a valid image here).
        fs::write(root.join("pic.png"), [0u8, 1, 2, 3]).unwrap();
        let reg = registry(&root);
        let out = reg
            .execute("read_file", json!({"file_path": "pic.png"}))
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&out.content).expect("marker JSON");
        assert_eq!(v["__lingmiao_image__"], json!(true));
        assert_eq!(v["mime"], json!("image/png"));
        assert_eq!(v["size"], json!(4));
        assert_eq!(v["data_url"], json!("data:image/png;base64,AAECAw=="));
        assert!(v["path"].as_str().unwrap().ends_with("pic.png"));
        // A text file still reads as plain text (unchanged behavior).
        fs::write(root.join("t.txt"), "hi").unwrap();
        let t = reg
            .execute("read_file", json!({"file_path": "t.txt"}))
            .await
            .unwrap();
        assert_eq!(t.content, "hi");
        fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn glob_and_grep_find_content() {
        let root = temp_root("glob");
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("sub/x.rs"), "fn main() {}\n").unwrap();
        fs::write(root.join("y.txt"), "fn main() {}\n").unwrap();
        let reg = registry(&root);
        let g = reg
            .execute("glob", json!({"pattern": "**/*.rs"}))
            .await
            .unwrap();
        assert!(g.content.contains("sub/x.rs"));
        assert!(!g.content.contains("y.txt"));
        // Content mode returns `path:line: text`; files_with_matches (default)
        // returns bare paths.
        let gr = reg
            .execute(
                "grep",
                json!({"pattern": "main", "glob": "*.txt", "output_mode": "content"}),
            )
            .await
            .unwrap();
        assert!(gr.content.contains("y.txt:1"), "{}", gr.content);
        let files = reg
            .execute("grep", json!({"pattern": "main", "glob": "*.txt"}))
            .await
            .unwrap();
        assert!(files.content.contains("y.txt"), "{}", files.content);
        assert!(
            !files.content.contains("y.txt:"),
            "bare path: {}",
            files.content
        );
        fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn grep_count_mode_reports_totals() {
        let root = temp_root("grep-count");
        fs::write(root.join("a.txt"), "hit\nhit\nmiss\n").unwrap();
        let reg = registry(&root);
        let out = reg
            .execute("grep", json!({"pattern": "hit", "output_mode": "count"}))
            .await
            .unwrap();
        assert!(out.content.contains("a.txt:2"), "{}", out.content);
        assert!(
            out.content
                .contains("Found 2 total occurrences across 1 files")
        );
        fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn grep_supports_regex_and_glob_filter() {
        // ripgrep means full regex (not just literal substrings).
        let root = temp_root("grep-regex");
        fs::write(root.join("a.rs"), "let x = 42;\n").unwrap();
        fs::write(root.join("b.txt"), "let x = 42;\n").unwrap();
        let reg = registry(&root);
        let out = reg
            .execute(
                "grep",
                json!({"pattern": "let\\s+\\w+\\s*=\\s*\\d+", "glob": "*.rs", "output_mode": "content"}),
            )
            .await
            .unwrap();
        assert!(out.content.contains("a.rs:1"), "{}", out.content);
        assert!(!out.content.contains("b.txt"), "{}", out.content);
        fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn grep_skips_internal_state_dirs() {
        // 2026-09-27「无回答」根因：grep 递归扫进内部状态目录，
        // 命中 trajectory JSONL 的超长行，把结果撑到 MB 级、撑爆模型输入预算。
        // 内部状态目录必须永不被搜（ripgrep 与 in-process 兜底都排除）。
        let root = temp_root("grep-skip");
        fs::create_dir_all(root.join(".memory/trajectories")).unwrap();
        fs::create_dir_all(root.join(".cache")).unwrap();
        fs::write(
            root.join(".memory/trajectories/t.jsonl"),
            "needle in the trajectory\n",
        )
        .unwrap();
        fs::write(root.join(".cache/backup.log"), "needle backup\n").unwrap();
        fs::write(root.join("src.rs"), "needle in real source\n").unwrap();
        let reg = registry(&root);
        let out = reg
            .execute(
                "grep",
                json!({"pattern": "needle", "output_mode": "content"}),
            )
            .await
            .unwrap();
        assert!(out.content.contains("src.rs"), "real source is searched");
        assert!(
            !out.content.contains(".memory") && !out.content.contains(".cache"),
            "internal dirs skipped: {}",
            out.content
        );
        fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn grep_bounds_a_pathological_line() {
        // A single very long line (JSONL / minified) must not be returned
        // verbatim — ripgrep's `--max-columns 500` prunes it.
        let root = temp_root("grep-longline");
        let long = format!("{}needle{}", "a".repeat(100_000), "b".repeat(100_000));
        fs::write(root.join("big.jsonl"), format!("{long}\n")).unwrap();
        let reg = registry(&root);
        let out = reg
            .execute(
                "grep",
                json!({"pattern": "needle", "output_mode": "content"}),
            )
            .await
            .unwrap();
        assert!(
            out.content.chars().count() < 2000,
            "one long hit line is capped, not echoed: {} chars",
            out.content.chars().count()
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn grep_helper_split_glob_list_keeps_braces() {
        assert_eq!(
            split_glob_list("*.rs,*.toml"),
            vec!["*.rs".to_string(), "*.toml".to_string()]
        );
        assert_eq!(
            split_glob_list("*.{ts,tsx} *.rs"),
            vec!["*.{ts,tsx}".to_string(), "*.rs".to_string()]
        );
        assert!(split_glob_list("").is_empty());
    }

    #[test]
    fn grep_helper_limit_defaults_to_250_and_zero_is_unlimited() {
        // Default cap when unspecified.
        assert_eq!(apply_limit(1000, 0, None), (0, 250, Some(250)));
        // Explicit 0 = unlimited.
        assert_eq!(apply_limit(1000, 0, Some(0)), (0, 1000, None));
        // Offset then limit.
        assert_eq!(apply_limit(1000, 10, Some(5)), (10, 15, Some(5)));
        // No truncation → no reported limit.
        assert_eq!(apply_limit(3, 0, Some(10)), (0, 3, None));
    }

    #[test]
    fn grep_helper_build_args_adds_ripgrep_flags() {
        let args = GrepArgs {
            pattern: "-flag".to_string(),
            path: String::new(),
            glob: "*.rs".to_string(),
            output_mode: "content".to_string(),
            context_before: None,
            context_after: None,
            context_c: Some(2),
            show_line_numbers: None,
            case_insensitive: Some(true),
            file_type: String::new(),
            head_limit: None,
            offset: None,
            multiline: false,
        };
        let rg = build_grep_args(&args, "content");
        assert!(rg.contains(&"--max-columns".to_string()));
        assert!(rg.contains(&"!.git".to_string()), "VCS dir excluded");
        assert!(rg.contains(&"-i".to_string()));
        assert!(rg.contains(&"-C".to_string()) && rg.contains(&"2".to_string()));
        // A `-`-prefixed pattern is passed via `-e`.
        let e = rg.iter().position(|a| a == "-e").unwrap();
        assert_eq!(rg[e + 1], "-flag");
        assert!(rg.contains(&"--glob".to_string()) && rg.contains(&"*.rs".to_string()));
    }

    #[test]
    fn exclude_args_cover_vcs_and_internal_dirs() {
        let args = exclusion_args();
        for needle in ["!.git", "!target", "!.memory", "!.cache"] {
            assert!(args.contains(&needle.to_string()), "missing {needle}");
        }
    }

    #[tokio::test]
    async fn list_directory_reports_sizes() {
        let root = temp_root("ls");
        fs::write(root.join("a.txt"), "abc").unwrap();
        fs::create_dir_all(root.join("d")).unwrap();
        let reg = registry(&root);
        let out = reg.execute("list_directory", json!({})).await.unwrap();
        assert!(out.content.contains("a.txt (3B)"));
        assert!(out.content.contains("d/"));
        fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn copy_file_copies_and_rejects_escape() {
        let root = temp_root("cp");
        fs::write(root.join("a.txt"), "data").unwrap();
        let reg = registry(&root);
        let out = reg
            .execute("copy_file", json!({"src": "a.txt", "dst": "b.txt"}))
            .await
            .unwrap();
        assert!(out.content.contains("Copied"));
        assert_eq!(fs::read_to_string(root.join("b.txt")).unwrap(), "data");
        let err = reg
            .execute("copy_file", json!({"src": "a.txt", "dst": "../escape.txt"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::PathNotAllowed { .. }));
        fs::remove_dir_all(&root).ok();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_stalled_bash_is_cut_off_by_the_wait_judge() {
        // F 项（cli 2026-10-05 拍板）端到端：一条**长时间无输出**的真实命令，
        // 由等待裁判（而不是它自己那个 300s 上限）掐断——这正是「太多 timeout
        // 等待时间过长」要修的场景。裁决者在这里是一个 stub（若真去调模型，
        // 这条测试就依赖网络了）；生产里它是 `ModelJudge`。
        struct InterruptOnce;
        #[async_trait::async_trait]
        impl lingmiao_core::polling::WaitJudge for InterruptOnce {
            async fn judge(
                &self,
                state: &lingmiao_core::polling::WaitState,
            ) -> lingmiao_core::polling::Verdict {
                lingmiao_core::polling::Verdict::Interrupt(format!(
                    "{} 已静默 {:.0}s，判定卡死",
                    state.what,
                    state.silent.as_secs_f64()
                ))
            }
        }
        let root = temp_root("bash-stall");
        let reg = registry(&root);
        let started = std::time::Instant::now();
        let out = lingmiao_core::polling::with_judge(
            Some(Arc::new(InterruptOnce) as Arc<dyn lingmiao_core::polling::WaitJudge>),
            reg.execute("bash", json!({"command": "sleep 300", "timeout": 300})),
        )
        .await
        .unwrap();
        let secs = started.elapsed().as_secs_f64();
        assert!(out.is_error);
        assert!(out.content.contains("已中断等待"), "{}", out.content);
        assert!(out.content.contains("bash: sleep 300"), "{}", out.content);
        assert!(
            secs < 120.0,
            "the wait must end on the judge's ruling, not on the 300s limit ({secs}s)"
        );
        assert!(
            out.content.contains("已静默"),
            "the report names the actual silence: {}",
            out.content
        );
        // The wait ended on the ruling, not on the 300s limit — that is the whole
        // property under test here. (That the killed command's *grandchildren*
        // are reaped too is asserted deterministically in
        // `lingmiao_core::proc::tests::kill_group_reaps_a_grandchild`.)
        fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn bash_runs_and_times_out() {
        let root = temp_root("bash");
        let reg = registry(&root);
        let out = reg
            .execute("bash", json!({"command": "echo hi"}))
            .await
            .unwrap();
        assert!(out.content.contains("exit=0"));
        assert!(out.content.contains("hi"));
        assert!(!out.is_error);
        // A non-zero exit is reported as an error output.
        let bad = reg
            .execute("bash", json!({"command": "exit 3"}))
            .await
            .unwrap();
        assert!(bad.is_error);
        // A short timeout aborts a long sleep without hanging.
        let to = reg
            .execute("bash", json!({"command": "sleep 5", "timeout": 1}))
            .await
            .unwrap();
        assert!(to.is_error);
        assert!(to.content.contains("timed out"));
        fs::remove_dir_all(&root).ok();
    }
}

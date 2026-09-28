//! `verify` tool group — the **acceptance oracle** (原版 `verifier.py`
//! 的 Rust 落地, 551 行纯代码 DSL, 无 LLM).
//!
//! A [`Tool`] that parses an `acceptance` DSL block out of a design document
//! and mechanically re-runs every assertion against the real working directory.
//! The oracle has veto power: an assertion either passes or it does not — no
//! model judgement involved.
//!
//! ## DSL (one assertion per line inside a ```acceptance fenced block)
//!
//! ```text
//! file_exists <path>
//! file_not_exists <path>
//! file_contains <path> :: <substring>
//! file_not_contains <path> :: <substring>
//! command_succeeds <cmd...>
//! command_fails <cmd...>
//! command_output_contains <cmd...> :: <substring>
//! freeze <path>
//! ```
//!
//! Commands may use `{python}` / `{cargo}` placeholders, expanded at run time
//! to the current interpreter / cargo (`{python} -m pytest` survives across
//! machines and virtualenvs).
//!
//! ## Four-layer acceptance tree
//!
//! Comment lines inside the block set the current group for the assertions that
//! follow:
//!
//! ```text
//! # [需求 R-01] 计算器支持四则运算
//! file_exists calc.py
//! # [验收 A-01] 加法单测通过
//! command_succeeds {python} -m pytest test_calc.py -q
//! ```
//!
//! Layer keywords: 需求 / 功能 / 模块 / 验收. The report aggregates pass rates
//! per layer.
//!
//! ## Honesty assertions (P1 · 最低限度诚实性检查)
//!
//! A deliberately **weak** floor (not a correctness proof): `doc_length_min`,
//! `ref_density_min`, `assert_no_summary_only`, `assert_needs_clarification`,
//! `assert_traceable`.
//!
//! Following M6.4「真源不抄」the kind sets and layer keywords are code
//! constants reviewed beside the runner — the docs explain *why*.

use std::path::{Component, Path, PathBuf};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::tool::{Tool, ToolError, ToolOutput, ToolRegistry, json_schema};

/// Tools this module contributes.
pub const TOOL_NAMES: [&str; 1] = ["verify_acceptance"];

/// File-based assertion kinds.
const FILE_KINDS: [&str; 4] = [
    "file_exists",
    "file_not_exists",
    "file_contains",
    "file_not_contains",
];
/// Command assertion kinds.
const COMMAND_KINDS: [&str; 3] = [
    "command_succeeds",
    "command_fails",
    "command_output_contains",
];
/// P1 honesty assertion kinds.
const HONESTY_KINDS: [&str; 5] = [
    "doc_length_min",
    "ref_density_min",
    "assert_no_summary_only",
    "assert_needs_clarification",
    "assert_traceable",
];
/// Layer keywords of the four-layer acceptance tree.
const LAYER_KEYWORDS: [&str; 4] = ["需求", "功能", "模块", "验收"];

/// The `::` separator between a command/path and its expected text.
const SEPARATOR: &str = " :: ";

/// Default per-command timeout (seconds) — mirrors the Python default of 120.
const DEFAULT_CMD_TIMEOUT: u64 = 120;

/// One mechanical acceptance assertion.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Assertion {
    /// Assertion kind (e.g. `file_exists`).
    pub kind: String,
    /// Target path (file kinds / honesty kinds / freeze).
    pub path: String,
    /// Command line (command kinds) or extra payload (honesty kinds).
    pub cmd: String,
    /// Expected substring / threshold.
    pub expected: String,
    /// Raw source line (stable identity for stuck detection).
    pub raw: String,
    /// Four-layer tree group id (e.g. `R-01` / `A-01`).
    pub group: String,
}

impl Assertion {
    fn new(kind: &str, raw: &str) -> Self {
        Self {
            kind: kind.to_string(),
            raw: raw.to_string(),
            ..Default::default()
        }
    }

    fn is_command(&self) -> bool {
        COMMAND_KINDS.contains(&self.kind.as_str())
    }

    fn is_honesty(&self) -> bool {
        HONESTY_KINDS.contains(&self.kind.as_str())
    }

    fn is_freeze(&self) -> bool {
        self.kind == "freeze"
    }
}

/// The outcome of running one assertion.
#[derive(Debug, Clone)]
pub struct CheckResult {
    /// The assertion that was run.
    pub assertion: Assertion,
    /// Whether it passed.
    pub ok: bool,
    /// Failure detail (empty when it passed).
    pub detail: String,
}

// ── parsing ─────────────────────────────────────────────────────

/// Extract the bodies of every ```acceptance fenced block in `doc`.
fn acceptance_blocks(doc: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut in_block = false;
    let mut cur = String::new();
    for line in doc.lines() {
        let trimmed = line.trim_start();
        if !in_block {
            if trimmed.starts_with("```acceptance") {
                in_block = true;
                cur.clear();
            }
        } else if trimmed.starts_with("```") {
            blocks.push(cur.clone());
            in_block = false;
        } else {
            cur.push_str(line);
            cur.push('\n');
        }
    }
    blocks
}

/// Parse a `# [层 标识] 描述` group comment into its id (e.g. `R-01`).
fn parse_group_comment(line: &str) -> Option<String> {
    let rest = line.trim_start().strip_prefix('#')?.trim_start();
    let rest = rest.strip_prefix('[')?;
    let end = rest.find(']')?;
    let inner = rest[..end].trim();
    let mut parts = inner.split_whitespace();
    let first = parts.next()?;
    if LAYER_KEYWORDS.contains(&first) {
        parts.next().map(str::to_string)
    } else {
        None
    }
}

/// Minimal `shlex`-style split (honours single/double quotes and backslash).
fn shlex_split(s: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut started = false;
    let mut in_single = false;
    let mut in_double = false;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' if !in_double => {
                in_single = !in_single;
                started = true;
            }
            '"' if !in_single => {
                in_double = !in_double;
                started = true;
            }
            '\\' if !in_single => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                    started = true;
                }
            }
            c if c.is_whitespace() && !in_single && !in_double => {
                if started || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            c => {
                cur.push(c);
                started = true;
            }
        }
    }
    if in_single || in_double {
        return Err("引号未闭合".to_string());
    }
    if started || !cur.is_empty() {
        out.push(cur);
    }
    Ok(out)
}

/// Parse one DSL line into an assertion (or an error string). Returns
/// `Ok(None)` for an empty/blank line.
fn parse_line(line: &str) -> Result<Option<Assertion>, String> {
    let (body, sep, expected) = match line.split_once(SEPARATOR) {
        Some((b, e)) => (b, true, e.to_string()),
        None => (line, false, String::new()),
    };
    let tokens = shlex_split(body).map_err(|e| format!("无法解析行: {line} ({e})"))?;
    if tokens.is_empty() {
        return Ok(None);
    }
    let kind = tokens[0].as_str();
    let args = &tokens[1..];
    if !FILE_KINDS.contains(&kind)
        && !COMMAND_KINDS.contains(&kind)
        && !HONESTY_KINDS.contains(&kind)
        && kind != "freeze"
    {
        return Err(format!("未知断言类型 '{kind}': {line}"));
    }

    if kind == "freeze" {
        if args.len() != 1 {
            return Err(format!("freeze 需要一个路径参数: {line}"));
        }
        let mut a = Assertion::new(kind, line);
        a.path = args[0].clone();
        return Ok(Some(a));
    }

    if FILE_KINDS.contains(&kind) {
        if args.len() != 1 {
            return Err(format!("{kind} 需要一个路径参数: {line}"));
        }
        if matches!(kind, "file_contains" | "file_not_contains") && !sep {
            return Err(format!("{kind} 需要 '::' 分隔的期望文本: {line}"));
        }
        let mut a = Assertion::new(kind, line);
        a.path = args[0].clone();
        a.expected = expected;
        return Ok(Some(a));
    }

    if HONESTY_KINDS.contains(&kind) {
        if args.is_empty() {
            return Err(format!("{kind} 需要路径参数: {line}"));
        }
        if matches!(kind, "doc_length_min" | "ref_density_min") && !sep {
            return Err(format!("{kind} 需要 '::' 分隔的数字阈值: {line}"));
        }
        if kind == "assert_traceable" {
            if !sep {
                return Err(format!(
                    "assert_traceable 需要 '::' 分隔的 doc 路径: {line}"
                ));
            }
            if expected.trim().is_empty() {
                return Err(format!("assert_traceable 的 doc 路径为空: {line}"));
            }
            let mut a = Assertion::new(kind, line);
            a.path = args[0].clone();
            a.cmd = expected.trim().to_string();
            a.expected = expected.trim().to_string();
            return Ok(Some(a));
        }
        let mut a = Assertion::new(kind, line);
        a.path = args[0].clone();
        a.cmd = args[1..].join(" ");
        a.expected = expected;
        return Ok(Some(a));
    }

    // command kinds
    if args.is_empty() {
        return Err(format!("{kind} 需要命令: {line}"));
    }
    if kind == "command_output_contains" && !sep {
        return Err(format!(
            "command_output_contains 需要 '::' 分隔的期望文本: {line}"
        ));
    }
    let mut a = Assertion::new(kind, line);
    a.cmd = args.join(" ");
    a.expected = expected;
    Ok(Some(a))
}

/// Extract every assertion from `doc`'s ```acceptance blocks.
///
/// Returns `(assertions, errors)`. Unknown/malformed lines become errors so the
/// caller can reject the document at intake.
pub fn parse_acceptance(doc: &str) -> (Vec<Assertion>, Vec<String>) {
    let mut assertions: Vec<Assertion> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    let blocks = acceptance_blocks(doc);
    if blocks.is_empty() {
        errors.push("文档中没有 ```acceptance 断言块".to_string());
        return (assertions, errors);
    }
    for block in blocks {
        let mut group = String::new();
        for line in block.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if line.starts_with('#') {
                if let Some(g) = parse_group_comment(line) {
                    group = g;
                }
                continue;
            }
            match parse_line(line) {
                Ok(None) => {}
                Ok(Some(mut a)) => {
                    a.group = group.clone();
                    assertions.push(a);
                }
                Err(e) => errors.push(e),
            }
        }
    }
    if assertions.is_empty() && errors.is_empty() {
        errors.push("acceptance 断言块为空".to_string());
    }
    (assertions, errors)
}

// ── command placeholders ────────────────────────────────────────

/// Expand `{python}` / `{cargo}` placeholders in a command line.
///
/// `{python}` maps to `$LINGMIAO_PYTHON` or `python3`; `{cargo}` to `$CARGO` or
/// `cargo`. Resolved at run time so a command survives across machines.
pub fn expand_command(cmd: &str) -> String {
    let python = std::env::var("LINGMIAO_PYTHON").unwrap_or_else(|_| "python3".to_string());
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    cmd.replace("{python}", &python).replace("{cargo}", &cargo)
}

// ── path safety ─────────────────────────────────────────────────

/// Join `path` onto `root`, rejecting any escape above `root` (mirrors the
/// Python `target.relative_to(cwd)` guard). The target need not exist yet.
fn safe_join(root: &Path, path: &str) -> Option<PathBuf> {
    let joined = root.join(path);
    let mut norm = PathBuf::new();
    for comp in joined.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                norm.pop();
            }
            other => norm.push(other.as_os_str()),
        }
    }
    if norm.starts_with(root) {
        Some(norm)
    } else {
        None
    }
}

// ── honesty helpers ─────────────────────────────────────────────

/// Known document extensions counted as "references" by `ref_density_min`.
const REF_EXTS: [&str; 8] = ["py", "md", "json", "ts", "js", "css", "toml", "txt"];

/// Approximate reference count: backtick-quoted spans + path-like tokens.
fn count_references(content: &str) -> usize {
    let mut refs = 0usize;
    // Backtick spans.
    let mut rest = content;
    while let Some(start) = rest.find('`') {
        let after = &rest[start + 1..];
        match after.find('`') {
            Some(end) => {
                refs += 1;
                rest = &after[end + 1..];
            }
            None => break,
        }
    }
    // Whitespace-separated tokens ending in a known extension.
    for tok in content.split_whitespace() {
        let cleaned = tok.trim_matches(|c: char| "`()[]<>,;:'\"".contains(c));
        if let Some((_, ext)) = cleaned.rsplit_once('.') {
            if REF_EXTS.contains(&ext) {
                refs += 1;
            }
        }
    }
    refs
}

/// Whether the doc has *concrete* content (a code fence / acceptance block /
/// file-path mention) rather than summary-only prose.
fn has_concrete_content(content: &str) -> bool {
    if content.contains("```") {
        return true;
    }
    if !acceptance_blocks(content).is_empty() {
        return true;
    }
    content.split_whitespace().any(|tok| {
        let cleaned = tok.trim_matches(|c: char| "`()[]<>,;:'\"".contains(c));
        cleaned
            .rsplit_once('.')
            .is_some_and(|(_, ext)| REF_EXTS.contains(&ext))
    })
}

/// Case-insensitive count of `[NEEDS CLARIFICATION]` markers.
fn count_needs_clarification(content: &str) -> usize {
    let upper = content.to_uppercase();
    let needle = "[NEEDS CLARIFICATION]";
    let mut count = 0;
    let mut from = 0;
    while let Some(i) = upper[from..].find(needle) {
        count += 1;
        from += i + needle.len();
    }
    count
}

/// Lightweight enumerated-component extraction (the Python `extract_components`
/// stand-in): lines introduced by ①-⑩ / `1.` / `-` / `*`.
fn extract_components(spec: &str) -> Vec<String> {
    let circled: Vec<char> = "①②③④⑤⑥⑦⑧⑨⑩".chars().collect();
    let mut out = Vec::new();
    for line in spec.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        let first = t.chars().next().unwrap_or(' ');
        let is_circled = circled.contains(&first);
        let is_list = t.starts_with("- ")
            || t.starts_with("* ")
            || t.starts_with("+ ")
            || numbered_marker(t).is_some();
        if is_circled || is_list {
            out.push(strip_marker(t));
        }
    }
    out
}

/// If `t` starts with `\d+[.、)]`, return the marker length (bytes).
fn numbered_marker(t: &str) -> Option<usize> {
    let mut it = t.char_indices().peekable();
    let mut digits = 0;
    while let Some((_, c)) = it.peek() {
        if c.is_ascii_digit() {
            digits += 1;
            it.next();
        } else {
            break;
        }
    }
    if digits == 0 {
        return None;
    }
    match it.peek() {
        Some((i, '.') | (i, '、') | (i, ')')) => Some(i + 1),
        _ => None,
    }
}

/// Strip a leading enumeration marker (①-⑩ / `1.` / `-` / `*`) and whitespace.
fn strip_marker(s: &str) -> String {
    let circled: Vec<char> = "①②③④⑤⑥⑦⑧⑨⑩".chars().collect();
    let trimmed = s.trim_start();
    if let Some(first) = trimmed.chars().next() {
        if circled.contains(&first) {
            return trimmed[first.len_utf8()..].trim().to_string();
        }
        if trimmed.starts_with("- ") || trimmed.starts_with("* ") || trimmed.starts_with("+ ") {
            return trimmed[2..].trim().to_string();
        }
    }
    if let Some(n) = numbered_marker(trimmed) {
        return trimmed[n..].trim().to_string();
    }
    trimmed.trim().to_string()
}

fn normalize(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Whether a spec component is traceable in the design doc text.
fn component_traceable(component: &str, doc_norm: &str) -> bool {
    let key = normalize(&strip_marker(component));
    if key.is_empty() {
        return true;
    }
    if doc_norm.contains(&key) {
        return true;
    }
    let tokens: Vec<&str> = key
        .split(' ')
        .take(3)
        .filter(|t| t.chars().count() >= 2)
        .collect();
    if tokens.iter().any(|t| doc_norm.contains(t)) {
        return true;
    }
    // CJK suffix-gram fallback: the doc often restates the noun without the verb.
    for tok in &tokens {
        let chars: Vec<char> = tok.chars().collect();
        for i in (2..=chars.len()).rev() {
            let gram: String = chars[chars.len() - i..].iter().collect();
            if doc_norm.contains(&gram) {
                return true;
            }
        }
    }
    false
}

/// Whether the doc contains at least one `R-<digits>` requirement-group marker.
fn has_requirement_marker(doc: &str) -> bool {
    let bytes = doc.as_bytes();
    let mut i = 0;
    while i + 2 < bytes.len() {
        if bytes[i] == b'R' && bytes[i + 1] == b'-' && bytes[i + 2].is_ascii_digit() {
            return true;
        }
        i += 1;
    }
    false
}

// ── execution ───────────────────────────────────────────────────

/// Run file-based + honesty assertions synchronously (pure I/O, no process).
fn run_sync(root: &Path, a: &Assertion) -> CheckResult {
    if a.is_honesty() {
        return run_honesty(root, a);
    }
    run_file(root, a)
}

fn run_file(root: &Path, a: &Assertion) -> CheckResult {
    let Some(target) = safe_join(root, &a.path) else {
        return CheckResult {
            assertion: a.clone(),
            ok: false,
            detail: format!("路径越出工作目录: {}", a.path),
        };
    };
    let exists = target.is_file();
    match a.kind.as_str() {
        "file_exists" => CheckResult {
            assertion: a.clone(),
            ok: exists,
            detail: if exists {
                String::new()
            } else {
                "文件不存在".into()
            },
        },
        "file_not_exists" => CheckResult {
            assertion: a.clone(),
            ok: !exists,
            detail: if exists {
                "文件仍然存在".into()
            } else {
                String::new()
            },
        },
        _ => {
            if !exists {
                return CheckResult {
                    assertion: a.clone(),
                    ok: false,
                    detail: "文件不存在".into(),
                };
            }
            let content = std::fs::read_to_string(&target).unwrap_or_default();
            if a.kind == "file_contains" {
                let ok = content.contains(&a.expected);
                CheckResult {
                    assertion: a.clone(),
                    ok,
                    detail: if ok {
                        String::new()
                    } else {
                        format!("文件不含 '{}'", a.expected)
                    },
                }
            } else {
                let ok = !content.contains(&a.expected);
                CheckResult {
                    assertion: a.clone(),
                    ok,
                    detail: if ok {
                        String::new()
                    } else {
                        format!("文件仍含 '{}'", a.expected)
                    },
                }
            }
        }
    }
}

fn run_honesty(root: &Path, a: &Assertion) -> CheckResult {
    let fail = |detail: String| CheckResult {
        assertion: a.clone(),
        ok: false,
        detail,
    };
    let Some(target) = safe_join(root, &a.path) else {
        return fail(format!("路径越出工作目录: {}", a.path));
    };

    match a.kind.as_str() {
        "doc_length_min" | "ref_density_min" => {
            if !target.is_file() {
                return fail("文件不存在".into());
            }
            let Ok(threshold) = a.expected.trim().parse::<usize>() else {
                return fail(format!("阈值不是整数: {}", a.expected));
            };
            let content = std::fs::read_to_string(&target).unwrap_or_default();
            if a.kind == "doc_length_min" {
                let n = content.chars().count();
                CheckResult {
                    assertion: a.clone(),
                    ok: n >= threshold,
                    detail: if n >= threshold {
                        String::new()
                    } else {
                        format!("文档长度 {n} < 阈值 {threshold}")
                    },
                }
            } else {
                let refs = count_references(&content);
                CheckResult {
                    assertion: a.clone(),
                    ok: refs >= threshold,
                    detail: if refs >= threshold {
                        String::new()
                    } else {
                        format!("引用密度 {refs} < 阈值 {threshold}")
                    },
                }
            }
        }
        "assert_no_summary_only" => {
            if !target.is_file() {
                return fail("文件不存在".into());
            }
            let content = std::fs::read_to_string(&target).unwrap_or_default();
            let ok = has_concrete_content(&content);
            CheckResult {
                assertion: a.clone(),
                ok,
                detail: if ok {
                    String::new()
                } else {
                    "文档只有概括文字，缺少代码/文件/断言块等具体内容".into()
                },
            }
        }
        "assert_needs_clarification" => {
            if !target.is_file() {
                return fail("文件不存在".into());
            }
            let content = std::fs::read_to_string(&target).unwrap_or_default();
            let n = count_needs_clarification(&content);
            CheckResult {
                assertion: a.clone(),
                ok: n == 0,
                detail: if n == 0 {
                    String::new()
                } else {
                    format!("文档仍有 {n} 处未解决的 [NEEDS CLARIFICATION] 标记")
                },
            }
        }
        "assert_traceable" => {
            let doc_rel = a.expected.trim();
            let Some(spec_path) = safe_join(root, &a.path) else {
                return fail("路径越出工作目录".into());
            };
            let Some(doc_path) = safe_join(root, doc_rel) else {
                return fail("路径越出工作目录".into());
            };
            if !spec_path.is_file() {
                return fail("spec 文件不存在".into());
            }
            if !doc_path.is_file() {
                return fail("设计文档不存在".into());
            }
            let spec_text = std::fs::read_to_string(&spec_path).unwrap_or_default();
            let doc_text = std::fs::read_to_string(&doc_path).unwrap_or_default();
            let components = extract_components(&spec_text);
            if components.is_empty() {
                return CheckResult {
                    assertion: a.clone(),
                    ok: true,
                    detail: "spec 无枚举组件，可追踪链天然满足".into(),
                };
            }
            if !has_requirement_marker(&doc_text) {
                return fail("设计文档没有 R-* 需求分组标记（无法建立可追踪链）".into());
            }
            let doc_norm = normalize(&doc_text);
            let missing: Vec<String> = components
                .iter()
                .filter(|c| !component_traceable(c, &doc_norm))
                .map(|c| c.chars().take(40).collect())
                .collect();
            let ok = missing.is_empty();
            CheckResult {
                assertion: a.clone(),
                ok,
                detail: if ok {
                    String::new()
                } else {
                    format!("spec 组件未在设计文档中引用: {}", missing.join("; "))
                },
            }
        }
        _ => fail(format!("未实现的诚实性断言类型: {}", a.kind)),
    }
}

/// Run one command assertion through `sh -c` with a timeout.
async fn run_command(root: &Path, a: &Assertion, timeout_secs: u64) -> CheckResult {
    let cmd = expand_command(&a.cmd);
    let mut command = tokio::process::Command::new("sh");
    command
        .arg("-c")
        .arg(&cmd)
        .current_dir(root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let output = match tokio::time::timeout(
        std::time::Duration::from_secs(timeout_secs),
        command.output(),
    )
    .await
    {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            return CheckResult {
                assertion: a.clone(),
                ok: false,
                detail: format!("命令执行失败: {e}"),
            };
        }
        Err(_) => {
            return CheckResult {
                assertion: a.clone(),
                ok: false,
                detail: format!("命令超时（{timeout_secs}s）: {cmd}"),
            };
        }
    };
    let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
    combined.push_str(&String::from_utf8_lossy(&output.stderr));
    let code = output.status.code().unwrap_or(-1);
    let tail = |s: &str| -> String {
        let chars: Vec<char> = s.chars().collect();
        let start = chars.len().saturating_sub(300);
        chars[start..].iter().collect()
    };
    match a.kind.as_str() {
        "command_succeeds" => CheckResult {
            assertion: a.clone(),
            ok: code == 0,
            detail: if code == 0 {
                String::new()
            } else {
                format!("退出码 {code}: {}", tail(&combined))
            },
        },
        "command_fails" => CheckResult {
            assertion: a.clone(),
            ok: code != 0,
            detail: if code != 0 {
                String::new()
            } else {
                "命令意外成功".into()
            },
        },
        _ => {
            let ok = code == 0 && combined.contains(&a.expected);
            let detail = if ok {
                String::new()
            } else if code != 0 {
                format!("退出码 {code}: {}", tail(&combined))
            } else {
                format!("输出不含 '{}'", a.expected)
            };
            CheckResult {
                assertion: a.clone(),
                ok,
                detail,
            }
        }
    }
}

/// An aggregate verification report.
#[derive(Debug, Clone)]
pub struct VerificationReport {
    /// One result per non-`freeze` assertion.
    pub results: Vec<CheckResult>,
    /// `freeze` paths declared in the block (enforced by the landing loop, not here).
    pub frozen_paths: Vec<String>,
}

impl VerificationReport {
    /// Whether at least one assertion ran and every one passed.
    pub fn all_passed(&self) -> bool {
        !self.results.is_empty() && self.results.iter().all(|r| r.ok)
    }

    /// Stable identity of the failing assertion set (stuck detection).
    pub fn failing_keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = self
            .results
            .iter()
            .filter(|r| !r.ok)
            .map(|r| r.assertion.raw.clone())
            .collect();
        keys.sort();
        keys
    }

    /// Pass rates per four-layer group, in first-seen order.
    pub fn by_layer(&self) -> Vec<(String, usize, usize)> {
        let mut order: Vec<String> = Vec::new();
        let mut counts: std::collections::BTreeMap<String, (usize, usize)> =
            std::collections::BTreeMap::new();
        for r in &self.results {
            let g = if r.assertion.group.is_empty() {
                "未分组".to_string()
            } else {
                r.assertion.group.clone()
            };
            let e = counts.entry(g.clone()).or_insert((0, 0));
            if !order.contains(&g) {
                order.push(g);
            }
            e.0 += 1;
            if r.ok {
                e.1 += 1;
            }
        }
        order
            .into_iter()
            .map(|g| {
                let (total, passed) = counts[&g];
                (g, total, passed)
            })
            .collect()
    }
}

/// Run every non-`freeze` assertion against `root`.
pub async fn run_assertions(
    root: &Path,
    assertions: &[Assertion],
    timeout_secs: u64,
) -> VerificationReport {
    let mut results = Vec::new();
    let mut frozen = Vec::new();
    for a in assertions {
        if a.is_freeze() {
            frozen.push(a.path.clone());
            continue;
        }
        let result = if a.is_command() {
            run_command(root, a, timeout_secs).await
        } else {
            run_sync(root, a)
        };
        results.push(result);
    }
    VerificationReport {
        results,
        frozen_paths: frozen,
    }
}

fn render_report(report: &VerificationReport) -> Value {
    let passed = report.results.iter().filter(|r| r.ok).count();
    let failed = report.results.len() - passed;
    let by_layer: Vec<Value> = report
        .by_layer()
        .into_iter()
        .map(|(group, total, ok)| json!({"group": group, "total": total, "passed": ok}))
        .collect();
    let results: Vec<Value> = report
        .results
        .iter()
        .map(|r| {
            json!({
                "group": r.assertion.group,
                "kind": r.assertion.kind,
                "raw": r.assertion.raw,
                "ok": r.ok,
                "detail": r.detail,
            })
        })
        .collect();
    json!({
        "count": report.results.len(),
        "passed": passed,
        "failed": failed,
        "all_passed": report.all_passed(),
        "failing_keys": report.failing_keys(),
        "by_layer": by_layer,
        "frozen_paths": report.frozen_paths,
        "results": results,
    })
}

// ── tool ────────────────────────────────────────────────────────

/// Arguments for `verify_acceptance`.
#[derive(Deserialize, schemars::JsonSchema)]
struct VerifyArgs {
    /// Path to a design document containing the ```acceptance block(s),
    /// relative to the working directory. Use this OR `assertions`.
    #[serde(default)]
    document: String,
    /// Inline acceptance DSL text (used when `document` is empty).
    #[serde(default)]
    assertions: String,
    /// Working directory the assertions run against. Omit for the project root.
    #[serde(default)]
    cwd: String,
    /// Parse only — do not execute commands (default false).
    #[serde(default)]
    dry_run: bool,
    /// Per-command timeout in seconds (default 120).
    #[serde(default)]
    command_timeout: u64,
}

/// `verify_acceptance` — parse + mechanically run an acceptance DSL.
pub struct VerifyAcceptanceTool {
    root: PathBuf,
}

#[async_trait]
impl Tool for VerifyAcceptanceTool {
    fn name(&self) -> &str {
        "verify_acceptance"
    }

    fn description(&self) -> &str {
        "机械验收（纯代码 oracle，无 LLM）：从设计文档的 ```acceptance 围栏块（或内联 DSL）\
         解析断言，并对真实工作目录逐条复跑——文件存在/包含、命令成败/输出包含、以及 P1 诚实性\
         断言（文档长度/引用密度/反概括/未决标记/需求可追踪链）。返回逐条结果与按「需求/功能/模块/\
         验收」四层聚合的通过率。dry_run=true 只解析不执行命令。"
    }

    fn parameters(&self) -> Value {
        json_schema::<VerifyArgs>()
    }

    fn is_mutating(&self) -> bool {
        // Commands run by the assertions may write; conservatively flagged.
        true
    }

    async fn execute(&self, arguments: Value) -> Result<ToolOutput, ToolError> {
        let a: VerifyArgs = serde_json::from_value(arguments)
            .map_err(|e| ToolError::invalid("verify_acceptance", e.to_string()))?;

        let root = if a.cwd.trim().is_empty() {
            self.root.clone()
        } else {
            let candidate = self.root.join(a.cwd.trim());
            if !candidate.is_dir() {
                return Err(ToolError::invalid(
                    "verify_acceptance",
                    format!("cwd `{}` 不是目录", a.cwd),
                ));
            }
            candidate
        };

        let (source, doc) = if !a.document.trim().is_empty() {
            let Some(p) = safe_join(&root, a.document.trim()) else {
                return Err(ToolError::invalid(
                    "verify_acceptance",
                    format!("document 路径越出工作目录: {}", a.document),
                ));
            };
            let text = std::fs::read_to_string(&p).map_err(|e| ToolError::io(&a.document, e))?;
            (format!("document:{}", a.document.trim()), text)
        } else if !a.assertions.trim().is_empty() {
            ("inline".to_string(), a.assertions.clone())
        } else {
            return Err(ToolError::invalid(
                "verify_acceptance",
                "one of `document` or `assertions` is required",
            ));
        };

        let (assertions, parse_errors) = parse_acceptance(&doc);
        let timeout = if a.command_timeout == 0 {
            DEFAULT_CMD_TIMEOUT
        } else {
            a.command_timeout.min(600)
        };

        if a.dry_run {
            let listed: Vec<Value> = assertions
                .iter()
                .map(|x| {
                    json!({
                        "group": x.group,
                        "kind": x.kind,
                        "raw": x.raw,
                        "path": x.path,
                        "cmd": x.cmd,
                        "expected": x.expected,
                    })
                })
                .collect();
            return Ok(ToolOutput::ok(to_pretty(&json!({
                "cwd": root.display().to_string(),
                "source": source,
                "dry_run": true,
                "assertions": listed,
                "parse_errors": parse_errors,
            }))));
        }

        let report = run_assertions(&root, &assertions, timeout).await;
        let mut value = render_report(&report);
        value["cwd"] = json!(root.display().to_string());
        value["source"] = json!(source);
        value["parse_errors"] = json!(parse_errors);

        let header = if report.all_passed() {
            format!(
                "✅ 验收通过（{}/{}）",
                report.results.len(),
                report.results.len()
            )
        } else {
            format!(
                "❌ 验收未通过（{}/{}）",
                report.results.iter().filter(|r| r.ok).count(),
                report.results.len()
            )
        };
        Ok(ToolOutput {
            content: format!("{header}\n{}", to_pretty(&value)),
            is_error: !report.all_passed(),
            // The acceptance oracle produces a report, not a file mutation — no
            // line diff to show (the field is always present, like every tool).
            diff: Vec::new(),
        })
    }
}

fn to_pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string())
}

/// Register the `verify` group against the project root.
pub fn register(registry: &mut ToolRegistry, root: &Path) {
    registry.register(VerifyAcceptanceTool {
        root: root.to_path_buf(),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("lingmiao-verify-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn parses_all_kinds_with_layers() {
        let doc = "\
intro
```acceptance
# [需求 R-01] 计算器
file_exists calc.py
# [功能 F-01] 加法
file_contains calc.py :: def add
command_succeeds echo hi
command_fails false
command_output_contains echo hello :: hello
freeze test_calc.py
# [验收 A-01] 单测
doc_length_min README.md :: 10
```
";
        let (assertions, errors) = parse_acceptance(doc);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(assertions.len(), 7);
        assert_eq!(assertions[0].kind, "file_exists");
        assert_eq!(assertions[0].group, "R-01");
        assert_eq!(assertions[1].group, "F-01");
        assert_eq!(assertions[1].expected, "def add");
        assert_eq!(assertions[4].kind, "command_output_contains");
        assert_eq!(assertions[4].expected, "hello");
        assert_eq!(assertions[5].kind, "freeze");
        assert_eq!(assertions[5].path, "test_calc.py");
        assert_eq!(assertions[6].group, "A-01");
    }

    #[test]
    fn rejects_unknown_kind_and_missing_block() {
        let (a, e) = parse_acceptance("no fence here");
        assert!(a.is_empty());
        assert!(e[0].contains("没有"));
        let (a, e) = parse_acceptance("```acceptance\nwibble x\n```\n");
        assert!(a.is_empty());
        assert!(e[0].contains("未知断言类型"));
    }

    #[test]
    fn expand_command_substitutes_placeholders() {
        let cmd = expand_command("{python} -m pytest");
        assert!(cmd.ends_with("-m pytest"));
        assert!(!cmd.contains("{python}"));
    }

    #[test]
    fn safe_join_rejects_escape() {
        let root = PathBuf::from("/tmp/proj");
        assert!(safe_join(&root, "a/b.txt").is_some());
        assert!(safe_join(&root, "../etc/passwd").is_none());
    }

    #[tokio::test]
    async fn runs_file_and_command_assertions() {
        let root = tmp("run");
        std::fs::write(root.join("calc.py"), "def add(a, b):\n    return a + b\n").unwrap();
        let doc = "```acceptance\n\
            file_exists calc.py\n\
            file_contains calc.py :: def add\n\
            file_not_exists nope.py\n\
            command_succeeds echo ok\n\
            command_fails false\n\
            command_output_contains echo hello :: hello\n\
            ```\n";
        let (assertions, errors) = parse_acceptance(doc);
        assert!(errors.is_empty(), "{errors:?}");
        let report = run_assertions(&root, &assertions, 30).await;
        assert!(report.all_passed(), "{:#?}", report.results);
        assert!(report.failing_keys().is_empty());
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn failing_assertion_is_reported() {
        let root = tmp("fail");
        let doc = "```acceptance\nfile_exists missing.py\ncommand_succeeds false\n```\n";
        let (assertions, _) = parse_acceptance(doc);
        let report = run_assertions(&root, &assertions, 30).await;
        assert!(!report.all_passed());
        assert_eq!(report.failing_keys().len(), 2);
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn honesty_assertions_run() {
        let root = tmp("honesty");
        std::fs::write(root.join("doc.md"), "# Doc\nsee `calc.py` and test.md\n").unwrap();
        std::fs::write(root.join("thin.md"), "just prose").unwrap();
        let doc = "```acceptance\n\
            doc_length_min doc.md :: 5\n\
            ref_density_min doc.md :: 1\n\
            assert_no_summary_only doc.md\n\
            assert_needs_clarification doc.md\n\
            assert_no_summary_only thin.md\n\
            ```\n";
        let (assertions, _) = parse_acceptance(doc);
        // The fourth (thin.md summary-only) must fail; the rest pass.
        let report = run_assertions(&root, &assertions, 30).await;
        let failed: Vec<&str> = report
            .results
            .iter()
            .filter(|r| !r.ok)
            .map(|r| r.assertion.raw.as_str())
            .collect();
        assert_eq!(failed, vec!["assert_no_summary_only thin.md"]);
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn tool_dry_run_lists_without_executing() {
        let root = tmp("dry");
        let mut reg = ToolRegistry::new();
        register(&mut reg, &root);
        let out = reg
            .execute(
                "verify_acceptance",
                json!({
                    "assertions": "```acceptance\ncommand_succeeds false\n```\n",
                    "dry_run": true,
                }),
            )
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("dry_run"), "{}", out.content);
        assert!(out.content.contains("command_succeeds"), "{}", out.content);
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn tool_requires_a_source() {
        let root = tmp("req");
        let mut reg = ToolRegistry::new();
        register(&mut reg, &root);
        let err = reg
            .execute("verify_acceptance", json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
        std::fs::remove_dir_all(&root).ok();
    }
}

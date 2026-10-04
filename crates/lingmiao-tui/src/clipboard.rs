//! Clipboard **read + write** for the TUI (cli 2026-09-27「鼠标拖动可以选中 UI 上的
//! 文字」· cli 2026-09-28「默认支持 win 和 linux 的复制粘贴」).
//!
//! The TUI **captures the mouse** (for the wheel — §11 / §16.3), so the
//! terminal's own select-and-copy never receives a drag: the app must implement
//! the selection *and* the copy itself. Because 灵妙 runs anywhere — a local
//! xterm, tmux, WSL, macOS — the copy is attempted over two channels:
//!
//! 1. **OSC 52** — `ESC ] 52 ; c ; <base64> BEL`, the terminal-native clipboard
//!    escape. It works in many terminals (and in tmux with `set -g set-clipboard
//!    on`), needs no local tool, and cannot fail loudly — so it is **always**
//!    emitted, even when a helper below also runs.
//! 2. **A platform helper** — the first of `clip.exe` (WSL interop), `pbcopy`
//!    (macOS), `xclip` / `xsel` (X11/Wayland) found on `PATH`.
//!
//! Two measured facts on the dev box (WSL2 + Xvfb `:99`) drive this code — see
//! obs `剪贴板通道实测-OSC52失效-clip.exe须UTF16LE`:
//!
//! * OSC 52 into a plain X11 **xterm is a no-op** (the clipboard keeps its old
//!   value), so a helper is genuinely needed in that environment;
//! * WSL's `clip.exe` decodes its **stdin as ANSI**, so the text must be encoded
//!   **UTF-16LE** — piping UTF-8 round-trips as mojibake (`u8-涓枃`).
//!
//! Both encodings are pinned by unit tests ([`osc52_sequence`], [`utf16le`])
//! rather than by shelling out, so the tests hold on any machine.
//!
//! ## Reading (`ctrl+v` / paste)
//!
//! cli 2026-09-28「默认支持 win 和 linux 的复制粘贴」: A **bracketed paste** is
//! delivered by the *terminal* as text plus `\x1b[200~`/`\x1b[201~` markers (the
//! app just enables the mode — [`crate::run`] emits `EnableBracketedPaste`), so
//! pasting into the box normally needs no clipboard read at all. But a bare
//! `ctrl+v` carries no text, and some terminals do not bracket a paste: both are
//! served by [`paste`], which reads the OS clipboard through the helper family
//! above — Windows/WSL first (`powershell.exe Get-Clipboard`, the *reader* for
//! the same clipboard `clip.exe` writes), then `pbpaste`, `wl-paste`, `xclip`,
//! `xsel`.
//!
//! Both paste paths funnel through [`normalize_paste`], which owns the
//! line-ending contract (cli 2026-09-29「输入框粘贴不好用」): `\r\n` → `\n`, and
//! trailing newlines dropped — the Windows reader adds its own `\r\n` to a bare
//! `Get-Clipboard -Raw`, which used to leave a blank line after every paste (and
//! insert one for an *empty* clipboard).

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;

/// The PowerShell one-liner that reads the Windows/WSL clipboard as text.
///
/// Two measured gotchas pin this exact form (see the module docs):
///
/// * `[Console]::OutputEncoding = UTF8` — otherwise CJK round-trips as mojibake.
/// * **`[Console]::Out.Write(...)`, not a bare `Get-Clipboard -Raw`** — PowerShell
///   appends a line terminator to its pipeline output, so a bare `Get-Clipboard
///   -Raw` hands back `"single line\r\n"` (and `"\r\n"` for an *empty* clipboard).
///   [`App::paste`] turns that `\r\n` into a newline, so every `ctrl+v` used to
///   leave a spurious blank line in the box, and pasting an empty clipboard
///   inserted one. `Out::Write` emits the string verbatim: exact bytes, real
///   trailing newlines kept, nothing invented.
///
/// Kept as a constant so a unit test can pin the shape without spawning Windows.
pub const POWERSHELL_READ_ARGS: &[&str] = &[
    "-NoProfile",
    "-NonInteractive",
    "-Command",
    "[Console]::OutputEncoding=[Text.Encoding]::UTF8; [Console]::Out.Write((Get-Clipboard -Raw))",
];

/// The **OSC 52** escape that sets the clipboard to `text`.
///
/// The payload is base64, so it is pure ASCII and can never contain a control
/// byte that would terminate the sequence early. `c` selects the CLIPBOARD
/// selection (as opposed to `p` / PRIMARY or `s` / SECONDARY).
pub fn osc52_sequence(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", BASE64.encode(text))
}

/// `text` as **UTF-16LE** bytes — the encoding WSL's `clip.exe` expects on
/// stdin (see the module docs). No extra dependency: `encode_utf16` + manual
/// little-endian widening.
pub fn utf16le(text: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len() * 2);
    for unit in text.encode_utf16() {
        out.extend_from_slice(&unit.to_le_bytes());
    }
    out
}

/// Copy `text` to the clipboard, best effort.
///
/// Returns `Ok(())` once at least one channel has been *attempted* without an
/// immediate error — neither OSC 52 nor a helper reports success reliably, so the
/// caller only needs to know whether anything could be done at all.
pub fn copy(text: &str) -> Result<(), String> {
    if text.is_empty() {
        return Err("空选择".to_string());
    }
    // 1. OSC 52 — always emitted (harmless where the terminal ignores it).
    {
        let mut out = std::io::stdout();
        let _ = out.write_all(osc52_sequence(text).as_bytes());
        let _ = out.flush();
    }
    // 2. A local helper, so environments where OSC 52 is a no-op (plain xterm on
    //    X11) still copy.
    platform_copy(text)
}

/// Read the OS clipboard (cli 2026-09-28「默认支持 win 和 linux 的复制粘贴」).
///
/// The terminals' own **bracketed paste** is the primary path (`ctrl+v`/paste
/// arrives as [`crossterm::event::Event::Paste`]); this is the fallback for a
/// bare `ctrl+v` or a terminal that does not bracket — and the only way to paste
/// a clipboard set by an app *outside* the terminal.
///
/// The helpers are tried in the same specificity order as [`copy`], so the two
/// halves agree on which clipboard they touch: Windows/WSL (`clip.exe` is the
/// writer, `powershell.exe Get-Clipboard` the reader — both reach the *host*
/// clipboard, not a second X11 one), then macOS (`pbpaste`), then X11/Wayland
/// (`wl-paste` / `xclip` / `xsel`). The result is *raw* — run it through
/// [`normalize_paste`] before it goes into the input box.
pub fn paste() -> Result<String, String> {
    // Windows / WSL: `Get-Clipboard -Raw` returns the text as-is (newlines kept).
    // The exact command — and why it is not a bare pipeline — is in
    // [`POWERSHELL_READ_ARGS`].
    if which("clip.exe").is_some()
        && let Some(out) = run_capture("powershell.exe", POWERSHELL_READ_ARGS)
    {
        return Ok(out);
    }
    if which("pbpaste").is_some()
        && let Some(out) = run_capture("pbpaste", &[])
    {
        return Ok(out);
    }
    // X11 / Wayland. `wl-paste` first: on a Wayland session `xclip` talks to
    // XWayland and may see a different (stale) clipboard.
    for (cmd, args) in [
        ("wl-paste", &["--no-newline"][..]),
        ("xclip", &["-selection", "clipboard", "-o"][..]),
        ("xsel", &["--clipboard", "--output"][..]),
    ] {
        if which(cmd).is_some()
            && let Some(out) = run_capture(cmd, args)
        {
            return Ok(out);
        }
    }
    Err("未找到本地剪贴板工具".to_string())
}

/// Turn raw clipboard text into what the input box should hold.
///
/// Three normalisations, all measured (cli 2026-09-29「输入框粘贴不好用」):
///
/// * **`\r\n` / `\r` → `\n`** — so a multi-line block edits correctly. Pasting
///   `\r` verbatim used to be rendered as the control character it is.
/// * **Strip trailing newlines** — a clipboard holding `"one line\n"` (the
///   common "I copied the whole line" case) must not leave the cursor on a blank
///   second line, and an *empty* clipboard (which the Windows reader reports as
///   exactly `"\r\n"`) must insert nothing at all rather than one newline.
/// * Nothing else — interior blank lines are the user's text and are kept.
///
/// Pure and terminal-free, so the contract is unit-tested (the Windows helper's
/// output can only be exercised on WSL).
pub fn normalize_paste(text: &str) -> String {
    text.replace("\r\n", "\n")
        .replace('\r', "\n")
        .trim_end_matches('\n')
        .to_string()
}

/// Run `cmd args…` and return its **stdout as UTF-8**, or `None` when it could
/// not be started or exited non-zero (e.g. `xclip` with an empty clipboard).
fn run_capture(cmd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok()
}

/// Try the platform helpers in order of specificity. The first one present on
/// `PATH` is used; its own spawn result is the return value.
fn platform_copy(text: &str) -> Result<(), String> {
    // WSL / Windows interop first: `clip.exe` is unique to this environment and
    // is the one that actually reaches the *host* clipboard under WSL.
    if which("clip.exe").is_some() && spawn_pipe("clip.exe", &[], &utf16le(text)) {
        return Ok(());
    }
    if which("pbcopy").is_some() && spawn_pipe("pbcopy", &[], text.as_bytes()) {
        return Ok(());
    }
    // X11: `xclip` / `xsel` must **stay alive** to own the selection, so they are
    // spawned rather than waited on (waiting would block the caller forever).
    for (cmd, args) in [
        ("xclip", &["-selection", "clipboard", "-in"][..]),
        ("xsel", &["--clipboard", "--input"][..]),
    ] {
        if which(cmd).is_some() && spawn_pipe(cmd, args, text.as_bytes()) {
            return Ok(());
        }
    }
    Err("OSC 52 已发送；未找到本地剪贴板工具".to_string())
}

/// Spawn `cmd args…`, write `bytes` to its stdin, and hand it the pipe. Returns
/// `false` when the process could not be started or its stdin closed early.
fn spawn_pipe(cmd: &str, args: &[&str], bytes: &[u8]) -> bool {
    let Ok(mut child) = Command::new(cmd)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    if let Some(mut stdin) = child.stdin.take()
        && stdin.write_all(bytes).is_err()
    {
        return false;
    }
    // Deliberately **not** waited on: `xclip`/`xsel` own the selection and must
    // keep running. The child is reaped by the OS once it exits on its own —
    // except `clip.exe`, which is short-lived and exits on EOF, so it is reaped
    // here rather than lingering as a zombie for the life of the TUI.
    if cmd == "clip.exe" {
        let _ = child.wait();
    }
    true
}

/// Minimal `PATH` lookup (no extra dependency): the first directory holding an
/// executable-*looking* file named `name`.
fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc52_wraps_base64_in_the_clipboard_escape() {
        let seq = osc52_sequence("hi");
        assert_eq!(seq, "\x1b]52;c;aGk=\x07");
        // The payload is base64 → pure ASCII, no control byte can terminate the
        // sequence early (a `\x07` inside the text would break every terminal).
        let seq = osc52_sequence("a\x07b");
        let body = seq
            .strip_prefix("\x1b]52;c;")
            .and_then(|s| s.strip_suffix('\x07'))
            .expect("the sequence is well formed");
        assert!(body.is_ascii(), "payload must be base64/ASCII: {body:?}");
        assert!(!body.contains('\x07'), "payload must not carry a BEL");
    }

    #[test]
    fn utf16le_encodes_bmp_chars_little_endian() {
        // ASCII: one 16-bit LE unit per char, low byte first.
        assert_eq!(utf16le("AB"), vec![0x41, 0x00, 0x42, 0x00]);
        // CJK (in the BMP): a single unit — `中` = U+4E2D.
        assert_eq!(utf16le("中"), vec![0x2D, 0x4E]);
    }

    #[test]
    fn utf16le_encodes_a_surrogate_pair_for_astral_chars() {
        // `🔧` (U+1F527) is outside the BMP → a UTF-16 **surrogate pair**
        // (D83D DD27), so the output is 4 bytes, not 2. This is exactly what a
        // naive `as u16` cast would get wrong.
        assert_eq!(utf16le("🔧"), vec![0x3D, 0xD8, 0x27, 0xDD]);
    }

    #[test]
    fn empty_text_is_refused_before_touching_the_clipboard() {
        assert!(copy("").is_err());
    }

    #[test]
    fn windows_reader_uses_out_write_not_a_bare_pipeline() {
        // Pin the shape (cli 2026-09-29「输入框粘贴不好用」): the helper must write
        // the string verbatim. A bare `Get-Clipboard -Raw` as the *last command*
        // of the pipeline gets PowerShell's output formatter, which appends
        // `\r\n` — that is what left a blank line after every paste (and made an
        // empty clipboard insert one). Only WSL can execute this, but the
        // contract is checkable anywhere.
        let cmd = POWERSHELL_READ_ARGS.last().copied().unwrap_or_default();
        assert!(
            cmd.contains("[Console]::Out.Write("),
            "the reader must emit the raw string: {cmd:?}"
        );
        assert!(
            cmd.contains("Get-Clipboard -Raw"),
            "…of the raw clipboard: {cmd:?}"
        );
        assert!(
            cmd.contains("OutputEncoding=[Text.Encoding]::UTF8"),
            "CJK must survive the pipe: {cmd:?}"
        );
    }

    #[test]
    fn normalize_paste_owns_the_line_ending_contract() {
        // CRLF / lone CR → LF (a `\r` inserted raw is a control character).
        assert_eq!(normalize_paste("a\r\nb"), "a\nb");
        assert_eq!(normalize_paste("a\rb"), "a\nb");
        // Trailing newlines dropped: "I copied the whole line" must not leave the
        // cursor on a blank second line…
        assert_eq!(normalize_paste("one line\n"), "one line");
        assert_eq!(normalize_paste("one line\r\n"), "one line");
        assert_eq!(normalize_paste("a\nb\n\n"), "a\nb");
        // …and the empty clipboard (which the Windows reader reports as exactly
        // `\r\n`) becomes empty, so it inserts nothing instead of one newline.
        assert_eq!(normalize_paste("\r\n"), "");
        assert_eq!(normalize_paste(""), "");
        // Interior blank lines are the user's text — kept verbatim.
        assert_eq!(normalize_paste("a\n\nb"), "a\n\nb");
        assert_eq!(normalize_paste("a\n\nb\n"), "a\n\nb");
    }

    #[test]
    fn run_capture_returns_none_when_the_helper_is_missing() {
        // A missing helper must degrade quietly (the caller falls through to the
        // next channel), never panic.
        assert_eq!(run_capture("definitely-not-a-cmd-9f3a", &[]), None);
    }

    #[cfg(unix)]
    #[test]
    fn run_capture_returns_stdout_of_a_successful_helper() {
        assert_eq!(
            run_capture("sh", &["-c", "printf 'hi'"]),
            Some("hi".to_string())
        );
        // A failing helper (non-zero exit) yields `None`, so `paste()` can tell
        // "no clipboard tool produced anything" from "it produced empty text".
        assert_eq!(run_capture("sh", &["-c", "exit 3"]), None);
    }
}

//! Image helpers shared by the file tools and the vision pipeline (⑥ vision).
//!
//! `read_file` on an image path returns a **marker JSON** instead of garbage
//! text; the stage agent turns that marker into an OpenAI multipart `image_url`
//! attachment when the active model supports vision. This module is the single
//! source of truth for the three questions involved, so the tool layer and the
//! engine layer cannot drift apart:
//!
//! * *what is an image* — [`is_image_path`] (extension allow-list),
//! * *which mime type* — [`mime_for`],
//! * *how to encode it* — [`to_data_url`], enforcing the system-prompt budget
//!   (单张 ≤ 10 MiB).
//!
//! The marker key [`MARKER_KEY`] (`__lingmiao_image__`) identifies an image
//! marker payload; a marker produced here round-trips through any consumer
//! that understands that shape.

use std::path::Path;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde_json::{Value, json};

use crate::errors::LingmiaoError;

/// Supported image extensions (lower-case, without the dot).
pub const IMAGE_EXTENSIONS: [&str; 6] = ["png", "jpg", "jpeg", "gif", "webp", "bmp"];

/// Single-image byte cap — mirrors the system prompt's 「单张 ≤ 10 MiB」.
pub const MAX_IMAGE_BYTES: u64 = 10 * 1024 * 1024;

/// The marker key identifying an image marker JSON payload.
pub const MARKER_KEY: &str = "__lingmiao_image__";

/// Whether `p` looks like a supported image by extension (case-insensitive).
pub fn is_image_path(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .map(|e| IMAGE_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

/// The MIME type for a path's extension (falls back to a generic binary type).
pub fn mime_for(p: &Path) -> &'static str {
    match p
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("bmp") => "image/bmp",
        _ => "application/octet-stream",
    }
}

/// Read `p` and return a `data:{mime};base64,{…}` URL.
///
/// Errors (never a panic) when the file cannot be read or exceeds
/// [`MAX_IMAGE_BYTES`] — the system prompt promises an explicit error on oversize
/// rather than a silent drop.
pub fn to_data_url(p: &Path) -> Result<String, LingmiaoError> {
    let meta = std::fs::metadata(p)
        .map_err(|e| LingmiaoError::safe(format!("cannot stat image `{}`: {e}", p.display())))?;
    if meta.len() > MAX_IMAGE_BYTES {
        return Err(LingmiaoError::safe(format!(
            "image `{}` is {} bytes, over the {} byte (10 MiB) single-image limit",
            p.display(),
            meta.len(),
            MAX_IMAGE_BYTES
        )));
    }
    let bytes = std::fs::read(p)
        .map_err(|e| LingmiaoError::safe(format!("cannot read image `{}`: {e}", p.display())))?;
    Ok(data_url_for(mime_for(p), &bytes))
}

/// Build a data URL from an already-read byte slice + mime (pure, testable).
pub fn data_url_for(mime: &str, bytes: &[u8]) -> String {
    format!("data:{mime};base64,{}", BASE64.encode(bytes))
}

/// Build the marker JSON a `read_file` returns for an image.
pub fn build_marker(rel_path: &str, mime: &str, size: u64, data_url: &str) -> Value {
    json!({
        MARKER_KEY: true,
        "path": rel_path,
        "mime": mime,
        "size": size,
        "data_url": data_url,
    })
}

/// Detect an image marker in a tool result and return its `(path, data_url)`.
///
/// Returns `None` for any non-marker / non-JSON content, so the stage agent can
/// call it unconditionally on every tool result.
pub fn marker_parts(content: &str) -> Option<(String, String)> {
    let v: Value = serde_json::from_str(content).ok()?;
    if v.get(MARKER_KEY).and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let path = v
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let url = v.get("data_url").and_then(Value::as_str)?.to_string();
    Some((path, url))
}

/// Return the marker JSON with its (huge) base64 `data_url` **replaced by a
/// short note**, or the input unchanged when it is not a marker.
///
/// cli 2026-09-28「图片 base64 双份注入」: a vision model receives the pixels
/// once as a multipart `image_url` attachment, but the tool-result *text* is the
/// raw marker JSON — so the megabytes of base64 ride along a second time as a
/// string, and the C-stage tool loop re-sends the whole message list on every
/// round-trip (which is how `prompt_tokens` ballooned to 31.5M). Feeding the
/// model a **compact** marker keeps the pixel data solely in the attachment.
///
/// The note keeps the byte count so the model still knows an image was read and
/// how large it was; a non-marker string is returned verbatim.
pub fn strip_data_url(content: &str) -> String {
    let Ok(v) = serde_json::from_str::<Value>(content) else {
        return content.to_string();
    };
    if v.get(MARKER_KEY).and_then(Value::as_bool) != Some(true) {
        return content.to_string();
    }
    let Some(obj) = v.as_object() else {
        return content.to_string();
    };
    let n = obj
        .get("data_url")
        .and_then(Value::as_str)
        .map(|s| s.len())
        .unwrap_or(0);
    let mut out = obj.clone();
    out.insert(
        "data_url".into(),
        json!(format!("(base64 已省略 {n} 字节 · 见多模态附件)")),
    );
    serde_json::to_string(&Value::Object(out)).unwrap_or_else(|_| content.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_supported_extensions_case_insensitively() {
        for name in ["a.png", "b.JPG", "c.jpeg", "d.Gif", "e.webp", "f.bmp"] {
            assert!(is_image_path(Path::new(name)), "{name} should be an image");
        }
        for name in ["a.txt", "b.rs", "c.svg", "noext"] {
            assert!(!is_image_path(Path::new(name)), "{name} is not an image");
        }
    }

    #[test]
    fn mime_matches_extension() {
        assert_eq!(mime_for(Path::new("x.png")), "image/png");
        assert_eq!(mime_for(Path::new("x.JPG")), "image/jpeg");
        assert_eq!(mime_for(Path::new("x.jpeg")), "image/jpeg");
        assert_eq!(mime_for(Path::new("x.gif")), "image/gif");
        assert_eq!(mime_for(Path::new("x.webp")), "image/webp");
        assert_eq!(mime_for(Path::new("x.bmp")), "image/bmp");
    }

    #[test]
    fn data_url_has_correct_prefix_and_round_trips() {
        // 4 bytes 0x00..0x03 → base64 "AAECAw==".
        assert_eq!(
            data_url_for("image/png", &[0, 1, 2, 3]),
            "data:image/png;base64,AAECAw=="
        );
    }

    #[test]
    fn to_data_url_reads_a_real_file() {
        let dir = std::env::temp_dir().join(format!("lingmiao-img-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("dot.png");
        std::fs::write(&p, [0u8, 1, 2, 3]).unwrap();
        let url = to_data_url(&p).unwrap();
        assert_eq!(url, "data:image/png;base64,AAECAw==");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn to_data_url_rejects_oversize() {
        let dir = std::env::temp_dir().join(format!("lingmiao-img-big-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("big.png");
        let f = std::fs::File::create(&p).unwrap();
        f.set_len(MAX_IMAGE_BYTES + 1).unwrap();
        drop(f);
        let err = to_data_url(&p).unwrap_err();
        assert!(
            err.message().contains("single-image limit"),
            "{}",
            err.message()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn strip_data_url_removes_base64_but_keeps_the_path() {
        // cli 2026-09-28「图片 base64 双份注入」: the text copy of a marker must
        // not carry the base64 (that rode along with the multipart attachment and
        // inflated prompt_tokens across the tool loop).
        let m = build_marker(
            "shots/a.png",
            "image/png",
            4,
            "data:image/png;base64,AAECAw==",
        );
        let text = serde_json::to_string(&m).unwrap();
        let stripped = strip_data_url(&text);
        assert!(!stripped.contains("AAECAw=="), "base64 gone: {stripped}");
        assert!(stripped.contains("shots/a.png"), "path kept: {stripped}");
        assert!(stripped.contains("image/png"), "mime kept: {stripped}");
        // Still parses as a marker (with a data_url note) — consumers can rely on
        // `marker_parts` shape, though the URL is now a placeholder string.
        let v: Value = serde_json::from_str(&stripped).unwrap();
        assert_eq!(v[MARKER_KEY], json!(true));
        // Non-marker text is returned verbatim (no accidental mangling).
        assert_eq!(strip_data_url("plain text"), "plain text");
        assert_eq!(strip_data_url(r#"{"foo":1}"#), r#"{"foo":1}"#);
    }

    #[test]
    fn marker_round_trips_through_parts() {
        let m = build_marker(
            "shots/a.png",
            "image/png",
            4,
            "data:image/png;base64,AAECAw==",
        );
        let text = serde_json::to_string(&m).unwrap();
        assert_eq!(m[MARKER_KEY], json!(true));
        let (path, url) = marker_parts(&text).expect("marker detected");
        assert_eq!(path, "shots/a.png");
        assert_eq!(url, "data:image/png;base64,AAECAw==");
        // Ordinary tool text is not a marker.
        assert!(marker_parts("plain text result").is_none());
        assert!(marker_parts(r#"{"foo":1}"#).is_none());
    }
}

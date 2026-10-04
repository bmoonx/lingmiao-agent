//! Hand-rolled Server-Sent Events (SSE) decoding — Q5 decision.
//!
//! The Python original relied on the `openai` SDK's streaming iterator. Here we
//! own the parser: [`SseDecoder`] accepts raw HTTP body chunks (which may split
//! SSE lines at arbitrary *byte* boundaries) and yields the `data:` payloads,
//! decoded as UTF-8 only once a full line is available so multi-byte characters
//! are never torn.

/// Incremental SSE `data:` line decoder.
#[derive(Debug, Default)]
pub struct SseDecoder {
    buf: Vec<u8>,
}

impl SseDecoder {
    /// Feed a chunk of the HTTP body; return every complete `data:` payload.
    ///
    /// Only the `data:` field is surfaced (event / id / comment lines and blank
    /// separators are ignored) — that is all the OpenAI-compatible protocol uses.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(nl) = self.buf.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = self.buf.drain(..=nl).collect();
            line.pop(); // drop the trailing '\n'
            let line = String::from_utf8_lossy(&line);
            let line = line.trim_end_matches('\r');
            if let Some(rest) = line.strip_prefix("data:") {
                out.push(rest.trim_start().to_string());
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_data_lines() {
        let mut d = SseDecoder::default();
        let got = d.push(b"data: {\"a\":1}\n\ndata: [DONE]\n\n");
        assert_eq!(got, vec!["{\"a\":1}".to_string(), "[DONE]".to_string()]);
    }

    #[test]
    fn reassembles_split_across_chunks() {
        let mut d = SseDecoder::default();
        assert!(d.push(b"data: {\"a\"").is_empty());
        assert!(d.push(b":1}").is_empty());
        assert_eq!(d.push(b"\n"), vec!["{\"a\":1}".to_string()]);
    }

    #[test]
    fn multibyte_split_across_bytes_is_not_torn() {
        let mut d = SseDecoder::default();
        let payload = "data: 你好\n".as_bytes().to_vec();
        let (a, b) = payload.split_at(8); // splits inside the 3-byte 好
        assert!(d.push(a).is_empty());
        assert_eq!(d.push(b), vec!["你好".to_string()]);
    }
}

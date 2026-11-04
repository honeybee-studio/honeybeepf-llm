//! One HTTP/2 stream matched to a provider: its response body gathered for usage.

use std::time::Instant;

use honeybeepf_llm_core::types::UsageInfo;

use super::super::body::parse_response_json;
use crate::probes::builtin::llm::processor::MAX_RESPONSE_BUFFER_SIZE;

pub(super) struct H2Stream {
    /// The request head's time, so latency is measured to the response's END_STREAM.
    start: Instant,
    buf: Vec<u8>,
    /// The body grew past the response cap, so it is not parsed.
    too_large: bool,
}

impl H2Stream {
    pub(super) fn open(start: Instant) -> Self {
        Self {
            start,
            buf: Vec::new(),
            too_large: false,
        }
    }

    pub(super) fn start(&self) -> Instant {
        self.start
    }

    /// Gather response DATA, dropping it once it would pass the cap.
    pub(super) fn data(&mut self, bytes: &[u8]) {
        if self.too_large {
            return;
        }
        if self.buf.len() + bytes.len() > MAX_RESPONSE_BUFFER_SIZE {
            self.too_large = true;
            self.buf = Vec::new();
            return;
        }
        self.buf.extend_from_slice(bytes);
    }

    /// The usage once the response has ended. Only a JSON object body is read; SSE, a content-encoded
    /// blob or a JSON array does not start with `{`, so it is not parsed.
    pub(super) fn finish(&self) -> Option<UsageInfo> {
        if self.too_large {
            return None;
        }
        let text = String::from_utf8_lossy(&self.buf);
        let json = text.trim_start();
        if !json.starts_with('{') {
            return None;
        }
        parse_response_json(json)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finish(body: &[u8]) -> Option<(u64, u64)> {
        let mut s = H2Stream::open(Instant::now());
        s.data(body);
        s.finish().map(|u| (u.prompt_tokens, u.completion_tokens))
    }

    /// Only a JSON object body is read. An SSE stream, a content-encoded blob or a JSON array does
    /// not begin with `{`, so none of them is read as JSON.
    #[test]
    fn only_a_json_object_body_is_parsed() {
        let usage = br#"{"model":"gpt-4o","usage":{"prompt_tokens":11,"completion_tokens":4}}"#;
        assert_eq!(finish(usage), Some((11, 4)));
        // A single-event SSE body is still JSON after `data: `, but must not be read as one here.
        assert_eq!(
            finish(b"data: {\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1}}\n\n"),
            None
        );
        assert_eq!(finish(b"[{\"usageMetadata\":{}}]"), None);
        assert_eq!(finish(b"\x1f\x8b\x08 gzip-looking bytes {}"), None);
    }
}

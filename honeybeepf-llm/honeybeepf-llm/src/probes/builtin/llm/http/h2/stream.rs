//! One HTTP/2 stream matched to a provider: its response body read for usage, by content type.

use std::borrow::Cow;
use std::time::Instant;

use honeybeepf_llm_core::byte_utils::contains_pattern;
use honeybeepf_llm_core::types::UsageInfo;

use super::super::body::parse_response_json;
use super::super::encoding;
use super::super::sse::{SseCursor, parse_sse_body};
use crate::probes::builtin::llm::processor::MAX_RESPONSE_BUFFER_SIZE;

/// How a stream's response body is read, chosen from its content type and coding.
enum Body {
    /// Unencoded SSE, read event by event so a long stream is not kept (reuses the HTTP/1.1 cursor).
    Streaming(SseCursor),
    /// Everything else: gathered to the response's end, then parsed. `coding` empty means no content
    /// coding; `sse` marks an encoded SSE body, so the decoded bytes go to the SSE parser.
    Buffered {
        coding: Vec<u8>,
        sse: bool,
        buf: Vec<u8>,
        too_large: bool,
    },
}

impl Body {
    fn plain_json() -> Self {
        Body::Buffered {
            coding: Vec::new(),
            sse: false,
            buf: Vec::new(),
            too_large: false,
        }
    }

    /// A body given up for passing a memory cap; it holds nothing and records nothing.
    fn dropped() -> Self {
        Body::Buffered {
            coding: Vec::new(),
            sse: false,
            buf: Vec::new(),
            too_large: true,
        }
    }
}

pub(super) struct H2Stream {
    /// The request head's time, so latency is measured to the response's END_STREAM.
    start: Instant,
    /// The last frame this stream saw, so the connection can drop one left idle.
    last: Instant,
    body: Body,
    /// Response DATA has begun, so a later HEADERS block is trailers and must not reset the body.
    got_data: bool,
    /// Tracked without a matched request (mid-join or lost request headers), so its host is unknown.
    speculative: bool,
    /// A record-only provider (Managed Agents session): counted under `agent_session` at its end,
    /// its running session total never read.
    record_only: bool,
}

impl H2Stream {
    /// A stream matched to a provider by its request headers.
    pub(super) fn open(start: Instant, record_only: bool) -> Self {
        Self {
            start,
            last: start,
            body: Body::plain_json(),
            got_data: false,
            speculative: false,
            record_only,
        }
    }

    /// A stream tracked on its response alone, its request headers never matched; the host is unknown.
    pub(super) fn open_speculative(start: Instant) -> Self {
        Self {
            speculative: true,
            ..Self::open(start, false)
        }
    }

    /// Whether the matched provider is record-only, so its end is counted without reading usage.
    pub(super) fn record_only(&self) -> bool {
        self.record_only
    }

    pub(super) fn start(&self) -> Instant {
        self.start
    }

    /// Whether this stream was tracked speculatively (no matched request), so its host is unknown.
    pub(super) fn speculative(&self) -> bool {
        self.speculative
    }

    pub(super) fn last(&self) -> Instant {
        self.last
    }

    /// Whether any response DATA has arrived, so an idle sweep can tell a stalled response from a slow start.
    pub(super) fn started(&self) -> bool {
        self.got_data
    }

    /// Whether the body was given up for a memory cap, so its end counts as a too-large response.
    pub(super) fn over_limit(&self) -> bool {
        matches!(
            &self.body,
            Body::Buffered {
                too_large: true,
                ..
            }
        )
    }

    /// Response body bytes this stream currently holds, returned to the connection budget when it ends.
    pub(super) fn held(&self) -> usize {
        match &self.body {
            Body::Buffered { buf, .. } => buf.len(),
            Body::Streaming(_) => 0,
        }
    }

    /// Pick how to read the body from the response's content type and coding. A HEADERS block after DATA
    /// is trailers (the frame reader cannot tell it apart), so it is ignored rather than resetting the body.
    pub(super) fn response_headers(&mut self, content_type: Option<&[u8]>, coding: &[u8]) {
        self.last = Instant::now();
        if self.got_data {
            return;
        }
        let sse = content_type.is_some_and(|c| contains_pattern(c, b"text/event-stream"));
        self.body = if !coding.is_empty() {
            Body::Buffered {
                coding: coding.to_vec(),
                sse,
                buf: Vec::new(),
                too_large: false,
            }
        } else if sse {
            Body::Streaming(SseCursor::default())
        } else {
            Body::plain_json()
        };
    }

    /// Takes response DATA. `room` is the connection's remaining buffered-byte budget; the returned change
    /// in held bytes keeps that budget. SSE is read as it streams and never counts against the budget.
    pub(super) fn data(&mut self, bytes: &[u8], room: usize) -> isize {
        self.last = Instant::now();
        self.got_data = true;
        match &mut self.body {
            Body::Streaming(cursor) => {
                cursor.feed(bytes);
                if cursor.partial_len() <= MAX_RESPONSE_BUFFER_SIZE {
                    return 0;
                }
            }
            Body::Buffered { buf, too_large, .. } => {
                if *too_large {
                    return 0;
                }
                if bytes.len() > room || buf.len() + bytes.len() > MAX_RESPONSE_BUFFER_SIZE {
                    let freed = buf.len() as isize;
                    (*too_large, *buf) = (true, Vec::new());
                    return -freed;
                }
                buf.extend_from_slice(bytes);
                return bytes.len() as isize;
            }
        }
        // An SSE line outgrew the buffer limit: drop the cursor so it cannot grow further.
        self.body = Body::dropped();
        0
    }

    /// The usage once the response has ended. A body that is not a JSON object (bad coding, a JSON array,
    /// a body given up for a cap) records nothing.
    pub(super) fn finish(&self) -> Option<UsageInfo> {
        match &self.body {
            Body::Streaming(cursor) => cursor.usage(),
            Body::Buffered {
                coding,
                sse,
                buf,
                too_large,
            } => {
                if *too_large {
                    return None;
                }
                let body = if coding.is_empty() {
                    Cow::Borrowed(buf.as_slice())
                } else {
                    let headers = [httparse::Header {
                        name: "content-encoding",
                        value: coding,
                    }];
                    encoding::decode(Cow::Borrowed(buf.as_slice()), &headers)?
                };
                if *sse {
                    return parse_sse_body(&body);
                }
                let text = String::from_utf8_lossy(&body);
                let json = text.trim_start();
                // Background Responses polling is HTTP/1.1 only (see PR notes); drop the state here.
                json.starts_with('{')
                    .then(|| parse_response_json(json, &mut None))
                    .flatten()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    const SSE: &[u8] = b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
        data: {\"model\":\"gpt-4o\",\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":4}}\n\n\
        data: [DONE]\n\n";
    const JSON: &[u8] =
        b"{\"model\":\"gpt-4o\",\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":4}}";

    fn usage(content_type: Option<&[u8]>, coding: &[u8], body: &[u8]) -> Option<(u64, u64)> {
        let mut s = H2Stream::open(Instant::now(), false);
        s.response_headers(content_type, coding);
        s.data(body, usize::MAX);
        s.finish().map(|u| (u.prompt_tokens, u.completion_tokens))
    }

    #[test]
    fn plain_sse_is_read_as_events() {
        assert_eq!(usage(Some(b"text/event-stream"), b"", SSE), Some((11, 4)));
    }

    #[test]
    fn gzip_body_is_decoded_then_parsed() {
        let gz = |data: &[u8]| {
            let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
            enc.write_all(data).expect("gzip");
            enc.finish().expect("gzip")
        };
        assert_eq!(
            usage(Some(b"application/json"), b"gzip", &gz(JSON)),
            Some((11, 4))
        );
        // Same coding, SSE content type: the decoded bytes go to the SSE parser.
        assert_eq!(
            usage(Some(b"text/event-stream"), b"gzip", &gz(SSE)),
            Some((11, 4))
        );
    }

    #[test]
    fn plain_json_records_but_a_bad_coding_or_sse_bytes_as_json_do_not() {
        assert_eq!(usage(Some(b"application/json"), b"", JSON), Some((11, 4)));
        // An SSE body seen without the event-stream content type is not a JSON object.
        assert_eq!(usage(Some(b"application/json"), b"", SSE), None);
        // An unknown content coding cannot be decoded.
        assert_eq!(usage(Some(b"application/json"), b"br", JSON), None);
    }

    #[test]
    fn a_later_headers_block_does_not_reset_the_body() {
        // Trailers (HEADERS after DATA): the body kind and buffer must survive so usage is still read.
        let mut s = H2Stream::open(Instant::now(), false);
        s.response_headers(Some(b"application/json"), b"");
        s.data(JSON, usize::MAX);
        s.response_headers(None, b"");
        assert_eq!(
            s.finish().map(|u| (u.prompt_tokens, u.completion_tokens)),
            Some((11, 4))
        );
    }

    #[test]
    fn an_sse_line_without_a_newline_is_bounded() {
        let mut s = H2Stream::open(Instant::now(), false);
        s.response_headers(Some(b"text/event-stream"), b"");
        let chunk = vec![b'a'; 16 * 1024];
        for _ in 0..(MAX_RESPONSE_BUFFER_SIZE / chunk.len() + 2) {
            s.data(&chunk, usize::MAX);
        }
        // The cursor was dropped once its unfinished line passed the cap, so it holds nothing.
        assert_eq!(s.held(), 0);
        assert_eq!(s.finish(), None);
    }
}

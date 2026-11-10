//! HTTP/1.1 request detection and response parsing: framing, decoding, SSE or JSON usage.

use honeybeepf_llm_core::byte_utils;
use honeybeepf_llm_core::chunked::{ChunkRules, ChunkState, ChunkWalk};
use honeybeepf_llm_core::types::UsageInfo;

use super::body::{BackgroundState, extract_text_from_json, is_response_id, parse_response_json};
use super::encoding;
use super::eventstream::{EventStreamCursor, parse_event_stream_body};
use super::registry::{PROVIDER_REGISTRY, detect_by_shape, is_llm_path};
use super::sse::{SseCursor, parse_sse_body};
use super::{Detected, ProtocolParser};
use crate::probes::builtin::llm::processor::MAX_RESPONSE_BUFFER_SIZE;

/// What earlier reads of one response already decoded, so each read costs only its new bytes.
#[derive(Default)]
pub struct ResponseProgress {
    chunks: Dechunker,
    sse: SseCursor,
    events: EventStreamCursor,
    background: Option<BackgroundState>,
}

impl ResponseProgress {
    /// Where the background call this response belongs to stands, once parsing has seen it.
    pub fn take_background(&mut self) -> Option<BackgroundState> {
        self.background.take()
    }
}

#[cfg(test)]
impl ResponseProgress {
    /// Bytes of an unfinished SSE line held for the next read.
    pub fn held_line_len(&self) -> usize {
        self.sse.partial_len()
    }
}

/// Chunk decoding resumed across reads; passes on the bytes `byte_utils::decode_chunked_body` would return.
#[derive(Default)]
struct Dechunker {
    /// Body offset of the next byte the walk has not used.
    pos: usize,
    walk: ChunkWalk,
    /// Not chunked, or stopped before any data, which decode_chunked_body returns unchanged.
    raw: bool,
    decoded_any: bool,
}

impl Dechunker {
    fn advance(&mut self, body: &[u8], chunked: bool, mut sink: impl FnMut(&[u8])) {
        self.raw |= !chunked;
        if !self.stopped() {
            let decoded_any = &mut self.decoded_any;
            let rest = body.get(self.pos..).unwrap_or_default();
            self.pos += self.walk.walk(rest, |data| {
                *decoded_any = true;
                sink(data);
            });
            if self.stopped() && !self.decoded_any {
                self.raw = true;
                self.pos = 0;
            }
        }
        if self.raw {
            sink(body.get(self.pos..).unwrap_or_default());
            self.pos = body.len();
        }
    }

    /// Whether no further size line will be read, so only the full walk can tell if the body ended.
    fn stopped(&self) -> bool {
        self.raw
            || matches!(
                self.walk.state(),
                ChunkState::Trailer | ChunkState::Ended | ChunkState::Broken
            )
    }
}

#[derive(Clone)]
pub struct Http11Parser;

impl Http11Parser {
    /// Whether the buffer starts with a complete HTTP/1.1 request head.
    pub fn is_request_head(buffer: &[u8]) -> bool {
        let mut headers = [httparse::EMPTY_HEADER; 64];
        let mut req = httparse::Request::new(&mut headers);
        matches!(req.parse(buffer), Ok(httparse::Status::Complete(_)))
    }

    /// The id in a `GET <provider path>/{id}` request head: how a background call's result is fetched.
    pub fn retrieved_response_id(buffer: &[u8]) -> Option<&str> {
        let mut headers = [httparse::EMPTY_HEADER; 64];
        let mut req = httparse::Request::new(&mut headers);
        req.parse(buffer).ok()?;
        if req.method != Some("GET") {
            return None;
        }
        let path = req.path?;
        let (base, id) = path
            .split_once('?')
            .map_or(path, |(p, _)| p)
            .rsplit_once('/')?;
        let host = req
            .headers
            .iter()
            .find(|h| h.name.eq_ignore_ascii_case("Host"))
            .and_then(|h| std::str::from_utf8(h.value).ok())?;
        (is_response_id(id) && PROVIDER_REGISTRY.find_provider(host, base).is_some()).then_some(id)
    }

    /// Whether the buffer starts with a complete request head whose Host no provider serves.
    pub fn has_foreign_host(buffer: &[u8]) -> bool {
        let mut headers = [httparse::EMPTY_HEADER; 64];
        let mut req = httparse::Request::new(&mut headers);
        if !matches!(req.parse(buffer), Ok(httparse::Status::Complete(_))) {
            return false;
        }
        // Shape detection: keep an unknown-host connection whose request path is an LLM pattern.
        if detect_by_shape() && req.path.is_some_and(is_llm_path) {
            return false;
        }
        req.headers
            .iter()
            .find(|h| h.name.eq_ignore_ascii_case("Host"))
            .and_then(|h| std::str::from_utf8(h.value).ok())
            .is_some_and(|host| !PROVIDER_REGISTRY.may_serve_host(host))
    }

    /// Whether the buffer holds a whole HTTP/1.1 response by its framing, after any 1xx interim responses.
    pub fn is_complete_response(buffer: &[u8]) -> bool {
        final_head(buffer).is_some_and(|head| head.framing.ended(head.body))
    }

    /// Whether a complete 2xx response framed by Content-Length is buffered. Only Content-Length is
    /// counted: a chunked body can reach its terminator with broken framing (capture loss), which is not
    /// a clean "provider returned no usage". SSE and event streams are read as they stream, not here.
    pub fn is_complete_success_response(buffer: &[u8]) -> bool {
        final_head(buffer).is_some_and(|head| {
            (200..300).contains(&head.status)
                && !head.is_sse
                && !head.is_eventstream
                && matches!(head.framing, Framing::Length(len) if head.body.len() >= len)
        })
    }

    /// Whether the buffer starts with a final response head that has headers; body text that merely starts with
    /// `HTTP/1.` parses with none, since SSE and JSON bodies have raw newlines only between events.
    pub fn starts_with_response_head(buffer: &[u8]) -> bool {
        final_head(buffer).is_some_and(|head| head.header_count > 0)
    }
}

/// How the end of a response body is found.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Framing {
    Length(usize),
    Chunked,
    UntilClose,
}

impl Framing {
    /// Whether the body holds all the framing announces.
    fn ended(self, body: &[u8]) -> bool {
        match self {
            Framing::Length(len) => body.len() >= len,
            Framing::Chunked => chunked_ended(body),
            Framing::UntilClose => false,
        }
    }
}

/// The final response head with its framing and stream flags.
struct Head<'b> {
    header_slots: [httparse::Header<'b>; 64],
    header_count: usize,
    body: &'b [u8],
    framing: Framing,
    is_sse: bool,
    /// AWS binary event stream (Bedrock `*-stream` routes).
    is_eventstream: bool,
    status: u16,
}

impl<'b> Head<'b> {
    fn headers(&self) -> &[httparse::Header<'b>] {
        &self.header_slots[..self.header_count]
    }
}

/// Parse the final response head once, past any complete 1xx interim responses.
fn final_head(buffer: &[u8]) -> Option<Head<'_>> {
    let mut rest = buffer;
    loop {
        let mut headers = [httparse::EMPTY_HEADER; 64];
        let mut resp = httparse::Response::new(&mut headers);
        let Ok(httparse::Status::Complete(n)) = resp.parse(rest) else {
            return None;
        };
        // `100 Continue` (large uploads) and `103 Early Hints` carry none of the final response's framing.
        if resp.code.is_some_and(|c| (100..200).contains(&c)) {
            rest = &rest[n..];
            continue;
        }
        // Transfer-Encoding overrides Content-Length (RFC 9112 §6.3).
        let framing = if header_contains(resp.headers, "Transfer-Encoding", b"chunked") {
            Framing::Chunked
        } else {
            content_length(resp.headers).map_or(Framing::UntilClose, Framing::Length)
        };
        let is_sse = header_contains(resp.headers, "Content-Type", b"text/event-stream");
        let is_eventstream = header_contains(
            resp.headers,
            "Content-Type",
            b"application/vnd.amazon.eventstream",
        );
        // httparse fills the array from the front and shrinks `resp.headers` to the parsed ones.
        let header_count = resp.headers.len();
        let status = resp.code.unwrap_or(0);
        return Some(Head {
            header_slots: headers,
            header_count,
            body: &rest[n..],
            framing,
            is_sse,
            is_eventstream,
            status,
        });
    }
}

/// Framing and body of a final response sent as is: not a stream and not content-encoded.
pub(super) fn plain_body(buffer: &[u8]) -> Option<(Framing, &[u8])> {
    let head = final_head(buffer)?;
    (!head.is_sse && !head.is_eventstream && !encoding::is_encoded(head.headers()))
        .then_some((head.framing, head.body))
}

/// Framing and body of a final SSE response sent without a content coding.
pub(super) fn plain_sse_body(buffer: &[u8]) -> Option<(Framing, &[u8])> {
    let head = final_head(buffer)?;
    (head.is_sse && !encoding::is_encoded(head.headers())).then_some((head.framing, head.body))
}

/// Framing, body and the single content coding of an SSE response sent under one content coding.
type EncodedSse<'a> = (Framing, &'a [u8], Box<[u8]>);

/// Framing, body and the one content coding of a final SSE response under a single content coding.
pub(super) fn encoded_sse_body(buffer: &[u8]) -> Option<EncodedSse<'_>> {
    let head = final_head(buffer)?;
    let coding = head
        .is_sse
        .then(|| encoding::single_coding(head.headers()))
        .flatten()?;
    Some((head.framing, head.body, coding))
}

fn header_contains(headers: &[httparse::Header], name: &str, pattern: &[u8]) -> bool {
    headers.iter().any(|h| {
        h.name.eq_ignore_ascii_case(name) && byte_utils::contains_pattern(h.value, pattern)
    })
}

impl ProtocolParser for Http11Parser {
    fn detect_request(&self, buffer: &[u8]) -> Option<Detected> {
        let mut headers = [httparse::EMPTY_HEADER; 64];
        let mut req = httparse::Request::new(&mut headers);

        // Generation calls are POST; GET/DELETE on the same paths read or delete stored objects.
        if req.parse(buffer).is_ok()
            && req.method == Some("POST")
            && let Some(path) = req.path
        {
            let host_header = req
                .headers
                .iter()
                .find(|h| h.name.eq_ignore_ascii_case("Host"))
                .map(|h| String::from_utf8_lossy(h.value).to_string())
                .unwrap_or_default();

            // Host+path matches a configured provider; with shape detection on, fall back to a
            // path-only match so an LLM call to a host no entry lists is still tracked.
            let provider = PROVIDER_REGISTRY
                .find_provider(&host_header, path)
                .or_else(|| {
                    detect_by_shape()
                        .then(|| PROVIDER_REGISTRY.find_provider_by_path(path))
                        .flatten()
                });
            if let Some(provider) = provider {
                // The path is logged and the query can carry a credential (Gemini `?key=`).
                return Some(Detected {
                    path: path.split_once('?').map_or(path, |(p, _)| p).to_string(),
                    record_only: provider.record_only,
                });
            }
        }
        None
    }

    fn extract_request_text(&self, buffer: &[u8]) -> String {
        let body_start = byte_utils::find_pattern(buffer, b"\r\n\r\n").map_or(0, |i| i + 4);

        let json_body = String::from_utf8_lossy(&buffer[body_start..]);
        extract_text_from_json(&json_body)
    }

    fn parse_response(&self, buffer: &[u8]) -> Option<UsageInfo> {
        self.parse_response_from(buffer, &mut ResponseProgress::default())
    }

    fn parse_response_from(
        &self,
        buffer: &[u8],
        progress: &mut ResponseProgress,
    ) -> Option<UsageInfo> {
        let head = final_head(buffer)?;
        let body = head.body;
        let is_chunked = head.framing == Framing::Chunked;
        let is_encoded = encoding::is_encoded(head.headers());

        // SSE events can straddle chunk boundaries, so they are read only after dechunking.
        // JSON bodies are walked too: where the walk stops tells `ended` when the full check is worth it.
        let ResponseProgress {
            chunks,
            sse,
            events,
            background,
        } = progress;
        let feed_sse = head.is_sse && !is_encoded;
        let feed_events = head.is_eventstream && !is_encoded;
        chunks.advance(body, is_chunked, |data| {
            if feed_sse {
                sse.feed(data);
            } else if feed_events {
                events.feed(data);
            }
        });
        // CRC-checked messages cannot be found again in raw bytes once the chunk framing broke.
        if feed_events {
            return events.usage();
        }

        // A close-delimited body has no end to wait for, so it is decoded as it stands.
        // A chunked body can only have ended once the walk stopped or it ends in the terminator.
        let ended = || match head.framing {
            Framing::Chunked => {
                (chunks.stopped() || body.ends_with(b"0\r\n\r\n")) && chunked_ended(body)
            }
            framing => framing == Framing::UntilClose || framing.ended(body),
        };

        if feed_sse {
            // A truncated read desyncs the chunk framing; once the stream has ended, fall back to raw lines.
            let usage = sse.usage().or_else(|| {
                if is_chunked && ended() {
                    parse_sse_body(body)
                } else {
                    None
                }
            });
            return stream_end(usage, background);
        }

        // Decoding is not incremental, so wait for the framed end.
        if is_encoded && !ended() {
            return None;
        }

        let dechunked = if is_chunked {
            byte_utils::decode_chunked_body(body)
        } else {
            std::borrow::Cow::Borrowed(body)
        };
        let decompressed = encoding::decode(dechunked, head.headers())?;

        if head.is_sse {
            return stream_end(parse_sse_body(&decompressed), background);
        }
        if head.is_eventstream {
            return parse_event_stream_body(&decompressed);
        }

        let trimmed = byte_utils::trim_trailing_whitespace(&decompressed);

        // Streamed JSON arrays (Gemini streamGenerateContent without alt=sse): usage is in the last
        // element, read the same way as past the buffer limit.
        let is_json_array = trimmed.iter().find(|b| !b.is_ascii_whitespace()) == Some(&b'[');
        if is_json_array {
            return super::json_ends::array_tail_usage(trimmed);
        }
        if !trimmed.ends_with(b"}") {
            return None;
        }

        let s = String::from_utf8_lossy(trimmed);
        let start = s.find('{')?;
        let json_body = &s[start..];

        parse_response_json(json_body, background)
    }
}

/// Responses streams carry usage only in their terminal event, so a stream with usage has ended.
fn stream_end(
    usage: Option<UsageInfo>,
    background: &mut Option<BackgroundState>,
) -> Option<UsageInfo> {
    if usage.is_some() {
        *background = Some(BackgroundState::Ended);
    }
    usage
}

/// Whether a chunked body holds its last chunk and trailer section, found by following the chunk sizes.
fn chunked_ended(body: &[u8]) -> bool {
    let mut walk = ChunkWalk::new(ENDED_RULES);
    walk.walk(body, |_| {});
    match walk.state() {
        ChunkState::Ended => true,
        // Bytes lost in capture break the framing; then only the terminator bytes are left to go by.
        ChunkState::Broken => body.ends_with(b"0\r\n\r\n"),
        _ => false,
    }
}

/// Chunk rules for `chunked_ended`: RFC 9112 extensions and trailers, CRLF after data checked.
const ENDED_RULES: ChunkRules = ChunkRules {
    extensions: true,
    utf8_line: false,
    data_crlf: true,
    // A chunk the response buffer can never hold is a garbled size line, not one still arriving.
    max_size: MAX_RESPONSE_BUFFER_SIZE,
    bare_lf_breaks: false,
    max_line: usize::MAX,
};

fn content_length(headers: &[httparse::Header]) -> Option<usize> {
    headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case("Content-Length"))
        .and_then(|h| std::str::from_utf8(h.value).ok())
        .and_then(|v| v.trim().parse::<usize>().ok())
}

#[cfg(test)]
mod tests;

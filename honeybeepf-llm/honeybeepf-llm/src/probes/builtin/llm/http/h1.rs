//! HTTP/1.1 request detection and response parsing: framing, decoding, SSE or JSON usage.

use honeybeepf_llm_core::byte_utils;
use honeybeepf_llm_core::chunked::{ChunkRules, ChunkState, ChunkWalk};
use honeybeepf_llm_core::types::UsageInfo;

use super::ProtocolParser;
use super::body::{extract_text_from_json, parse_response_json};
use super::encoding;
use super::registry::PROVIDER_REGISTRY;
use super::sse::{SseCursor, parse_sse_body};
use crate::probes::builtin::llm::processor::MAX_RESPONSE_BUFFER_SIZE;

/// What earlier reads of one response already decoded, so each read costs only its new bytes.
#[derive(Default)]
pub struct ResponseProgress {
    chunks: Dechunker,
    sse: SseCursor,
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

    /// Whether the buffer starts with a complete request head whose Host no provider serves.
    pub fn has_foreign_host(buffer: &[u8]) -> bool {
        let mut headers = [httparse::EMPTY_HEADER; 64];
        let mut req = httparse::Request::new(&mut headers);
        if !matches!(req.parse(buffer), Ok(httparse::Status::Complete(_))) {
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

/// The final response head with its framing and SSE flag.
struct Head<'b> {
    header_slots: [httparse::Header<'b>; 64],
    header_count: usize,
    body: &'b [u8],
    framing: Framing,
    is_sse: bool,
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
        // httparse fills the array from the front and shrinks `resp.headers` to the parsed ones.
        let header_count = resp.headers.len();
        return Some(Head {
            header_slots: headers,
            header_count,
            body: &rest[n..],
            framing,
            is_sse,
        });
    }
}

/// Framing and body of a final response sent as is: neither SSE nor content-encoded.
pub(super) fn plain_body(buffer: &[u8]) -> Option<(Framing, &[u8])> {
    let head = final_head(buffer)?;
    (!head.is_sse && !encoding::is_encoded(head.headers())).then_some((head.framing, head.body))
}

/// Framing and body of a final SSE response sent without a content coding.
pub(super) fn plain_sse_body(buffer: &[u8]) -> Option<(Framing, &[u8])> {
    let head = final_head(buffer)?;
    (head.is_sse && !encoding::is_encoded(head.headers())).then_some((head.framing, head.body))
}

fn header_contains(headers: &[httparse::Header], name: &str, pattern: &[u8]) -> bool {
    headers.iter().any(|h| {
        h.name.eq_ignore_ascii_case(name) && byte_utils::contains_pattern(h.value, pattern)
    })
}

impl ProtocolParser for Http11Parser {
    fn detect_request(&self, buffer: &[u8]) -> Option<String> {
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

            // Check if host+path matches any configured provider
            if PROVIDER_REGISTRY
                .find_provider(&host_header, path)
                .is_some()
            {
                // The path is logged and the query can carry a credential (Gemini `?key=`).
                return Some(path.split_once('?').map_or(path, |(p, _)| p).to_string());
            }
        }
        None
    }

    fn extract_request_text(&self, buffer: &[u8]) -> String {
        let body_start = byte_utils::find_pattern(buffer, b"\r\n\r\n")
            .map(|i| i + 4)
            .unwrap_or(0);

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
        let ResponseProgress { chunks, sse } = progress;
        let feed_sse = head.is_sse && !is_encoded;
        chunks.advance(body, is_chunked, |data| {
            if feed_sse {
                sse.feed(data);
            }
        });

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
            return sse.usage().or_else(|| {
                if is_chunked && ended() {
                    parse_sse_body(body)
                } else {
                    None
                }
            });
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
            return parse_sse_body(&decompressed);
        }

        let trimmed = byte_utils::trim_trailing_whitespace(&decompressed);

        // Streamed JSON arrays (Gemini streamGenerateContent without alt=sse): the first element is not the final usage.
        let is_json_array = trimmed.iter().find(|b| !b.is_ascii_whitespace()) == Some(&b'[');
        if is_json_array || !trimmed.ends_with(b"}") {
            return None;
        }

        let s = String::from_utf8_lossy(trimmed);
        let start = s.find('{')?;
        let json_body = &s[start..];

        parse_response_json(json_body)
    }
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
mod tests {
    use super::super::chunk_cases;
    use super::super::test_support::tokens;
    use super::*;

    #[test]
    fn chunked_sse_usage_event_split_across_chunks() {
        let ev1 = "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n";
        let ev2a = "data: {\"model\":\"gpt-4o\",\"choices\":[],\"usa";
        let ev2b = "ge\":{\"prompt_tokens\":11,\"completion_tokens\":4}}\n\ndata: [DONE]\n\n";
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n\
             {:x}\r\n{ev1}\r\n{:x}\r\n{ev2a}\r\n{:x}\r\n{ev2b}\r\n0\r\n\r\n",
            ev1.len(),
            ev2a.len(),
            ev2b.len(),
        );
        let info = Http11Parser.parse_response(resp.as_bytes());
        assert_eq!(tokens(info), Some((11, 4)));
    }

    #[test]
    fn chunked_sse_with_lost_bytes_falls_back_to_raw_lines() {
        let ev1 = "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n";
        let ev2 = "data: {\"model\":\"gpt-4o\",\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":4}}\n\ndata: [DONE]\n\n";
        // The first chunk claims 40 bytes more than arrived, as after a truncated SSL_read.
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n\
             {:x}\r\n{ev1}\r\n{:x}\r\n{ev2}\r\n0\r\n\r\n",
            ev1.len() + 40,
            ev2.len(),
        );
        let info = Http11Parser.parse_response(resp.as_bytes());
        assert_eq!(tokens(info), Some((11, 4)));
    }

    #[test]
    fn responses_create_is_detected_but_retrieval_is_not() {
        let post = b"POST /v1/responses HTTP/1.1\r\nHost: api.openai.com\r\n\r\n{\"input\":\"hi\"}";
        assert_eq!(
            Http11Parser.detect_request(post).as_deref(),
            Some("/v1/responses")
        );
        let get = b"GET /v1/responses/r1 HTTP/1.1\r\nHost: api.openai.com\r\n\r\n";
        assert_eq!(Http11Parser.detect_request(get), None);
    }

    #[test]
    fn responses_compact_is_detected_but_input_tokens_is_not() {
        let compact = b"POST /v1/responses/compact HTTP/1.1\r\nHost: api.openai.com\r\n\r\n{\"model\":\"gpt-6\",\"input\":\"hi\"}";
        assert_eq!(
            Http11Parser.detect_request(compact).as_deref(),
            Some("/v1/responses/compact")
        );
        let count = b"POST /v1/responses/input_tokens HTTP/1.1\r\nHost: api.openai.com\r\n\r\n{\"model\":\"gpt-6\",\"input\":\"hi\"}";
        assert_eq!(Http11Parser.detect_request(count), None);
    }

    #[test]
    fn only_post_to_a_generation_path_is_detected() {
        let post = b"POST /v1/chat/completions HTTP/1.1\r\nHost: api.openai.com\r\n\r\n{}";
        assert!(Http11Parser.detect_request(post).is_some());
        for req in [
            &b"GET /v1/chat/completions?limit=20 HTTP/1.1\r\nHost: api.openai.com\r\n\r\n"[..],
            b"GET /v1/chat/completions/chatcmpl-1 HTTP/1.1\r\nHost: api.openai.com\r\n\r\n",
            b"DELETE /v1/chat/completions/chatcmpl-1 HTTP/1.1\r\nHost: api.openai.com\r\n\r\n",
            b"POST /v1/chat/completions/chatcmpl-1 HTTP/1.1\r\nHost: api.openai.com\r\n\r\n{\"metadata\":{}}",
            b"POST /v1/messages/count_tokens HTTP/1.1\r\nHost: api.anthropic.com\r\n\r\n{}",
            b"GET /v1/messages/batches/msgbatch_01 HTTP/1.1\r\nHost: api.anthropic.com\r\n\r\n",
        ] {
            let line = String::from_utf8_lossy(req);
            assert_eq!(Http11Parser.detect_request(req), None, "{line}");
        }
    }

    #[test]
    fn detected_path_leaves_out_the_query() {
        let req = b"POST /v1beta/models/m:generateContent?key=AIzaFAKE HTTP/1.1\r\nHost: generativelanguage.googleapis.com\r\n\r\n{}";
        assert_eq!(
            Http11Parser.detect_request(req).as_deref(),
            Some("/v1beta/models/m:generateContent")
        );
    }

    #[test]
    fn request_head_needs_a_complete_http11_head() {
        assert!(Http11Parser::is_request_head(
            b"POST /v1/embeddings HTTP/1.1\r\nHost: api.openai.com\r\n\r\n{}"
        ));
        assert!(!Http11Parser::is_request_head(
            b"POST /v1/embeddings HTTP/1.1\r\nHost: api.op"
        ));
        assert!(!Http11Parser::is_request_head(
            b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n"
        ));
    }

    #[test]
    fn complete_response_follows_framing_after_interim_responses() {
        let complete: [&[u8]; 3] = [
            b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 2\r\n\r\nok",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nok\r\n0\r\n\r\n",
            b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok",
        ];
        for buf in complete {
            assert!(Http11Parser::is_complete_response(buf), "{buf:?}");
        }
        let incomplete: [&[u8]; 4] = [
            b"HTTP/1.1 200 OK\r\nContent-Length: 20\r\n\r\nok",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nok\r\n",
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\ndata: {}\n\n",
            b"HTTP/1.1 100 Continue\r\n\r\n",
        ];
        for buf in incomplete {
            assert!(!Http11Parser::is_complete_response(buf), "{buf:?}");
        }
    }

    #[test]
    fn final_response_after_interim_responses_is_parsed() {
        let json =
            r#"{"model":"gpt-4o","choices":[],"usage":{"prompt_tokens":11,"completion_tokens":4}}"#;
        let event = format!("data: {json}\n\ndata: [DONE]\n\n");
        let finals = [
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n\
                 {:x}\r\n{json}\r\n0\r\n\r\n",
                json.len()
            ),
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n\
                 {:x}\r\n{event}\r\n0\r\n\r\n",
                event.len()
            ),
            format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n{event}"),
        ];
        for interim in [
            "HTTP/1.1 100 Continue\r\n\r\n",
            "HTTP/1.1 103 Early Hints\r\nLink: </s.css>; rel=preload\r\n\r\nHTTP/1.1 100 Continue\r\n\r\n",
        ] {
            for last in &finals {
                let resp = format!("{interim}{last}");
                let info = Http11Parser.parse_response(resp.as_bytes());
                assert_eq!(tokens(info), Some((11, 4)), "{resp:?}");
            }
        }
    }

    #[test]
    fn content_length_zero_is_whole_without_usage() {
        let resp = b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n";
        assert!(Http11Parser::is_complete_response(resp));
        assert_eq!(tokens(Http11Parser.parse_response(resp)), None);
    }

    #[test]
    fn transfer_encoding_overrides_content_length() {
        let json =
            r#"{"model":"gpt-4o","choices":[],"usage":{"prompt_tokens":11,"completion_tokens":4}}"#;
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\
             Transfer-Encoding: chunked\r\n\r\n{:x}\r\n{json}\r\n0\r\n\r\n",
            json.len()
        );
        let before_last_chunk = &resp.as_bytes()[..resp.len() - "0\r\n\r\n".len()];
        assert!(!Http11Parser::is_complete_response(before_last_chunk));
        assert!(Http11Parser::is_complete_response(resp.as_bytes()));
        let info = Http11Parser.parse_response(resp.as_bytes());
        assert_eq!(tokens(info), Some((11, 4)));
    }

    #[test]
    fn chunked_on_a_later_transfer_encoding_line_frames_the_body() {
        let resp = "HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip\r\nTransfer-Encoding: chunked\r\n\r\n\
                    2\r\nok\r\n0\r\n\r\n";
        assert!(!Http11Parser::is_complete_response(
            &resp.as_bytes()[..resp.len() - "0\r\n\r\n".len()]
        ));
        assert!(Http11Parser::is_complete_response(resp.as_bytes()));
    }

    #[test]
    fn content_encoding_as_the_last_header_is_applied() {
        let json =
            r#"{"model":"gpt-4o","choices":[],"usage":{"prompt_tokens":11,"completion_tokens":4}}"#;
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
             Content-Encoding: compress\r\n\r\n{json}",
            json.len()
        );
        // An unknown coding is refused, so usage here means the last header was cut off.
        assert_eq!(tokens(Http11Parser.parse_response(resp.as_bytes())), None);
    }

    #[test]
    fn interim_only_or_partial_head_is_neither_whole_nor_parsed() {
        for buf in [
            "HTTP/1.1 100 Continue\r\n\r\n",
            "HTTP/1.1 103 Early Hints\r\nLink: </s.css>; rel=preload\r\n\r\nHTTP/1.1 100 Continue\r\n\r\n",
            "HTTP/1.1 100 Cont",
            "HTTP/1.1 200 OK\r\nContent-Le",
            "HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\nContent-Length: 2\r\n",
        ] {
            assert!(
                !Http11Parser::is_complete_response(buf.as_bytes()),
                "{buf:?}"
            );
            assert_eq!(
                tokens(Http11Parser.parse_response(buf.as_bytes())),
                None,
                "{buf:?}"
            );
        }
    }

    #[test]
    fn chunked_end_follows_chunk_sizes() {
        // The first chunk's data ends in `0\r\n`, so the bytes after it look like a last chunk.
        let head = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n";
        for last in [
            "0\r\n\r\n",
            "0;ext=1\r\n\r\n",
            "0\r\nX-Request-Id: r1\r\n\r\n",
        ] {
            let resp = format!("{head}5\r\nab0\r\n\r\n3;n=v\r\n{{}}\n\r\n{last}");
            assert!(
                Http11Parser::is_complete_response(resp.as_bytes()),
                "{resp:?}"
            );
            for cut in head.len()..resp.len() {
                let part = &resp.as_bytes()[..cut];
                assert!(
                    !Http11Parser::is_complete_response(part),
                    "{:?}",
                    &resp[..cut]
                );
            }
        }
    }

    #[test]
    fn cloud_hosts_are_detected_on_http11() {
        let detect = |url: &str| {
            let (host, path) = url.split_at(url.find('/').unwrap_or(url.len()));
            let req = format!("POST {path} HTTP/1.1\r\nHost: {host}\r\n\r\n{{}}");
            Http11Parser.detect_request(req.as_bytes()).is_some()
        };
        assert!(detect(
            "aws-external-anthropic.us-west-2.api.aws/v1/messages"
        ));
        assert!(detect(
            "bedrock-mantle.us-east-2.api.aws:443/openai/v1/responses"
        ));
        assert!(detect(
            "bedrock-runtime.us-east-1.amazonaws.com/model/m/converse"
        ));
        assert!(detect(
            "claude.googleapis.com/v1/projects/p/locations/global/workspaces/w/invoke/v1/messages"
        ));
        assert!(detect("myres.services.ai.azure.com/anthropic/v1/messages"));
        assert!(detect(
            "myres.openai.azure.com/openai/responses?api-version=2025-04-01-preview"
        ));
        assert!(detect(
            "myres.openai.azure.com/openai/deployments/d1/completions?api-version=2024-10-21"
        ));
        assert!(!detect("ec2.us-west-2.api.aws/v1/messages"));
    }

    fn chunked(head: &str, chunks: &[&str]) -> Vec<u8> {
        let mut resp = head.as_bytes().to_vec();
        for c in chunks {
            resp.extend_from_slice(format!("{:x}\r\n{c}\r\n", c.len()).as_bytes());
        }
        resp.extend_from_slice(b"0\r\n\r\n");
        resp
    }

    /// Responses whose resumed parse has to match a fresh one: framings, desyncs, protocol-looking text, non-UTF-8.
    fn resumable_responses() -> Vec<Vec<u8>> {
        let sse = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n";
        let finish = "data: {\"choices\":[{\"delta\":{\"content\":\"quote data: [DONE] and \\\"finishReason\\\"\"},\"finish_reason\":null}]}\n\n";
        let usage = "data: {\"model\":\"gpt-4o\",\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":4}}\n\ndata: [DONE]\n\n";
        let tail = format!("\r\n{finish}\r\n{:x}\r\n{usage}\r\n0\r\n\r\n", usage.len());
        // The first size line claims 40 bytes that never arrived, as after a truncated SSL_read.
        let lost = format!("{sse}{:x}{tail}", finish.len() + 40);
        let ext = format!("{sse}{:x};x=1{tail}", finish.len());
        let mut gemini = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n".to_vec();
        gemini.extend_from_slice("data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"h\u{e9} \u{1f41d}\"}]}}],\"usageMetadata\":{\"promptTokenCount\":7}}\r\n\r\n".as_bytes());
        gemini.extend_from_slice(b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"\xff\xfe\"}]},\"finishReason\":\"STOP\"}],");
        gemini.extend_from_slice(
            b"\"usageMetadata\":{\"promptTokenCount\":7,\"candidatesTokenCount\":3}}\r\n\r\n",
        );
        let anthropic = concat!(
            "HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n",
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-test\",\"usage\":{\"input_tokens\":25,\"output_tokens\":1}}}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":15}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        );
        let json =
            r#"{"model":"gpt-4o","choices":[],"usage":{"prompt_tokens":11,"completion_tokens":4}}"#;
        let json_head = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n";
        vec![
            chunked(sse, &[finish, &usage[..30], &usage[30..]]),
            lost.into_bytes(),
            ext.into_bytes(),
            gemini,
            anthropic.as_bytes().to_vec(),
            chunked(json_head, &[&json[..20], &json[20..]]),
        ]
    }

    #[test]
    fn resumed_parse_matches_a_fresh_parse_at_every_read_boundary() {
        for resp in resumable_responses() {
            let fresh = |end: usize| Http11Parser.parse_response(&resp[..end]);
            let text = String::from_utf8_lossy(&resp);
            assert!(fresh(resp.len()).is_some(), "{text}");
            let mut byte_reads = ResponseProgress::default();
            for end in 1..=resp.len() {
                let resumed = Http11Parser.parse_response_from(&resp[..end], &mut byte_reads);
                assert_eq!(resumed, fresh(end), "byte reads, at {end}: {text}");
                let mut two_reads = ResponseProgress::default();
                Http11Parser.parse_response_from(&resp[..end], &mut two_reads);
                let whole = Http11Parser.parse_response_from(&resp, &mut two_reads);
                assert_eq!(whole, fresh(resp.len()), "split at {end}: {text}");
            }
        }
    }

    #[test]
    fn dechunker_passes_on_what_decode_chunked_body_returns() {
        let bodies: [&[u8]; 6] = [
            b"5\r\nhello\r\n3\r\n, w\r\n0\r\n\r\n",
            b"9\r\nhello\r\n3\r\n, w\r\n0\r\n\r\n",
            b"5;x=1\r\nhello\r\n0\r\n\r\n",
            b"5\r\nhello\r\n3;x=1\r\n, w\r\n0\r\n\r\n",
            b"zz\r\ndata: {}\n\n",
            b"ffffffffffffffff\r\n{}\r\n0\r\n\r\n",
        ];
        for body in bodies {
            for end in 1..=body.len() {
                let prefix = &body[..end];
                let mut got = Vec::new();
                let mut dechunker = Dechunker::default();
                for read in 1..=end {
                    dechunker.advance(&prefix[..read], true, |d| got.extend_from_slice(d));
                }
                match byte_utils::decode_chunked_body(prefix) {
                    std::borrow::Cow::Owned(want) => assert_eq!(got, want, "{prefix:?}"),
                    // Unchanged: no CRLF yet, nothing decoded yet, or read as is.
                    std::borrow::Cow::Borrowed(_) => {
                        assert!(got.is_empty() || got == prefix, "{prefix:?}: {got:?}")
                    }
                }
            }
        }
    }

    #[test]
    fn chunk_walkers_in_h1_keep_their_answers() {
        for case in chunk_cases::chunk_cases() {
            let text = String::from_utf8_lossy(case.body);
            let decoded = byte_utils::decode_chunked_body(case.body);
            assert_eq!(&*decoded, case.or_body(case.decoded), "decode {text:?}");
            assert_eq!(chunked_ended(case.body), case.ended, "ended {text:?}");
            let (want, stopped) = case.dechunked;
            for split in 0..=case.body.len() {
                let (mut dechunker, mut got) = (Dechunker::default(), Vec::new());
                dechunker.advance(&case.body[..split], true, |d| got.extend_from_slice(d));
                dechunker.advance(case.body, true, |d| got.extend_from_slice(d));
                let at = format!("dechunk split {split} {text:?}");
                assert_eq!(
                    (&got[..], dechunker.stopped()),
                    (case.or_body(want), stopped),
                    "{at}"
                );
            }
        }
    }

    #[test]
    fn json_array_stream_is_not_parsed_from_first_element() {
        let resp = concat!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n",
            "[{\"candidates\":[],\"usageMetadata\":{\"promptTokenCount\":9,\"candidatesTokenCount\":1}}",
        );
        let info = Http11Parser.parse_response(resp.as_bytes());
        assert_eq!(tokens(info), None);
    }
}

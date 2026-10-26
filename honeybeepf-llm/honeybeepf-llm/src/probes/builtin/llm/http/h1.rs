//! HTTP/1.1 request detection and response parsing: framing, decoding, SSE or JSON usage.

use honeybeepf_llm_core::byte_utils;
use honeybeepf_llm_core::types::UsageInfo;

use super::ProtocolParser;
use super::body::{extract_text_from_json, parse_response_json};
use super::encoding;
use super::registry::PROVIDER_REGISTRY;
use super::sse::parse_sse_body;
use crate::probes::builtin::llm::processor::MAX_RESPONSE_BUFFER_SIZE;

#[derive(Clone)]
pub struct Http11Parser;

impl Http11Parser {
    /// Whether the buffer starts with a complete HTTP/1.1 request head.
    pub fn is_request_head(buffer: &[u8]) -> bool {
        let mut headers = [httparse::EMPTY_HEADER; 64];
        let mut req = httparse::Request::new(&mut headers);
        matches!(req.parse(buffer), Ok(httparse::Status::Complete(_)))
    }

    /// Whether the buffer holds a whole HTTP/1.1 response by its framing, after any 1xx interim responses.
    pub fn is_complete_response(buffer: &[u8]) -> bool {
        let mut rest = buffer;
        loop {
            let mut headers = [httparse::EMPTY_HEADER; 64];
            let mut resp = httparse::Response::new(&mut headers);
            let Ok(httparse::Status::Complete(n)) = resp.parse(rest) else {
                return false;
            };
            let body = &rest[n..];
            if resp.code.is_some_and(|c| (100..200).contains(&c)) {
                rest = body;
                continue;
            }
            if let Some(len) = content_length(resp.headers) {
                return body.len() >= len;
            }
            return resp
                .headers
                .iter()
                .find(|h| h.name.eq_ignore_ascii_case("Transfer-Encoding"))
                .is_some_and(|h| byte_utils::contains_pattern(h.value, b"chunked"))
                && chunked_ended(body);
        }
    }
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
        let mut headers = [httparse::EMPTY_HEADER; 64];
        let mut resp = httparse::Response::new(&mut headers);

        let body_offset = match resp.parse(buffer) {
            Ok(httparse::Status::Complete(n)) => n,
            _ => return None,
        };

        let is_sse = resp.headers.iter().any(|h| {
            h.name.eq_ignore_ascii_case("Content-Type")
                && String::from_utf8_lossy(h.value).contains("text/event-stream")
        });
        let is_chunked = resp.headers.iter().any(|h| {
            h.name.eq_ignore_ascii_case("Transfer-Encoding")
                && String::from_utf8_lossy(h.value).contains("chunked")
        });
        let is_encoded = encoding::is_encoded(resp.headers);

        let body = &buffer[body_offset..];
        let ended = if is_chunked {
            chunked_ended(body)
        } else {
            content_length(resp.headers).is_none_or(|len| body.len() >= len)
        };
        // Parsing re-runs on every read and decoding is not incremental, so wait for the framed end.
        if is_encoded && !ended {
            return None;
        }

        // Decode chunked transfer encoding first
        let dechunked = if is_chunked {
            byte_utils::decode_chunked_body(body)
        } else {
            std::borrow::Cow::Borrowed(body)
        };
        let decompressed = encoding::decode(dechunked, resp.headers)?;

        // SSE events can straddle chunk boundaries, so they are parsed only after dechunking.
        if is_sse {
            // A truncated read desyncs the chunk framing; once the stream has ended, fall back to raw lines.
            return parse_sse_body(&decompressed).or_else(|| {
                if is_chunked && !is_encoded && ended {
                    parse_sse_body(body)
                } else {
                    None
                }
            });
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
    // Bytes lost in capture break the framing; then only the terminator bytes are left to go by.
    let desynced = || body.ends_with(b"0\r\n\r\n");
    let mut rest = body;
    loop {
        let Some(line_end) = byte_utils::find_pattern(rest, b"\r\n") else {
            return false;
        };
        let size = rest[..line_end].split(|&b| b == b';').next().unwrap_or(&[]);
        let Some(size) = std::str::from_utf8(size)
            .ok()
            .and_then(|s| usize::from_str_radix(s.trim(), 16).ok())
        else {
            return desynced();
        };
        // A chunk the response buffer can never hold is a garbled size line, not one still arriving.
        if size > MAX_RESPONSE_BUFFER_SIZE {
            return desynced();
        }
        rest = &rest[line_end + 2..];
        if size == 0 {
            return rest.starts_with(b"\r\n") || byte_utils::contains_pattern(rest, b"\r\n\r\n");
        }
        match rest.get(size..).and_then(|after| after.get(..2)) {
            None => return false,
            Some(b"\r\n") => rest = &rest[size + 2..],
            Some(_) => return desynced(),
        }
    }
}

fn content_length(headers: &[httparse::Header]) -> Option<usize> {
    headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case("Content-Length"))
        .and_then(|h| std::str::from_utf8(h.value).ok())
        .and_then(|v| v.trim().parse::<usize>().ok())
}

#[cfg(test)]
mod tests {
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

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
        Http11Parser.detect_request(post).map(|d| d.path).as_deref(),
        Some("/v1/responses")
    );
    let get = b"GET /v1/responses/r1 HTTP/1.1\r\nHost: api.openai.com\r\n\r\n";
    assert!(Http11Parser.detect_request(get).is_none());
}

#[test]
fn retrieval_gets_give_their_response_id() {
    for (req, id) in [
        (&b"GET /v1/responses/resp_1 HTTP/1.1\r\nHost: api.openai.com\r\n\r\n"[..], Some("resp_1")),
        (b"GET /v1/responses/resp_1?stream=true&starting_after=2 HTTP/1.1\r\nHost: api.openai.com\r\n\r\n", Some("resp_1")),
        (b"GET /openai/responses/resp_1?api-version=2025-04-01-preview HTTP/1.1\r\nHost: r.openai.azure.com\r\n\r\n", Some("resp_1")),
        (b"GET /v1/responses/resp_1/input_items HTTP/1.1\r\nHost: api.openai.com\r\n\r\n", None),
        (b"GET /v1/responses/resp_1 HTTP/1.1\r\nHost: noise.internal\r\n\r\n", None),
        (b"POST /v1/responses/resp_1 HTTP/1.1\r\nHost: api.openai.com\r\n\r\n", None),
        (b"GET /v1/responses/ HTTP/1.1\r\nHost: api.openai.com\r\n\r\n", None),
        (b"GET /v1/responses/resp%201 HTTP/1.1\r\nHost: api.openai.com\r\n\r\n", None),
        (b"GET /v1/responses/resp_1 HTTP/1.1\r\n\r\n", None),
    ] {
        assert_eq!(Http11Parser::retrieved_response_id(req), id, "{req:?}");
    }
}

#[test]
fn responses_compact_is_detected_but_input_tokens_is_not() {
    let compact = b"POST /v1/responses/compact HTTP/1.1\r\nHost: api.openai.com\r\n\r\n{\"model\":\"gpt-6\",\"input\":\"hi\"}";
    assert_eq!(
        Http11Parser
            .detect_request(compact)
            .map(|d| d.path)
            .as_deref(),
        Some("/v1/responses/compact")
    );
    let count = b"POST /v1/responses/input_tokens HTTP/1.1\r\nHost: api.openai.com\r\n\r\n{\"model\":\"gpt-6\",\"input\":\"hi\"}";
    assert!(Http11Parser.detect_request(count).is_none());
}

#[test]
fn images_and_transcriptions_are_detected_by_post() {
    for path in [
        "/v1/images/generations",
        "/v1/images/edits",
        "/v1/audio/transcriptions",
        "/v1/audio/translations",
    ] {
        let req = format!("POST {path} HTTP/1.1\r\nHost: api.openai.com\r\n\r\n{{}}");
        assert_eq!(
            Http11Parser
                .detect_request(req.as_bytes())
                .map(|d| d.path)
                .as_deref(),
            Some(path),
            "{path}"
        );
    }
    // Speech (TTS) is not a recorded path yet, and a GET is not a generation call.
    let tts = b"POST /v1/audio/speech HTTP/1.1\r\nHost: api.openai.com\r\n\r\n{}";
    assert!(Http11Parser.detect_request(tts).is_none());
    let get = b"GET /v1/images/generations HTTP/1.1\r\nHost: api.openai.com\r\n\r\n";
    assert!(Http11Parser.detect_request(get).is_none());
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
        assert!(Http11Parser.detect_request(req).is_none(), "{line}");
    }
}

#[test]
fn detected_path_leaves_out_the_query() {
    let req = b"POST /v1beta/models/m:generateContent?key=AIzaFAKE HTTP/1.1\r\nHost: generativelanguage.googleapis.com\r\n\r\n{}";
    assert_eq!(
        Http11Parser.detect_request(req).map(|d| d.path).as_deref(),
        Some("/v1beta/models/m:generateContent")
    );
}

#[test]
fn managed_agents_session_posts_are_detected_as_record_only() {
    for path in ["/v1/sessions", "/v1/sessions/sess_01/events"] {
        let req = format!("POST {path} HTTP/1.1\r\nHost: api.anthropic.com\r\n\r\n{{}}");
        let detected = Http11Parser
            .detect_request(req.as_bytes())
            .unwrap_or_else(|| panic!("{path} not detected"));
        assert_eq!(detected.path, path);
        assert!(detected.record_only, "{path}");
    }
    // The event stream is a GET, and the Messages path stays a normal recorded call.
    let stream =
        b"GET /v1/sessions/sess_01/events/stream HTTP/1.1\r\nHost: api.anthropic.com\r\n\r\n";
    assert!(Http11Parser.detect_request(stream).is_none());
    let messages = b"POST /v1/messages HTTP/1.1\r\nHost: api.anthropic.com\r\n\r\n{}";
    let msg = Http11Parser.detect_request(messages).expect("messages");
    assert!(!msg.record_only);
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
    let sse =
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n";
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
    let json_head =
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n";
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

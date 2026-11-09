use super::*;
use honeybeepf_llm_common::{MAX_SSL_BUF_SIZE, MAX_SSL_FRAGMENTS};

const REQUEST: &[u8] = b"POST /v1/chat/completions HTTP/1.1\r\nHost: api.openai.com\r\nContent-Type: application/json\r\nContent-Length: 27\r\n\r\n{\"model\":\"t\",\"messages\":[]}";

fn ctx() -> CallContext {
    CallContext {
        pid: 1,
        #[cfg(feature = "k8s")]
        pod_info: None,
    }
}

/// Sends one SSL call the way the kernel does, leaving out the fragment at `skip`.
fn send(
    p: &mut StreamProcessor,
    dir: LlmDirection,
    call: &[u8],
    skip: Option<usize>,
) -> Vec<Outcome> {
    send_with(p, &mut BackgroundCalls::default(), dir, call, skip)
}

fn send_with(
    p: &mut StreamProcessor,
    bg: &mut BackgroundCalls,
    dir: LlmDirection,
    call: &[u8],
    skip: Option<usize>,
) -> Vec<Outcome> {
    let limit = capture_limit(dir);
    let truncated = call.len() > limit;
    let mut out = Vec::new();
    for i in 0..MAX_SSL_FRAGMENTS {
        let offset = i * MAX_SSL_BUF_SIZE;
        if i > 0 && (offset >= call.len() || offset >= limit) {
            break;
        }
        if skip == Some(i) {
            continue;
        }
        let data = &call[offset..call.len().min(offset + MAX_SSL_BUF_SIZE)];
        out.push(p.handle_fragment(
            dir,
            data,
            offset as u32,
            call.len() as u32,
            truncated,
            bg,
            &ctx(),
        ));
    }
    out
}

/// Response head plus body, split into the 16 KB TLS records a client reads one at a time.
fn h1_response(body_len: usize) -> Vec<Vec<u8>> {
    let pad = "x".repeat(body_len.saturating_sub(200));
    let body = format!(
        r#"{{"id": "c1", "object": "chat.completion", "model": "gpt-4o", "choices": [{{"index": 0, "message": {{"role": "assistant", "content": "{pad}"}}, "finish_reason": "stop"}}], "usage": {{"prompt_tokens": 11, "completion_tokens": 4, "total_tokens": 15}}}}"#
    );
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    let mut reads = vec![head.into_bytes()];
    reads.extend(body.as_bytes().chunks(16384).map(<[u8]>::to_vec));
    reads
}

fn outcomes(
    p: &mut StreamProcessor,
    reads: &[Vec<u8>],
    skip: Option<(usize, usize)>,
) -> Vec<Outcome> {
    let mut out = send(p, LlmDirection::Write, REQUEST, None);
    for (n, r) in reads.iter().enumerate() {
        let skip = skip.filter(|(read, _)| *read == n).map(|(_, frag)| frag);
        out.extend(send(p, LlmDirection::Read, r, skip));
    }
    out
}

#[test]
fn test_response_with_last_record_over_4kb_is_recorded_once() {
    // 40 KB body: records of 16384, 16384 and ~7.5 KB; usage sits in the last one.
    for body_len in [6000, 16000, 40237] {
        let mut p = StreamProcessor::new();
        let out = outcomes(&mut p, &h1_response(body_len), None);
        let recorded = out.iter().filter(|o| **o == Outcome::Recorded).count();
        assert_eq!(
            (recorded, out.last()),
            (1, Some(&Outcome::Recorded)),
            "{out:?}"
        );
    }
}

#[test]
fn test_lost_fragment_counts_response_as_unparsed() {
    // 20 KB body: a lost middle fragment falls inside the content string, so parsing could
    // still "succeed"; a lost tail fragment only shows when the next call starts at offset 0.
    for lost in [1, 3] {
        let mut p = StreamProcessor::new();
        let out = outcomes(&mut p, &h1_response(20200), Some((1, lost)));
        assert_eq!(
            unparsed(&out),
            [UnparsedReason::CaptureLost],
            "fragment {lost}: {out:?}"
        );
        assert!(
            !out.contains(&Outcome::Recorded),
            "fragment {lost}: {out:?}"
        );

        let next = outcomes(&mut p, &h1_response(3000), None);
        assert_eq!(next.last(), Some(&Outcome::Recorded), "{next:?}");
    }
}

#[test]
fn test_read_over_capture_limit_is_unparsed() {
    let mut p = StreamProcessor::new();
    let mut reads = h1_response(40237);
    // One SSL_read returning more than one TLS record.
    let joined = reads.drain(1..).flatten().collect::<Vec<u8>>();
    reads.push(joined);
    let out = outcomes(&mut p, &reads, None);
    assert_eq!(unparsed(&out), [UnparsedReason::CaptureLost], "{out:?}");
    assert!(!out.contains(&Outcome::Recorded), "{out:?}");
}

/// The receive the way the pre-fix kernel emitted it: one record of at most MAX_SSL_BUF_SIZE, marked
/// truncated when the skb is larger, so the tail where usage lives is never sent.
fn plain_recv_one_bucket(p: &mut StreamProcessor, bg: &mut BackgroundCalls, skb: &[u8]) -> Outcome {
    let captured = &skb[..skb.len().min(MAX_SSL_BUF_SIZE)];
    p.handle_fragment(
        LlmDirection::Read,
        captured,
        0,
        skb.len() as u32,
        skb.len() > MAX_SSL_BUF_SIZE,
        bg,
        &ctx(),
    )
}

/// The receive the way the fixed `capture_recv` emits it: up to MAX_SSL_FRAGMENTS records of at most
/// MAX_SSL_BUF_SIZE, each at its own offset, truncated only when the skb exceeds the Read limit.
fn plain_recv_skb(p: &mut StreamProcessor, bg: &mut BackgroundCalls, skb: &[u8]) -> Vec<Outcome> {
    let limit = capture_limit(LlmDirection::Read);
    let truncated = skb.len() > limit;
    let mut out = Vec::new();
    for i in 0..MAX_SSL_FRAGMENTS {
        let offset = i * MAX_SSL_BUF_SIZE;
        if i > 0 && (offset >= skb.len() || offset >= limit) {
            break;
        }
        let data = &skb[offset..skb.len().min(offset + MAX_SSL_BUF_SIZE)];
        out.push(p.handle_fragment(
            LlmDirection::Read,
            data,
            offset as u32,
            skb.len() as u32,
            truncated,
            bg,
            &ctx(),
        ));
    }
    out
}

#[test]
fn plaintext_large_response_in_one_skb_records_only_when_fragmented() {
    // The same >4 KB non-streaming JSON response, delivered GRO-coalesced in one skb.
    let resp: Vec<u8> = h1_response(6000).concat();
    assert!(resp.len() > MAX_SSL_BUF_SIZE);

    // Pre-fix: one truncated record, so the tail never arrives and the processor drops the response.
    let mut dropped = StreamProcessor::new();
    let mut bg = BackgroundCalls::default();
    send_with(&mut dropped, &mut bg, LlmDirection::Write, REQUEST, None);
    let one = plain_recv_one_bucket(&mut dropped, &mut bg, &resp);
    assert_ne!(one, Outcome::Recorded, "one truncated record: {one:?}");

    // Fixed: the skb is split into fragments, so the whole response reaches the parser and records.
    let mut p = StreamProcessor::new();
    let mut bg = BackgroundCalls::default();
    send_with(&mut p, &mut bg, LlmDirection::Write, REQUEST, None);
    let out = plain_recv_skb(&mut p, &mut bg, &resp);
    assert_eq!(out.last(), Some(&Outcome::Recorded), "fragmented: {out:?}");
}

#[test]
fn test_request_over_capture_limit_still_detected() {
    // Large prompts (images) arrive as one SSL_write; only the head is needed for detection.
    let mut p = StreamProcessor::new();
    let mut big = REQUEST.to_vec();
    big.extend(std::iter::repeat_n(b' ', 30000));
    let mut out = send(&mut p, LlmDirection::Write, &big, None);
    for r in &h1_response(3000) {
        out.extend(send(&mut p, LlmDirection::Read, r, None));
    }
    assert_eq!(out.last(), Some(&Outcome::Recorded), "{out:?}");
}

const MESSAGES_REQUEST: &[u8] = b"POST /v1/messages HTTP/1.1\r\nHost: api.anthropic.com\r\nContent-Type: application/json\r\n\r\n{\"model\":\"claude-opus-5-5\",\"max_tokens\":64,\"messages\":[{\"role\":\"user\",\"content\":\"Hello, Claude\"}]}";
const MESSAGES_RESPONSE: &str = r#"{"id":"msg_1","type":"message","model":"claude-opus-5-5","content":[],"usage":{"input_tokens":14,"output_tokens":36}}"#;

fn h1(content_type: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// One request and its response on the same stream; the response head and body are read separately.
fn exchange(p: &mut StreamProcessor, request: &[u8], response: &[u8]) -> Vec<Outcome> {
    exchange_with(p, &mut BackgroundCalls::default(), request, response)
}

fn exchange_with(
    p: &mut StreamProcessor,
    bg: &mut BackgroundCalls,
    request: &[u8],
    response: &[u8],
) -> Vec<Outcome> {
    let mut out = send_with(p, bg, LlmDirection::Write, request, None);
    let split = response
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map_or(response.len(), |i| i + 4);
    let (head, body) = response.split_at(split);
    out.extend(send_with(p, bg, LlmDirection::Read, head, None));
    if !body.is_empty() {
        out.extend(send_with(p, bg, LlmDirection::Read, body, None));
    }
    out
}

fn count(out: &[Outcome], want: Outcome) -> usize {
    out.iter().filter(|o| **o == want).count()
}

fn unparsed(out: &[Outcome]) -> Vec<UnparsedReason> {
    out.iter()
        .filter_map(|o| match o {
            Outcome::Unparsed(reason) => Some(*reason),
            _ => None,
        })
        .collect()
}

const SESSION_REQUEST: &[u8] = b"POST /v1/sessions HTTP/1.1\r\nHost: api.anthropic.com\r\nContent-Type: application/json\r\n\r\n{\"system\":\"You are helpful\"}";
const EVENTS_REQUEST: &[u8] = b"POST /v1/sessions/sess_1/events HTTP/1.1\r\nHost: api.anthropic.com\r\nContent-Type: application/json\r\n\r\n{\"events\":[]}";
// The session object carries a cumulative session total; it must be counted, never recorded as tokens.
const SESSION_RESPONSE: &str = r#"{"id":"sess_1","type":"session","usage":{"input_tokens":5000,"output_tokens":3200,"cache_read_input_tokens":20000,"list_cost":{"amount":"187","currency":"USD"}}}"#;
// An events send is acknowledged with no usage; the call is still a session call and must be counted.
const EVENTS_ACK: &str = r#"{"data":[{"id":"evt_1","type":"user.message"}]}"#;

#[test]
fn test_managed_agents_session_call_is_counted_agent_session_not_recorded() {
    let mut p = StreamProcessor::new();
    let out = exchange(
        &mut p,
        SESSION_REQUEST,
        &h1("application/json", SESSION_RESPONSE),
    );
    assert_eq!(unparsed(&out), [UnparsedReason::AgentSession], "{out:?}");
    assert_eq!(count(&out, Outcome::Recorded), 0, "{out:?}");

    // A usage-less ack is counted the same way: completion, not usage, is the signal (matches HTTP/2).
    let ack = exchange(&mut p, EVENTS_REQUEST, &h1("application/json", EVENTS_ACK));
    assert_eq!(unparsed(&ack), [UnparsedReason::AgentSession], "{ack:?}");

    // A later Messages call on the same stream records normally: record_only is cleared on reset.
    let next = exchange(
        &mut p,
        MESSAGES_REQUEST,
        &h1("application/json", MESSAGES_RESPONSE),
    );
    assert_eq!(next.last(), Some(&Outcome::Recorded), "{next:?}");
}

#[test]
fn test_messages_after_count_tokens_is_recorded() {
    let mut p = StreamProcessor::new();
    let count_tokens = b"POST /v1/messages/count_tokens HTTP/1.1\r\nHost: api.anthropic.com\r\nContent-Type: application/json\r\n\r\n{\"model\":\"claude-opus-5-5\",\"messages\":[{\"role\":\"user\",\"content\":\"Hello, Claude\"}]}";
    let mut out = exchange(
        &mut p,
        count_tokens,
        &h1("application/json", r#"{"input_tokens":14}"#),
    );
    out.extend(exchange(
        &mut p,
        MESSAGES_REQUEST,
        &h1("application/json", MESSAGES_RESPONSE),
    ));
    assert_eq!(count(&out, Outcome::Recorded), 1, "{out:?}");
    assert!(unparsed(&out).is_empty(), "{out:?}");
}

#[test]
fn test_responses_compact_after_input_tokens_is_recorded() {
    let input_tokens = b"POST /v1/responses/input_tokens HTTP/1.1\r\nHost: api.openai.com\r\nContent-Type: application/json\r\n\r\n{\"model\":\"gpt-6\",\"input\":\"Hello\"}";
    let compact = b"POST /v1/responses/compact HTTP/1.1\r\nHost: api.openai.com\r\nContent-Type: application/json\r\n\r\n{\"model\":\"gpt-6\",\"input\":\"Hello\"}";
    let compacted = h1(
        "application/json",
        r#"{"id":"resp_1","object":"response.compaction","output":[{"type":"compaction","id":"cmp_1","encrypted_content":"e"}],"created_at":1731459200,"usage":{"input_tokens":1200,"input_tokens_details":{"cached_tokens":1024},"output_tokens":300,"output_tokens_details":{"reasoning_tokens":0},"total_tokens":1500}}"#,
    );
    let mut p = StreamProcessor::new();
    let mut out = exchange(
        &mut p,
        input_tokens,
        &h1(
            "application/json",
            r#"{"object":"response.input_tokens","input_tokens":11}"#,
        ),
    );
    out.extend(exchange(&mut p, compact, &compacted));
    // Head and body in one read.
    out.extend(send(&mut p, LlmDirection::Write, compact, None));
    out.extend(send(&mut p, LlmDirection::Read, &compacted, None));
    assert_eq!(count(&out, Outcome::Recorded), 2, "{out:?}");
    assert!(unparsed(&out).is_empty(), "{out:?}");
}

#[test]
fn test_chat_after_non_llm_request_is_recorded() {
    let mut p = StreamProcessor::new();
    // Moderations is not a generation endpoint, so it is detected as non-LLM; the chat that follows on
    // the same connection must still be recorded.
    let non_llm = b"POST /v1/moderations HTTP/1.1\r\nHost: api.openai.com\r\nContent-Type: application/json\r\n\r\n{\"model\":\"omni-moderation-latest\",\"input\":\"hi\"}";
    let mut out = exchange(
        &mut p,
        non_llm,
        &h1(
            "application/json",
            r#"{"id":"modr-1","model":"omni-moderation-latest","results":[]}"#,
        ),
    );
    let reads = h1_response(3000);
    out.extend(outcomes(&mut p, &reads, None));
    assert_eq!(count(&out, Outcome::Recorded), 1, "{out:?}");
    assert_eq!(out.last(), Some(&Outcome::Recorded), "{out:?}");
}

/// A multipart upload whose last body writes land after detection gave up at 4 KB, as httpx sends it.
/// The path must stay a non-LLM one (file upload, not transcription, which is a recorded path):
/// the point is a call recorded after an untracked upload.
fn upload(p: &mut StreamProcessor, expect_continue: bool) -> Vec<Outcome> {
    let head = format!(
        "POST /v1/files HTTP/1.1\r\nHost: api.openai.com\r\n\
         Content-Type: multipart/form-data; boundary=b0\r\nContent-Length: 66614\r\n{}\r\n",
        if expect_continue {
            "Expect: 100-continue\r\n"
        } else {
            ""
        }
    );
    let mut out = send(p, LlmDirection::Write, head.as_bytes(), None);
    if expect_continue {
        out.extend(send(
            p,
            LlmDirection::Read,
            b"HTTP/1.1 100 Continue\r\n\r\n",
            None,
        ));
    }
    let part = b"--b0\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.wav\"\r\n\r\n";
    for body in [&part[..], &[0xAB; 65536], &[0xCD; 1000], b"\r\n--b0--\r\n"] {
        out.extend(send(p, LlmDirection::Write, body, None));
    }
    out.extend(send(
        p,
        LlmDirection::Read,
        &h1("application/json", r#"{"text":"hello"}"#),
        None,
    ));
    out
}

#[test]
fn test_calls_after_upload_are_recorded() {
    use std::io::Write as _;
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gz.write_all(MESSAGES_RESPONSE.as_bytes()).expect("gzip");
    let body = gz.finish().expect("gzip");
    let gzip = [
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Encoding: gzip\r\n\
             Content-Length: {}\r\n\r\n",
            body.len()
        )
        .as_bytes(),
        &body,
    ]
    .concat();
    for expect_continue in [false, true] {
        let mut p = StreamProcessor::new();
        let mut out = upload(&mut p, expect_continue);
        for response in [&gzip, &h1("application/json", MESSAGES_RESPONSE)] {
            out.extend(exchange(&mut p, MESSAGES_REQUEST, response));
        }
        assert_eq!(
            count(&out, Outcome::Recorded),
            2,
            "{expect_continue}: {out:?}"
        );
        assert!(unparsed(&out).is_empty(), "{expect_continue}: {out:?}");
    }
}

#[test]
fn test_stored_completion_retrieval_is_not_recorded() {
    let mut p = StreamProcessor::new();
    let mut out = outcomes(&mut p, &h1_response(3000), None);
    let retrieve =
        b"GET /v1/chat/completions/chatcmpl-abc123 HTTP/1.1\r\nHost: api.openai.com\r\n\r\n";
    let stored = r#"{"object":"chat.completion","id":"chatcmpl-abc123","model":"gpt-6-astra","choices":[],"usage":{"total_tokens":31,"completion_tokens":18,"prompt_tokens":13}}"#;
    out.extend(exchange(&mut p, retrieve, &h1("application/json", stored)));
    assert_eq!(count(&out, Outcome::Recorded), 1, "{out:?}");
}

#[test]
fn test_response_without_usage_is_unparsed_when_next_request_starts() {
    let mut p = StreamProcessor::new();
    let bad_gateway = b"HTTP/1.1 502 Bad Gateway\r\nContent-Type: text/html\r\nContent-Length: 34\r\n\r\n<html><body>502 Bad Gateway</body>";
    let mut out = exchange(&mut p, MESSAGES_REQUEST, bad_gateway);
    out.extend(exchange(
        &mut p,
        MESSAGES_REQUEST,
        &h1("application/json", MESSAGES_RESPONSE),
    ));
    assert_eq!(unparsed(&out), [UnparsedReason::NextRequest], "{out:?}");
    assert_eq!(count(&out, Outcome::Recorded), 1, "{out:?}");
    assert_eq!(out.last(), Some(&Outcome::Recorded), "{out:?}");
}

// A complete 2xx response that carried no usage (an embedding whose provider omits it) is counted
// as usage_absent when it ends, not left to the next-request or close reason.
#[test]
fn test_framed_success_without_usage_is_usage_absent() {
    let mut p = StreamProcessor::new();
    let body = r#"{"id":"msg_1","type":"message","model":"m"}"#;
    let out = exchange(&mut p, MESSAGES_REQUEST, &h1("application/json", body));
    assert_eq!(unparsed(&out), [UnparsedReason::UsageAbsent], "{out:?}");
    assert_eq!(count(&out, Outcome::Recorded), 0, "{out:?}");
}

// A response still short of its Content-Length has no usage yet but must not be called usage_absent;
// the reader keeps waiting for the rest.
#[test]
fn test_partial_response_is_not_usage_absent() {
    let mut p = StreamProcessor::new();
    let mut out = send(&mut p, LlmDirection::Write, MESSAGES_REQUEST, None);
    let head = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 64\r\n\r\n{\"id\":\"msg_1\"";
    out.extend(send(&mut p, LlmDirection::Read, head, None));
    assert_eq!(unparsed(&out), [], "{out:?}");
    assert_eq!(count(&out, Outcome::Recorded), 0, "{out:?}");
}

// A usage-less transcription body (whisper-1, or a duration-billed model whose tokens are not read
// here) carries no tokens, so the call counts as usage_absent ("seen, tokens unavailable") rather than
// being dropped silently.
#[test]
fn test_transcription_without_usage_is_unparsed() {
    let mut p = StreamProcessor::new();
    let transcribe = b"POST /v1/audio/transcriptions HTTP/1.1\r\nHost: api.openai.com\r\n\r\n{}";
    let mut out = exchange(
        &mut p,
        transcribe,
        &h1("application/json", r#"{"text":"hello"}"#),
    );
    out.extend(exchange(
        &mut p,
        MESSAGES_REQUEST,
        &h1("application/json", MESSAGES_RESPONSE),
    ));
    assert_eq!(unparsed(&out), [UnparsedReason::UsageAbsent], "{out:?}");
    assert_eq!(count(&out, Outcome::Recorded), 1, "{out:?}");
}

// A recorded transcription can arrive as a large multipart upload (httpx spills past the 4 KB
// detection window) followed by a usage-bearing response: it is recorded and the next call is too.
#[test]
fn test_transcription_upload_then_chat_is_recorded() {
    let mut p = StreamProcessor::new();
    let head = "POST /v1/audio/transcriptions HTTP/1.1\r\nHost: api.openai.com\r\n\
         Content-Type: multipart/form-data; boundary=b0\r\nContent-Length: 66614\r\n\r\n";
    let mut out = send(&mut p, LlmDirection::Write, head.as_bytes(), None);
    let part = b"--b0\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.wav\"\r\n\r\n";
    for body in [&part[..], &[0xAB; 65536], &[0xCD; 1000], b"\r\n--b0--\r\n"] {
        out.extend(send(&mut p, LlmDirection::Write, body, None));
    }
    let resp = r#"{"text":"hi","usage":{"type":"tokens","input_tokens":14,"output_tokens":4,"total_tokens":18}}"#;
    out.extend(send(
        &mut p,
        LlmDirection::Read,
        &h1("application/json", resp),
        None,
    ));
    out.extend(exchange(
        &mut p,
        MESSAGES_REQUEST,
        &h1("application/json", MESSAGES_RESPONSE),
    ));
    assert_eq!(count(&out, Outcome::Recorded), 2, "{out:?}");
    assert!(unparsed(&out).is_empty(), "{out:?}");
}

#[test]
fn test_body_after_100_continue_is_recorded() {
    let mut p = StreamProcessor::new();
    let (head, body) = REQUEST.split_at(REQUEST.len() - 27);
    let mut out = send(&mut p, LlmDirection::Write, head, None);
    out.extend(send(
        &mut p,
        LlmDirection::Read,
        b"HTTP/1.1 100 Continue\r\n\r\n",
        None,
    ));
    out.extend(send(&mut p, LlmDirection::Write, body, None));
    for r in &h1_response(3000) {
        out.extend(send(&mut p, LlmDirection::Read, r, None));
    }
    assert!(unparsed(&out).is_empty(), "{out:?}");
    assert_eq!(out.last(), Some(&Outcome::Recorded), "{out:?}");
}

#[test]
fn test_write_before_whole_response_keeps_streaming_response() {
    let mut p = StreamProcessor::new();
    let mut out = send(&mut p, LlmDirection::Write, REQUEST, None);
    for r in [
        &b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n"[..],
        b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
    ] {
        out.extend(send(&mut p, LlmDirection::Read, r, None));
    }
    out.extend(send(&mut p, LlmDirection::Write, MESSAGES_REQUEST, None));
    let tail = b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
                 data: {\"model\":\"gpt-4o\",\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":4}}\n\n\
                 data: [DONE]\n\n";
    out.extend(send(&mut p, LlmDirection::Read, tail, None));
    assert!(unparsed(&out).is_empty(), "{out:?}");
    assert_eq!(out.last(), Some(&Outcome::Recorded), "{out:?}");
}

#[test]
fn test_unparsed_response_after_100_continue_frees_next_request() {
    let mut p = StreamProcessor::new();
    let (head, body) = REQUEST.split_at(REQUEST.len() - 27);
    let mut out = send(&mut p, LlmDirection::Write, head, None);
    out.extend(send(
        &mut p,
        LlmDirection::Read,
        b"HTTP/1.1 100 Continue\r\n\r\n",
        None,
    ));
    out.extend(send(&mut p, LlmDirection::Write, body, None));
    let event =
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
    let chunked = format!("{:x}\r\n{event}\r\n0\r\n\r\n", event.len());
    for r in [
        &b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n"[..],
        chunked.as_bytes(),
    ] {
        out.extend(send(&mut p, LlmDirection::Read, r, None));
    }
    out.extend(exchange(
        &mut p,
        MESSAGES_REQUEST,
        &h1("application/json", MESSAGES_RESPONSE),
    ));
    assert_eq!(unparsed(&out), [UnparsedReason::NextRequest], "{out:?}");
    assert_eq!(out.last(), Some(&Outcome::Recorded), "{out:?}");
}

#[test]
fn test_streamed_response_after_100_continue_is_recorded() {
    let mut p = StreamProcessor::new();
    let (head, body) = REQUEST.split_at(REQUEST.len() - 27);
    let mut out = send(&mut p, LlmDirection::Write, head, None);
    out.extend(send(
        &mut p,
        LlmDirection::Read,
        b"HTTP/1.1 100 Continue\r\n\r\n",
        None,
    ));
    out.extend(send(&mut p, LlmDirection::Write, body, None));
    let event = "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":4}}\n\ndata: [DONE]\n\n";
    let chunked = format!("{:x}\r\n{event}\r\n0\r\n\r\n", event.len());
    for r in [
        &b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n"[..],
        chunked.as_bytes(),
    ] {
        out.extend(send(&mut p, LlmDirection::Read, r, None));
    }
    assert!(unparsed(&out).is_empty(), "{out:?}");
    assert_eq!(out.last(), Some(&Outcome::Recorded), "{out:?}");
}

#[test]
fn test_whole_response_in_one_read_is_recorded() {
    let sse = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n\
                data: {\"model\":\"gpt-4o\",\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":4}}\n\n\
                data: [DONE]\n\n";
    for response in [h1("application/json", MESSAGES_RESPONSE), sse.to_vec()] {
        let mut p = StreamProcessor::new();
        let mut out = Vec::new();
        for _ in 0..2 {
            out.extend(send(&mut p, LlmDirection::Write, MESSAGES_REQUEST, None));
            out.extend(send(&mut p, LlmDirection::Read, &response, None));
        }
        assert_eq!(
            out,
            [
                Outcome::Pending,
                Outcome::Recorded,
                Outcome::Pending,
                Outcome::Recorded
            ]
        );
    }
}

#[test]
fn test_head_with_part_of_body_in_one_read_is_recorded_once() {
    let response = h1("application/json", MESSAGES_RESPONSE);
    let (first, rest) = response.split_at(response.len() - 20);
    let mut p = StreamProcessor::new();
    let mut out = send(&mut p, LlmDirection::Write, MESSAGES_REQUEST, None);
    out.extend(send(&mut p, LlmDirection::Read, first, None));
    out.extend(send(&mut p, LlmDirection::Read, rest, None));
    assert_eq!(out, [Outcome::Pending, Outcome::Pending, Outcome::Recorded]);
}

#[test]
fn test_chunked_response_with_trailers_frees_next_request() {
    let mut p = StreamProcessor::new();
    let error = b"HTTP/1.1 500 Internal Server Error\r\nContent-Type: text/plain\r\n\
                  Transfer-Encoding: chunked\r\nTrailer: X-Request-Id\r\n\r\n\
                  5\r\noops!\r\n0\r\nX-Request-Id: r1\r\n\r\n";
    let mut out = exchange(&mut p, MESSAGES_REQUEST, error);
    out.extend(exchange(
        &mut p,
        MESSAGES_REQUEST,
        &h1("application/json", MESSAGES_RESPONSE),
    ));
    assert_eq!(unparsed(&out), [UnparsedReason::NextRequest], "{out:?}");
    assert_eq!(out.last(), Some(&Outcome::Recorded), "{out:?}");
}

#[test]
fn test_chunk_larger_than_response_buffer_frees_next_request() {
    for size in ["2000000", "ffffffffffffffff"] {
        let mut p = StreamProcessor::new();
        let garbled = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n\
             {size}\r\n{{}}\r\n0\r\n\r\n"
        );
        let mut out = exchange(&mut p, MESSAGES_REQUEST, garbled.as_bytes());
        out.extend(exchange(
            &mut p,
            MESSAGES_REQUEST,
            &h1("application/json", MESSAGES_RESPONSE),
        ));
        assert_eq!(
            unparsed(&out),
            [UnparsedReason::NextRequest],
            "{size}: {out:?}"
        );
        assert_eq!(out.last(), Some(&Outcome::Recorded), "{size}: {out:?}");
    }
}

/// `h1_response` grown or shrunk to exactly `total` bytes.
fn h1_response_of_size(total: usize) -> Vec<Vec<u8>> {
    let mut body_len = total;
    loop {
        let reads = h1_response(body_len);
        let len: usize = reads.iter().map(Vec::len).sum();
        if len == total {
            return reads;
        }
        body_len = body_len + total - len;
    }
}

/// A chat completion with `before` bytes of text ahead of its usage and `after` bytes behind it.
fn chat_json(before: usize, after: usize) -> Vec<u8> {
    let (x, y) = ("x".repeat(before), "y".repeat(after));
    format!(r#"{{"model":"gpt-4o","choices":[{{"message":{{"content":"{x}"}}}}],"usage":{{"prompt_tokens":11,"completion_tokens":4}},"id":"{y}"}}"#).into_bytes()
}

/// The response in 16 KB reads; chunked in 8191-byte chunks, so chunks and reads never line up.
fn h1_reads(headers: &str, body: &[u8]) -> Vec<Vec<u8>> {
    typed_reads("application/json", headers, body)
}

fn typed_reads(content_type: &str, headers: &str, body: &[u8]) -> Vec<Vec<u8>> {
    let headers = headers.replace("{len}", &body.len().to_string());
    let mut framed = Vec::with_capacity(body.len() * 2);
    if headers.contains("chunked") {
        for c in body.chunks(8191) {
            framed.extend_from_slice(format!("{:x}\r\n", c.len()).as_bytes());
            framed.extend_from_slice(c);
            framed.extend_from_slice(b"\r\n");
        }
        framed.extend_from_slice(b"0\r\n\r\n");
    } else {
        framed.extend_from_slice(body);
    }
    let head = format!("HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\n{headers}\r\n\r\n");
    let mut reads = vec![head.into_bytes()];
    reads.extend(framed.chunks(16384).map(<[u8]>::to_vec));
    reads
}

#[test]
fn test_response_over_buffer_limit_without_usage_at_its_ends_is_unparsed() {
    let size = MAX_RESPONSE_BUFFER_SIZE + (1 << 20);
    let middle = chat_json(size / 2, size / 2);
    // Never decoded: an encoded body is dropped as soon as it passes the limit.
    let encoded = chat_json(size, 0);
    for (n, reads) in [
        h1_reads("Content-Length: {len}", &middle),
        h1_reads("Transfer-Encoding: chunked", &middle),
        h1_reads("Content-Encoding: gzip\r\nContent-Length: {len}", &encoded),
        h1_reads("Connection: close", &encoded),
        typed_reads(
            "text/event-stream",
            "Content-Encoding: gzip\r\nTransfer-Encoding: chunked",
            &openai_sse_body(70_000),
        ),
    ]
    .iter()
    .enumerate()
    {
        assert!(reads.iter().map(Vec::len).sum::<usize>() > MAX_RESPONSE_BUFFER_SIZE);
        let mut p = StreamProcessor::new();
        let mut out = outcomes(&mut p, reads, None);
        out.extend(outcomes(&mut p, &h1_response(3000), None));
        assert_eq!(unparsed(&out), [UnparsedReason::ResponseTooLarge], "{n}");
        assert_eq!(count(&out, Outcome::Recorded), 1, "{n}");
        assert_eq!(out.last(), Some(&Outcome::Recorded), "{n}");
    }
}

#[test]
fn test_json_response_over_buffer_limit_is_recorded() {
    let mut cases = vec![
        h1_response_of_size(MAX_RESPONSE_BUFFER_SIZE),
        h1_response_of_size(MAX_RESPONSE_BUFFER_SIZE + 1),
    ];
    for body_len in [20 << 20, 40 << 20] {
        let body = chat_json(body_len, 0);
        cases.push(h1_reads("Content-Length: {len}", &body));
        cases.push(h1_reads("Transfer-Encoding: chunked", &body));
    }
    for reads in cases {
        let total: usize = reads.iter().map(Vec::len).sum();
        let mut p = StreamProcessor::new();
        let out = outcomes(&mut p, &reads, None);
        assert!(unparsed(&out).is_empty(), "{total}: {out:?}");
        assert_eq!(count(&out, Outcome::Recorded), 1, "{total}");
        assert_eq!(out.last(), Some(&Outcome::Recorded), "{total}");
        // Past the limit only the two ends are kept, whatever the size.
        assert!(total <= MAX_RESPONSE_BUFFER_SIZE || p.read_buf.capacity() == 0);

        let next = outcomes(&mut p, &h1_response(3000), None);
        assert_eq!(next.last(), Some(&Outcome::Recorded), "{total}: {next:?}");
    }
}

#[test]
fn test_json_response_over_buffer_limit_missing_a_read_ends_at_the_next_request() {
    // A whole read lost before the ringbuf (no gap seen): the length walk never reaches 0 on its own.
    let mut reads = h1_reads("Content-Length: {len}", &chat_json(20 << 20, 0));
    reads.remove(reads.len() - 10);
    let mut p = StreamProcessor::new();
    let mut out = outcomes(&mut p, &reads, None);
    out.extend(outcomes(&mut p, &h1_response(3000), None));
    out.extend(outcomes(&mut p, &h1_response(3000), None));
    assert_eq!(unparsed(&out), [UnparsedReason::NextRequest], "{out:?}");
    assert_eq!(count(&out, Outcome::Recorded), 2);
}

#[test]
fn test_request_over_buffer_limit_is_unparsed() {
    let chunk = [b' '; MAX_SSL_BUF_SIZE];
    let too_large = [UnparsedReason::RequestTooLarge];
    for (body, want) in [
        (MAX_REQUEST_BUFFER_SIZE - REQUEST.len(), &[][..]),
        (MAX_REQUEST_BUFFER_SIZE - REQUEST.len() + 1, &too_large),
        (MAX_REQUEST_BUFFER_SIZE + 100_000, &too_large),
    ] {
        let mut p = StreamProcessor::new();
        let mut out = send(&mut p, LlmDirection::Write, REQUEST, None);
        let mut left = body;
        while left > 0 {
            let n = left.min(chunk.len());
            out.extend(send(&mut p, LlmDirection::Write, &chunk[..n], None));
            left -= n;
        }
        for r in &h1_response(3000) {
            out.extend(send(&mut p, LlmDirection::Read, r, None));
        }
        assert_eq!(unparsed(&out), want, "{body}");
        assert_eq!(count(&out, Outcome::Recorded), 1 - want.len(), "{body}");

        let next = outcomes(&mut p, &h1_response(3000), None);
        assert_eq!(next.last(), Some(&Outcome::Recorded), "{body}: {next:?}");
    }
}

/// A chunked OpenAI chat stream as servers flush it: one event per read, then usage and `[DONE]`.
fn openai_sse_reads(events: usize) -> Vec<Vec<u8>> {
    let chunk = |ev: &str| format!("{:x}\r\n{ev}\r\n", ev.len()).into_bytes();
    let mut reads = vec![
        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream; charset=utf-8\r\nTransfer-Encoding: chunked\r\n\r\n"
            .to_vec(),
    ];
    for i in 0..events {
        reads.push(chunk(&format!(
            "data: {{\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"model\":\"gpt-4o\",\"choices\":[{{\"index\":0,\
             \"delta\":{{\"content\":\" tok{i}\"}},\"logprobs\":null,\"finish_reason\":null}}],\"usage\":null}}\n\n"
        )));
    }
    reads.push(chunk(
        "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\
         \"delta\":{},\"logprobs\":null,\"finish_reason\":\"stop\"}],\"usage\":null}\n\n",
    ));
    let usage = format!(
        "data: {{\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"model\":\"gpt-4o\",\"choices\":[],\
         \"usage\":{{\"prompt_tokens\":11,\"completion_tokens\":{events}}}}}\n\ndata: [DONE]\n\n"
    );
    let mut last = chunk(&usage);
    last.extend_from_slice(b"0\r\n\r\n");
    reads.push(last);
    reads
}

/// An OpenAI chat stream of ~300-byte deltas whose text looks like stream ends and usage.
fn openai_sse_body(events: usize) -> Vec<u8> {
    let mut body = Vec::with_capacity(events * 320);
    for i in 0..events {
        body.extend_from_slice(format!(
            "data: {{\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-4o\",\
             \"system_fingerprint\":\"fp\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\" t{i} data: [DONE] \
             \\\"usage\\\":{{\\\"prompt_tokens\\\":99}}\"}},\"logprobs\":null,\"finish_reason\":null}}],\"usage\":null}}\n\n"
        ).as_bytes());
    }
    body.extend_from_slice(format!(
        "data: {{\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"model\":\"gpt-4o\",\"choices\":[],\
         \"usage\":{{\"prompt_tokens\":11,\"completion_tokens\":{events}}}}}\n\ndata: [DONE]\n\n"
    ).as_bytes());
    body
}

/// An SSE response with a Content-Length that is exactly `total` bytes, padded by a comment line.
fn sse_reads_of_size(total: usize) -> Vec<Vec<u8>> {
    let body = openai_sse_body(total / 330);
    let base: usize = typed_reads("text/event-stream", "Content-Length: {len}", &body)
        .iter()
        .map(Vec::len)
        .sum();
    let padded = [b": ", &b"x".repeat(total - base - 3)[..], b"\n", &body].concat();
    typed_reads("text/event-stream", "Content-Length: {len}", &padded)
}

#[test]
fn test_sse_response_over_buffer_limit_is_recorded() {
    let body = openai_sse_body(70_000);
    let mut cases = vec![
        sse_reads_of_size(MAX_RESPONSE_BUFFER_SIZE),
        sse_reads_of_size(MAX_RESPONSE_BUFFER_SIZE + 1),
        openai_sse_reads(100_000),
    ];
    for headers in [
        "Transfer-Encoding: chunked",
        "Content-Length: {len}",
        "Connection: close",
    ] {
        cases.push(typed_reads("text/event-stream", headers, &body));
    }
    let mut continued = typed_reads("text/event-stream", "Transfer-Encoding: chunked", &body);
    continued[0].splice(0..0, *b"HTTP/1.1 100 Continue\r\n\r\n");
    cases.push(continued);
    for (n, reads) in cases.iter().enumerate() {
        let total: usize = reads.iter().map(Vec::len).sum();
        let mut p = StreamProcessor::new();
        let out = outcomes(&mut p, reads, None);
        assert!(unparsed(&out).is_empty(), "{n} {total}");
        assert_eq!(count(&out, Outcome::Recorded), 1, "{n} {total}");
        assert!(total <= MAX_RESPONSE_BUFFER_SIZE || p.read_buf.capacity() == 0);

        let next = outcomes(&mut p, &h1_response(3000), None);
        assert_eq!(next.last(), Some(&Outcome::Recorded), "{n}: {next:?}");
    }
}

#[test]
fn test_sse_response_over_buffer_limit_without_usage_is_unparsed_at_the_next_request() {
    let mut body = String::from_utf8(openai_sse_body(70_000)).expect("UTF-8");
    body.truncate(body.rfind("data: {").expect("usage event"));
    let reads = typed_reads(
        "text/event-stream",
        "Content-Length: {len}",
        body.as_bytes(),
    );
    let mut p = StreamProcessor::new();
    let mut out = outcomes(&mut p, &reads, None);
    assert!(out.iter().all(|o| *o == Outcome::Pending));
    out.extend(outcomes(&mut p, &h1_response(3000), None));
    assert_eq!(unparsed(&out), [UnparsedReason::NextRequest]);
    assert_eq!(out.last(), Some(&Outcome::Recorded));
}

#[test]
fn test_sse_line_cut_by_the_buffer_limit_is_not_kept_twice() {
    // Ends past the limit, so it is unfinished at the takeover, yet stays under the line limit.
    let line = format!("data: {}\n\n", "x".repeat(MAX_RESPONSE_BUFFER_SIZE - 50));
    let body = [line.as_bytes(), &openai_sse_body(10)].concat();
    let reads = typed_reads("text/event-stream", "Content-Length: {len}", &body);
    let mut p = StreamProcessor::new();
    let out = outcomes(&mut p, &reads, None);
    assert_eq!(count(&out, Outcome::Recorded), 1, "{out:?}");
    assert_eq!(p.read_progress.held_line_len(), 0);
}

#[test]
fn test_sse_response_over_buffer_limit_missing_a_read_is_unparsed_once() {
    // A whole read lost before the ringbuf: the chunk walk loses its place.
    let mut reads = typed_reads(
        "text/event-stream",
        "Transfer-Encoding: chunked",
        &openai_sse_body(70_000),
    );
    reads.remove(reads.len() - 10);
    let mut p = StreamProcessor::new();
    let out = outcomes(&mut p, &reads, None);
    assert_eq!(count(&out, Outcome::Recorded), 0);
    let mut next = outcomes(&mut p, &h1_response(3000), None);
    next.extend(outcomes(&mut p, &h1_response(3000), None));
    assert_eq!(unparsed(&out).len() + unparsed(&next).len(), 1, "{next:?}");
    assert_eq!(count(&next, Outcome::Recorded), 2);
}

#[test]
fn test_stream_after_a_lost_fragment_is_parsed_from_its_own_start() {
    let mut p = StreamProcessor::new();
    let mut lossy = openai_sse_reads(50);
    // One event over 4 KB arrives in two fragments; the second is lost after 10 KB were parsed.
    let event = format!("data: {}\n\n", "x".repeat(6000));
    lossy[40] = format!("{:x}\r\n{event}\r\n", event.len()).into_bytes();
    let short = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":4}}\n\ndata: [DONE]\n\n";
    let mut out = outcomes(&mut p, &lossy, Some((40, 1)));
    out.extend(outcomes(&mut p, &[short.to_vec()], None));
    assert_eq!(unparsed(&out), [UnparsedReason::CaptureLost], "{out:?}");
    assert_eq!(out.last(), Some(&Outcome::Recorded), "{out:?}");
}

#[test]
fn test_call_after_a_lost_response_head_read_is_recorded() {
    // Whole SSL_reads lost in the kernel leave no gap in the fragment offsets.
    let mut short_first_read = h1_response(3000);
    let head = short_first_read.remove(0);
    short_first_read.splice(0..0, [head[..5].to_vec(), head[5..].to_vec()]);
    for (n, (mut reads, lost)) in [
        (h1_response(3000), 0),
        (h1_response(40000), 0),
        (openai_sse_reads(5), 0),
        (vec![h1("application/json", MESSAGES_RESPONSE)], 0),
        // `HTTP/` alone passes as a status line; the request head after it shows the loss.
        (short_first_read, 1),
    ]
    .into_iter()
    .enumerate()
    {
        reads.remove(lost);
        let mut p = StreamProcessor::new();
        let mut out = outcomes(&mut p, &h1_response(3000), None);
        out.extend(outcomes(&mut p, &reads, None));
        out.extend(outcomes(&mut p, &h1_response(3000), None));
        assert_eq!(
            unparsed(&out),
            [UnparsedReason::CaptureLost],
            "{n}: {out:?}"
        );
        assert_eq!(count(&out, Outcome::Recorded), 2, "{n}: {out:?}");
        assert_eq!(out.last(), Some(&Outcome::Recorded), "{n}: {out:?}");
    }
}

#[test]
fn test_call_after_a_lost_last_response_read_is_recorded() {
    let sse = openai_sse_reads(5);
    let json = h1_response(40000);
    for (n, (reads, lost)) in [
        (sse.clone(), 1),
        (sse.clone(), 2),
        (json.clone(), 1),
        (json.clone(), 2),
    ]
    .into_iter()
    .enumerate()
    {
        let kept = &reads[..reads.len() - lost];
        let mut p = StreamProcessor::new();
        let mut out = outcomes(&mut p, &sse, None);
        out.extend(outcomes(&mut p, kept, None));
        out.extend(outcomes(&mut p, &sse, None));
        out.extend(outcomes(&mut p, &json, None));
        assert_eq!(
            unparsed(&out),
            [UnparsedReason::CaptureLost],
            "{n}: {out:?}"
        );
        assert_eq!(count(&out, Outcome::Recorded), 3, "{n}: {out:?}");
        assert_eq!(out.last(), Some(&Outcome::Recorded), "{n}: {out:?}");
    }
}

#[test]
fn test_newest_request_is_resumed_after_two_lost_responses() {
    let sse = openai_sse_reads(5);
    let mut p = StreamProcessor::new();
    let mut out = outcomes(&mut p, &sse[..sse.len() - 1], None);
    // The second request's whole response is lost too.
    out.extend(send(&mut p, LlmDirection::Write, MESSAGES_REQUEST, None));
    out.extend(outcomes(&mut p, &sse, None));
    out.extend(outcomes(&mut p, &h1_response(3000), None));
    assert_eq!(
        unparsed(&out),
        [UnparsedReason::CaptureLost, UnparsedReason::CaptureLost],
        "{out:?}"
    );
    assert_eq!(count(&out, Outcome::Recorded), 2, "{out:?}");
    assert_eq!(out.last(), Some(&Outcome::Recorded), "{out:?}");
}

#[test]
fn test_truncated_status_line_read_after_a_lost_tail_is_not_parsed() {
    let sse = openai_sse_reads(5);
    let mut p = StreamProcessor::new();
    let mut out = outcomes(&mut p, &sse[..sse.len() - 1], None);
    // One SSL_read over the 16 KB capture limit: its tail is missing, so it cannot be framed.
    out.extend(send(&mut p, LlmDirection::Write, REQUEST, None));
    out.extend(send(
        &mut p,
        LlmDirection::Read,
        &h1_response(40000).concat(),
        None,
    ));
    out.extend(outcomes(&mut p, &h1_response(3000), None));
    assert_eq!(unparsed(&out), [UnparsedReason::CaptureLost], "{out:?}");
    assert_eq!(count(&out, Outcome::Recorded), 1, "{out:?}");
    assert_eq!(out.last(), Some(&Outcome::Recorded), "{out:?}");
}

#[test]
fn test_non_llm_request_after_a_lost_last_read_is_skipped() {
    let sse = openai_sse_reads(5);
    let count_tokens = b"POST /v1/messages/count_tokens HTTP/1.1\r\nHost: api.anthropic.com\r\nContent-Type: application/json\r\n\r\n{\"model\":\"claude-opus-5-5\",\"messages\":[]}";
    let mut p = StreamProcessor::new();
    let mut out = outcomes(&mut p, &sse[..sse.len() - 1], None);
    out.extend(exchange(
        &mut p,
        count_tokens,
        &h1("application/json", r#"{"input_tokens":14}"#),
    ));
    out.extend(exchange(
        &mut p,
        MESSAGES_REQUEST,
        &h1("application/json", MESSAGES_RESPONSE),
    ));
    assert_eq!(unparsed(&out), [UnparsedReason::CaptureLost], "{out:?}");
    assert_eq!(count(&out, Outcome::Recorded), 1, "{out:?}");
    assert_eq!(out.last(), Some(&Outcome::Recorded), "{out:?}");
}

#[test]
fn test_read_starting_with_status_line_text_mid_stream_is_recorded() {
    let mut reads = openai_sse_reads(5);
    let ev = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"HTTP/1.1 200 OK\"}}]}\n\n";
    let chunk = format!("{:x}\r\n{ev}\r\n", ev.len()).into_bytes();
    let Some(at) = chunk.windows(7).position(|w| w == b"HTTP/1.") else {
        panic!("the event carries the status-line text");
    };
    reads.splice(3..3, [chunk[..at].to_vec(), chunk[at..].to_vec()]);
    // Also with the next request written first, as a pipelining client does.
    for pipelined in [false, true] {
        let mut p = StreamProcessor::new();
        let mut out = send(&mut p, LlmDirection::Write, REQUEST, None);
        for (n, r) in reads.iter().enumerate() {
            if pipelined && n == 4 {
                out.extend(send(&mut p, LlmDirection::Write, MESSAGES_REQUEST, None));
            }
            out.extend(send(&mut p, LlmDirection::Read, r, None));
        }
        assert!(unparsed(&out).is_empty(), "{pipelined}: {out:?}");
        assert_eq!(out.last(), Some(&Outcome::Recorded), "{pipelined}: {out:?}");
    }
}

#[test]
fn test_first_read_of_partial_status_line_is_recorded() {
    for split in 1..=8 {
        let mut reads = h1_response(3000);
        let head = reads.remove(0);
        reads.splice(0..0, [head[..split].to_vec(), head[split..].to_vec()]);
        let mut p = StreamProcessor::new();
        let out = outcomes(&mut p, &reads, None);
        assert!(unparsed(&out).is_empty(), "{split}: {out:?}");
        assert_eq!(out.last(), Some(&Outcome::Recorded), "{split}: {out:?}");
    }
}

#[test]
fn test_response_after_empty_lines_is_recorded() {
    let ok = h1("application/json", MESSAGES_RESPONSE);
    for lead in [&b"\r\n"[..], b"\n", b"\r\n\r\n"] {
        let mut p = StreamProcessor::new();
        let mut out = send(&mut p, LlmDirection::Write, MESSAGES_REQUEST, None);
        out.extend(send(
            &mut p,
            LlmDirection::Read,
            &[lead, &ok].concat(),
            None,
        ));
        // The empty line in a read of its own, as a server that ends the previous response with it.
        out.extend(send(&mut p, LlmDirection::Write, MESSAGES_REQUEST, None));
        out.extend(send(&mut p, LlmDirection::Read, lead, None));
        out.extend(send(&mut p, LlmDirection::Read, &ok, None));
        assert!(unparsed(&out).is_empty(), "{lead:?}: {out:?}");
        assert_eq!(count(&out, Outcome::Recorded), 2, "{lead:?}: {out:?}");
    }
}

use crate::probes::builtin::llm::http::test_support::{es_chunk, es_event, es_message};

const CONVERSE_STREAM_REQUEST: &[u8] = b"POST /model/anthropic.claude-sonnet-5-5-v1%3A0/converse-stream HTTP/1.1\r\nHost: bedrock-runtime.us-east-1.amazonaws.com\r\nContent-Type: application/json\r\n\r\n{\"messages\":[{\"role\":\"user\",\"content\":[{\"text\":\"hi\"}]}]}";
const INVOKE_STREAM_REQUEST: &[u8] = b"POST /model/amazon.titan-text-express-v1/invoke-with-response-stream HTTP/1.1\r\nHost: bedrock-runtime.us-east-1.amazonaws.com\r\nContent-Type: application/json\r\n\r\n{\"inputText\":\"hi\"}";

/// A Bedrock stream in reads: the head, then one message per chunk and read, `last` closing the body.
fn bedrock_stream_reads(deltas: usize, delta_len: usize, last: &[u8]) -> Vec<Vec<u8>> {
    let chunk = |m: &[u8]| [format!("{:x}\r\n", m.len()).as_bytes(), m, b"\r\n"].concat();
    let mut reads = vec![b"HTTP/1.1 200 OK\r\nContent-Type: application/vnd.amazon.eventstream\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec()];
    let delta = format!(
        r#"{{"contentBlockIndex":0,"delta":{{"text":"{}"}}}}"#,
        "x".repeat(delta_len)
    );
    reads.extend((0..deltas).map(|_| chunk(&es_event("contentBlockDelta", &delta))));
    reads.push([chunk(last), b"0\r\n\r\n".to_vec()].concat());
    reads
}

fn converse_metadata() -> Vec<u8> {
    es_event(
        "metadata",
        r#"{"usage":{"inputTokens":30,"outputTokens":9,"totalTokens":39}}"#,
    )
}

fn bedrock_call(p: &mut StreamProcessor, request: &[u8], reads: &[Vec<u8>]) -> Vec<Outcome> {
    let mut out = send(p, LlmDirection::Write, request, None);
    for r in reads {
        out.extend(send(p, LlmDirection::Read, r, None));
    }
    out
}

#[test]
fn test_bedrock_streams_after_count_tokens_are_recorded() {
    let count_tokens = b"POST /model/anthropic.claude-sonnet-5-5-v1%3A0/count-tokens HTTP/1.1\r\nHost: bedrock-runtime.us-east-1.amazonaws.com\r\nContent-Type: application/json\r\n\r\n{\"input\":{}}";
    let mut p = StreamProcessor::new();
    let mut out = exchange(
        &mut p,
        count_tokens,
        &h1("application/json", r#"{"inputTokens":14}"#),
    );
    out.extend(bedrock_call(
        &mut p,
        CONVERSE_STREAM_REQUEST,
        &bedrock_stream_reads(5, 20, &converse_metadata()),
    ));
    let metrics = es_chunk(
        r#"{"outputText":"","completionReason":"FINISH","amazon-bedrock-invocationMetrics":{"inputTokenCount":12,"outputTokenCount":40}}"#,
    );
    out.extend(bedrock_call(
        &mut p,
        INVOKE_STREAM_REQUEST,
        &bedrock_stream_reads(5, 20, &metrics),
    ));
    // A throttled stream is a failed call, and the connection goes on.
    let throttled = es_message(
        &[
            (":exception-type", "throttlingException"),
            (":message-type", "exception"),
        ],
        br#"{"message":"Too many requests"}"#,
    );
    out.extend(bedrock_call(
        &mut p,
        INVOKE_STREAM_REQUEST,
        &bedrock_stream_reads(2, 20, &throttled),
    ));
    out.extend(bedrock_call(
        &mut p,
        CONVERSE_STREAM_REQUEST,
        &bedrock_stream_reads(0, 0, &converse_metadata()),
    ));
    assert!(unparsed(&out).is_empty(), "{out:?}");
    assert_eq!(count(&out, Outcome::Recorded), 4, "{out:?}");
}

#[test]
fn test_bedrock_stream_over_buffer_limit_is_unparsed() {
    // ~16 KB messages, one per read as the server flushes them, until the stream passes 16 MB.
    let reads = bedrock_stream_reads(1050, 16_000, &converse_metadata());
    assert!(reads.iter().map(Vec::len).sum::<usize>() > MAX_RESPONSE_BUFFER_SIZE);
    let mut p = StreamProcessor::new();
    let mut out = bedrock_call(&mut p, CONVERSE_STREAM_REQUEST, &reads);
    out.extend(bedrock_call(
        &mut p,
        CONVERSE_STREAM_REQUEST,
        &bedrock_stream_reads(1, 20, &converse_metadata()),
    ));
    assert_eq!(
        unparsed(&out),
        [UnparsedReason::ResponseTooLarge],
        "{out:?}"
    );
    assert_eq!(out.last(), Some(&Outcome::Recorded), "{out:?}");
}

#[test]
fn test_bedrock_stream_missing_its_last_read_resyncs_on_the_next_response() {
    let reads = bedrock_stream_reads(5, 20, &converse_metadata());
    let mut p = StreamProcessor::new();
    let mut out = bedrock_call(&mut p, CONVERSE_STREAM_REQUEST, &reads[..reads.len() - 1]);
    out.extend(bedrock_call(&mut p, CONVERSE_STREAM_REQUEST, &reads));
    assert_eq!(unparsed(&out), [UnparsedReason::CaptureLost], "{out:?}");
    assert_eq!(count(&out, Outcome::Recorded), 1, "{out:?}");
    assert_eq!(out.last(), Some(&Outcome::Recorded), "{out:?}");
}

fn fastest(mut run: impl FnMut()) -> std::time::Duration {
    (0..3)
        .map(|_| {
            let start = Instant::now();
            run();
            start.elapsed()
        })
        .min()
        .unwrap_or_default()
}

#[test]
fn test_long_sse_stream_costs_about_one_parse_of_its_bytes() {
    // Parsing the whole buffer again on every read would cost ~reads/2 parses of the final buffer.
    let reads = openai_sse_reads(1000);
    let whole = reads.concat();
    let streamed = fastest(|| {
        let mut p = StreamProcessor::new();
        for _ in 0..2 {
            let out = outcomes(&mut p, &reads, None);
            assert_eq!(count(&out, Outcome::Recorded), 1, "{out:?}");
            assert_eq!(out.last(), Some(&Outcome::Recorded), "{out:?}");
        }
    });
    let parsed_once = fastest(|| {
        assert!(http::Http11Parser.parse_response(&whole).is_some());
    });
    assert!(
        streamed < parsed_once * 40,
        "two streams took {streamed:?}, one parse of the whole response {parsed_once:?}"
    );
}

thread_local! {
    static LOGS: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
}

/// Keeps every record at every level, per thread so parallel tests don't mix their output.
struct CaptureLogger;

impl log::Log for CaptureLogger {
    fn enabled(&self, _: &log::Metadata) -> bool {
        true
    }

    fn log(&self, r: &log::Record) {
        let line = format!("{} {}\n", r.level(), r.args());
        LOGS.with(|logs| logs.borrow_mut().push_str(&line));
    }

    fn flush(&self) {}
}

static LOGGER: CaptureLogger = CaptureLogger;

#[test]
fn test_credentials_stay_out_of_logs() {
    log::set_logger(&LOGGER).expect("no other logger in the test binary");
    log::set_max_level(log::LevelFilter::Trace);
    // Fake keys carry HBTEST; the query and header forms are how Gemini and SigV4 send them.
    let gemini = "AIzaSyHBTEST0gemini0000000000000000000";
    let presigned = "X-Amz-Credential=AKIAHBTEST0000000000%2F20261023%2Fus-east-1%2Fbedrock%2Faws4_request\
                     &X-Amz-Security-Token=HBTEST0token&X-Amz-Signature=HBTEST0signature";
    let openai = "sk-proj-HBTEST0openai00000000000000000000000000";
    let requests = [
        format!(
            "POST /v1beta/models/gemini-2.5-flash:generateContent?key={gemini} HTTP/1.1\r\n\
                 Host: generativelanguage.googleapis.com\r\nx-goog-api-key: {gemini}\r\n\r\n{{}}"
        ),
        format!(
            "POST /model/anthropic.claude-sonnet-5-5-v1:0/converse?{presigned} HTTP/1.1\r\n\
                 Host: bedrock-runtime.us-east-1.amazonaws.com\r\n\r\n{{}}"
        ),
        format!(
            "POST /v1/chat/completions HTTP/1.1\r\nHost: api.openai.com\r\nAuthorization: Bearer {openai}\r\n\r\n\
                 {{\"messages\":[{{\"role\":\"user\",\"content\":\"{openai}\"}}]}}"
        ),
    ];
    let response = h1(
        "application/json",
        r#"{"model":"m","usage":{"prompt_tokens":1,"completion_tokens":1}}"#,
    );
    for request in &requests {
        LOGS.with(|logs| logs.borrow_mut().clear());
        let out = exchange(&mut StreamProcessor::new(), request.as_bytes(), &response);
        let logs = LOGS.with(std::cell::RefCell::take);
        assert_eq!(out.last(), Some(&Outcome::Recorded), "{out:?}");
        assert!(logs.contains("[LLM] Detected"), "{logs}");
        assert!(!logs.contains("HBTEST"), "{logs}");
    }
}

const BG_CREATE: &[u8] = b"POST /v1/responses HTTP/1.1\r\nHost: api.openai.com\r\nContent-Type: application/json\r\n\r\n{\"model\":\"o3-pro\",\"background\":true,\"input\":\"hi\"}";
const BG_POLL: &[u8] = b"GET /v1/responses/resp_bg1 HTTP/1.1\r\nHost: api.openai.com\r\n\r\n";
const BG_RESUME: &[u8] =
    b"GET /v1/responses/resp_bg1?stream=true&starting_after=2 HTTP/1.1\r\nHost: api.openai.com\r\n\r\n";
const BG_USAGE: &str = r#"{"input_tokens":40,"input_tokens_details":{"cached_tokens":0},"output_tokens":120,"output_tokens_details":{"reasoning_tokens":0},"total_tokens":160}"#;

/// Background call `resp_bg1` at `status`, as a create or poll returns it.
fn bg_object(status: &str, usage: &str) -> String {
    format!(
        r#"{{"id":"resp_bg1","object":"response","model":"o3-pro","status":"{status}","background":true,"error":null,"usage":{usage}}}"#
    )
}

fn bg_response(status: &str, usage: &str) -> Vec<u8> {
    h1("application/json", &bg_object(status, usage))
}

/// A started background call on its own connection.
fn bg_created(bg: &mut BackgroundCalls) -> Vec<Outcome> {
    let out = exchange_with(
        &mut StreamProcessor::new(),
        bg,
        BG_CREATE,
        &bg_response("queued", "null"),
    );
    assert_eq!(out.last(), Some(&Outcome::Deferred), "{out:?}");
    out
}

#[test]
fn test_background_call_is_recorded_once_from_polls_on_any_connection() {
    let mut bg = BackgroundCalls::default();
    let mut out = bg_created(&mut bg);
    let (mut a, mut b) = (StreamProcessor::new(), StreamProcessor::new());
    out.extend(exchange_with(
        &mut b,
        &mut bg,
        BG_POLL,
        &bg_response("in_progress", "null"),
    ));
    out.extend(exchange_with(
        &mut a,
        &mut bg,
        BG_POLL,
        &bg_response("completed", BG_USAGE),
    ));
    // A poll after the result is not detected.
    out.extend(exchange_with(
        &mut b,
        &mut bg,
        BG_POLL,
        &bg_response("completed", BG_USAGE),
    ));
    assert_eq!(count(&out, Outcome::Recorded), 1, "{out:?}");
    assert!(unparsed(&out).is_empty(), "{out:?}");
    assert!(!bg.contains("resp_bg1"));
}

#[test]
fn test_two_polls_in_flight_record_the_result_once() {
    use LlmDirection::{Read, Write};
    let mut bg = BackgroundCalls::default();
    let mut out = bg_created(&mut bg);
    let (mut a, mut b) = (StreamProcessor::new(), StreamProcessor::new());
    let done = bg_response("completed", BG_USAGE);
    out.extend(send_with(&mut a, &mut bg, Write, BG_POLL, None));
    out.extend(send_with(&mut b, &mut bg, Write, BG_POLL, None));
    out.extend(send_with(&mut a, &mut bg, Read, &done, None));
    out.extend(send_with(&mut b, &mut bg, Read, &done, None));
    assert_eq!(count(&out, Outcome::Recorded), 1, "{out:?}");
}

#[test]
fn test_resumed_stream_records_the_background_call() {
    let in_progress = format!(
        "event: response.in_progress\ndata: {{\"type\":\"response.in_progress\",\"sequence_number\":3,\"response\":{}}}\n\n",
        bg_object("in_progress", "null")
    );
    let completed = format!(
        "event: response.completed\ndata: {{\"type\":\"response.completed\",\"sequence_number\":4,\"response\":{}}}\n\n",
        bg_object("completed", BG_USAGE)
    );
    let mut bg = BackgroundCalls::default();
    let mut out = bg_created(&mut bg);
    // Cut before the end: the call stays pending, nothing is counted.
    let mut cut = StreamProcessor::new();
    out.extend(send_with(
        &mut cut,
        &mut bg,
        LlmDirection::Write,
        BG_RESUME,
        None,
    ));
    let head = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n";
    out.extend(send_with(&mut cut, &mut bg, LlmDirection::Read, head, None));
    out.extend(send_with(
        &mut cut,
        &mut bg,
        LlmDirection::Read,
        in_progress.as_bytes(),
        None,
    ));
    out.push(cut.close(&ctx()));
    assert!(
        bg.contains("resp_bg1") && unparsed(&out).is_empty(),
        "{out:?}"
    );
    // The whole stream in one read.
    let stream = h1("text/event-stream", &format!("{in_progress}{completed}"));
    let mut resumed = StreamProcessor::new();
    out.extend(send_with(
        &mut resumed,
        &mut bg,
        LlmDirection::Write,
        BG_RESUME,
        None,
    ));
    out.extend(send_with(
        &mut resumed,
        &mut bg,
        LlmDirection::Read,
        &stream,
        None,
    ));
    assert_eq!(count(&out, Outcome::Recorded), 1, "{out:?}");
    assert!(!bg.contains("resp_bg1"));
}

#[test]
fn test_poll_error_keeps_the_call_and_cancel_without_usage_is_one_failed_call() {
    let mut bg = BackgroundCalls::default();
    let mut out = bg_created(&mut bg);
    let error = r#"{"error":{"message":"server error","type":"server_error"}}"#;
    let server_error = format!(
        "HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{error}",
        error.len()
    );
    let mut p = StreamProcessor::new();
    out.extend(exchange_with(
        &mut p,
        &mut bg,
        BG_POLL,
        server_error.as_bytes(),
    ));
    assert!(
        bg.contains("resp_bg1") && count(&out, Outcome::Recorded) == 0,
        "{out:?}"
    );
    out.extend(exchange_with(
        &mut p,
        &mut bg,
        BG_POLL,
        &bg_response("cancelled", "null"),
    ));
    assert_eq!(count(&out, Outcome::Recorded), 1, "{out:?}");
    assert!(
        unparsed(&out).is_empty() && !bg.contains("resp_bg1"),
        "{out:?}"
    );
    // Only a cancel may end without usage; other ends are counted, not recorded as 0 tokens.
    for status in ["completed", "incomplete"] {
        let mut bg = BackgroundCalls::default();
        bg_created(&mut bg);
        let out = exchange_with(&mut p, &mut bg, BG_POLL, &bg_response(status, "null"));
        assert_eq!(
            unparsed(&out),
            [UnparsedReason::BackgroundUnfinished],
            "{status}"
        );
        assert!(!bg.contains("resp_bg1"), "{status}");
    }
}

#[test]
fn test_poll_result_in_any_framing_is_recorded_on_the_creating_connection() {
    use std::io::Write as _;
    let completed = bg_object("completed", BG_USAGE);
    let sse = format!(
        "event: response.completed\ndata: {{\"type\":\"response.completed\",\"response\":{completed}}}\n\n"
    );
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gz.write_all(sse.as_bytes()).expect("gzip");
    let gz = gz.finish().expect("gzip");
    let mut gzip_sse = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\n\r\n",
        gz.len()
    )
    .into_bytes();
    gzip_sse.extend(gz);
    // Past the 16 MB buffer only the body's two ends are kept, with no background state.
    let filler = format!(r#""output":"{}","usage""#, "x".repeat(17 << 20));
    let big = completed.replacen(r#""usage""#, &filler, 1);
    for (case, reads) in [
        ("json", vec![bg_response("completed", BG_USAGE)]),
        (
            "chunked",
            h1_reads("Transfer-Encoding: chunked", completed.as_bytes()),
        ),
        ("gzip sse", vec![gzip_sse]),
        (
            "past 16 MB",
            h1_reads("Content-Length: {len}", big.as_bytes()),
        ),
    ] {
        let (mut bg, mut p) = (BackgroundCalls::default(), StreamProcessor::new());
        let mut out = exchange_with(&mut p, &mut bg, BG_CREATE, &bg_response("queued", "null"));
        out.extend(exchange_with(
            &mut p,
            &mut bg,
            BG_POLL,
            &bg_response("in_progress", "null"),
        ));
        out.extend(send_with(
            &mut p,
            &mut bg,
            LlmDirection::Write,
            BG_POLL,
            None,
        ));
        for r in &reads {
            out.extend(send_with(&mut p, &mut bg, LlmDirection::Read, r, None));
        }
        // The connection goes on with an ordinary call.
        out.extend(outcomes(&mut p, &h1_response(3000), None));
        assert_eq!(count(&out, Outcome::Recorded), 2, "{case}: {out:?}");
        assert!(
            unparsed(&out).is_empty() && !bg.contains("resp_bg1"),
            "{case}: {out:?}"
        );
    }
}

#[test]
fn test_only_a_queued_background_object_defers_its_call() {
    let mut bg = BackgroundCalls::default();
    let create = b"POST /v1/responses HTTP/1.1\r\nHost: api.openai.com\r\n\r\n{\"model\":\"o3-pro\",\"input\":\"hi\"}";
    // Model text that looks like a background object, in a response without usage.
    let lookalike = r#"{"id":"resp_2","object":"response","status":"completed","error":null,"output":[{"content":[{"text":"{\"id\":\"resp_x\",\"background\":true,\"status\":\"queued\"}"}]}],"usage":null}"#;
    let mut p = StreamProcessor::new();
    let mut out = exchange_with(&mut p, &mut bg, create, &h1("application/json", lookalike));
    // A stored (not background) response fetched by id after it was recorded.
    let stored =
        bg_object("completed", BG_USAGE).replace(r#""background":true"#, r#""background":false"#);
    out.extend(exchange_with(
        &mut p,
        &mut bg,
        create,
        &h1("application/json", &stored),
    ));
    out.extend(exchange_with(
        &mut p,
        &mut bg,
        BG_POLL,
        &h1("application/json", &stored),
    ));
    // The lookalike is a complete response with null usage, so it counts as usage_absent, not deferred.
    assert_eq!(unparsed(&out), [UnparsedReason::UsageAbsent], "{out:?}");
    assert_eq!(count(&out, Outcome::Recorded), 1, "{out:?}");
    assert_eq!(count(&out, Outcome::Deferred), 0, "{out:?}");
}

#[test]
fn h2_loss_skips_a_dropped_middle_fragment_by_its_exact_size() {
    let mut p = StreamProcessor::new();
    p.frag_next = 16384;
    p.frag_dir = LlmDirection::Read;
    // A read fragment at 32768 when 16384 was expected: exactly 16384 bytes dropped, skipped here now.
    assert_eq!(
        p.h2_loss(LlmDirection::Read, 32768, 49152, 16384, false),
        H2Gap::Skip(16384)
    );
}

#[test]
fn h2_loss_skips_a_truncated_tail_on_the_next_feed() {
    let mut p = StreamProcessor::new();
    // A truncated read: 20000-byte call, 16384 captured, so 3616 bytes are lost after it — pending.
    assert_eq!(
        p.h2_loss(LlmDirection::Read, 0, 20000, 16384, true),
        H2Gap::None
    );
    // The next read feed skips exactly that many, once.
    assert_eq!(
        p.h2_loss(LlmDirection::Read, 0, 500, 500, false),
        H2Gap::Skip(3616)
    );
    assert_eq!(
        p.h2_loss(LlmDirection::Read, 0, 500, 500, false),
        H2Gap::None
    );
}

#[test]
fn h2_loss_resyncs_the_previous_direction_when_a_new_call_interrupts_it() {
    let mut p = StreamProcessor::new();
    // A write call is left unfinished when a read call starts: the write lost a tail of unknown size.
    p.frag_next = 4096;
    p.frag_dir = LlmDirection::Write;
    assert_eq!(
        p.h2_loss(LlmDirection::Read, 0, 500, 500, false),
        H2Gap::None
    );
    // The read completed; the write re-anchors on its next feed.
    p.frag_next = 0;
    p.frag_dir = LlmDirection::Read;
    assert_eq!(
        p.h2_loss(LlmDirection::Write, 0, 100, 100, false),
        H2Gap::Resync
    );
}

/// A minimal HTTP/2 frame (9-byte header + payload), for mid-stream-join detection tests.
fn h2_frame(kind: u8, flags: u8, stream: u32, payload: &[u8]) -> Vec<u8> {
    let len = (payload.len() as u32).to_be_bytes();
    [&len[1..], &[kind, flags], &stream.to_be_bytes(), payload].concat()
}

#[test]
fn an_http11_request_is_not_misclassified_as_http2_midjoin() {
    // The h1 request+response records normally; the mid-join frame probe never fires on HTTP/1.1 bytes,
    // which start with an uppercase method, so the connection stays HTTP/1.1.
    let mut p = StreamProcessor::new();
    let out = outcomes(&mut p, &h1_response(50), None);
    assert_eq!(count(&out, Outcome::Recorded), 1, "{out:?}");
}

#[test]
fn a_connection_joined_mid_stream_is_detected_and_records_from_data() {
    // No preface: three client frame heads classify the connection as HTTP/2 (joined mid-stream). The
    // request headers were missed, so the response stream is tracked on its DATA and its usage read there.
    let mut p = StreamProcessor::new();
    let client = [
        h2_frame(0, 0, 1, b"aa"),
        h2_frame(0, 0, 1, b"bb"),
        h2_frame(0, 0, 1, b"cc"),
    ]
    .concat();
    let fed = send(&mut p, LlmDirection::Write, &client, None);
    assert!(fed.iter().all(|o| *o == Outcome::Pending), "{fed:?}");
    let json = b"{\"model\":\"gpt-4o\",\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":4}}";
    let third = json.len() / 3;
    let server = [
        h2_frame(0, 0, 1, &json[..third]),
        h2_frame(0, 0, 1, &json[third..2 * third]),
        h2_frame(0, 0x1, 1, &json[2 * third..]),
    ]
    .concat();
    let out = send(&mut p, LlmDirection::Read, &server, None);
    assert_eq!(count(&out, Outcome::Recorded), 1, "{out:?}");
}

/// An HPACK literal field (no indexing, new name, not Huffman): names/values < 127 B.
fn h2_lit(name: &[u8], value: &[u8]) -> Vec<u8> {
    let mut out = vec![0x00, name.len() as u8];
    out.extend_from_slice(name);
    out.push(value.len() as u8);
    out.extend_from_slice(value);
    out
}

/// A client request HEADERS frame (END_HEADERS, END_STREAM: no body) for `method authority path`.
fn h2_req(method: &[u8], authority: &[u8], path: &[u8], stream: u32) -> Vec<u8> {
    let block = [
        h2_lit(b":method", method),
        h2_lit(b":authority", authority),
        h2_lit(b":path", path),
    ]
    .concat();
    h2_frame(1, 0x4 | 0x1, stream, &block)
}

/// A server JSON response: HEADERS (status 200, END_HEADERS) then DATA (`body`, END_STREAM).
fn h2_json_response(stream: u32, body: &[u8]) -> Vec<u8> {
    [
        h2_frame(1, 0x4, stream, &h2_lit(b":status", b"200")),
        h2_frame(0, 0x1, stream, body),
    ]
    .concat()
}

#[test]
fn h2_background_call_is_recorded_once_from_a_later_poll() {
    use LlmDirection::{Read, Write};
    use honeybeepf_llm_core::h2::frame::PREFACE;
    let mut bg = BackgroundCalls::default();

    // Create on one connection: POST /v1/responses answered "queued" → the call is deferred, not recorded.
    let mut a = StreamProcessor::new();
    let mut out = send_with(&mut a, &mut bg, Write, PREFACE, None);
    out.extend(send_with(
        &mut a,
        &mut bg,
        Write,
        &h2_req(b"POST", b"api.openai.com", b"/v1/responses", 1),
        None,
    ));
    let queued = bg_object("queued", "null");
    out.extend(send_with(
        &mut a,
        &mut bg,
        Read,
        &h2_json_response(1, queued.as_bytes()),
        None,
    ));
    assert!(bg.contains("resp_bg1"), "{out:?}");
    assert_eq!(out.last(), Some(&Outcome::Deferred), "{out:?}");

    // Poll on a second connection: GET /v1/responses/{id} answered "completed" → recorded once, against the create.
    let mut b = StreamProcessor::new();
    let done = bg_object("completed", BG_USAGE);
    out.extend(send_with(&mut b, &mut bg, Write, PREFACE, None));
    out.extend(send_with(
        &mut b,
        &mut bg,
        Write,
        &h2_req(b"GET", b"api.openai.com", b"/v1/responses/resp_bg1", 1),
        None,
    ));
    out.extend(send_with(
        &mut b,
        &mut bg,
        Read,
        &h2_json_response(1, done.as_bytes()),
        None,
    ));
    assert_eq!(count(&out, Outcome::Recorded), 1, "{out:?}");
    assert!(!bg.contains("resp_bg1"), "{out:?}");
}

#[test]
fn h2_a_stored_response_get_we_never_queued_is_not_recorded() {
    // A GET /v1/responses/{id} the node never deferred (no matching create) settles nothing: its body is a
    // stored object, not our background result.
    use LlmDirection::{Read, Write};
    use honeybeepf_llm_core::h2::frame::PREFACE;
    let mut bg = BackgroundCalls::default();
    let mut p = StreamProcessor::new();
    let mut out = send_with(&mut p, &mut bg, Write, PREFACE, None);
    out.extend(send_with(
        &mut p,
        &mut bg,
        Write,
        &h2_req(b"GET", b"api.openai.com", b"/v1/responses/resp_other", 1),
        None,
    ));
    let done = bg_object("completed", BG_USAGE).replace("resp_bg1", "resp_other");
    out.extend(send_with(
        &mut p,
        &mut bg,
        Read,
        &h2_json_response(1, done.as_bytes()),
        None,
    ));
    assert_eq!(count(&out, Outcome::Recorded), 0, "{out:?}");
    assert!(unparsed(&out).is_empty(), "{out:?}");
}

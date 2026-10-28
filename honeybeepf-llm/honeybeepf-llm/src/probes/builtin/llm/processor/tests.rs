use super::*;
use honeybeepf_llm_common::{MAX_SSL_BUF_SIZE, MAX_SSL_FRAGMENTS};

const REQUEST: &[u8] = b"POST /v1/chat/completions HTTP/1.1\r\nHost: api.openai.com\r\nContent-Type: application/json\r\nContent-Length: 27\r\n\r\n{\"model\":\"t\",\"messages\":[]}";

/// Sends one SSL call the way the kernel does, leaving out the fragment at `skip`.
fn send(
    p: &mut StreamProcessor,
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
            1,
            #[cfg(feature = "k8s")]
            None,
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
    let mut out = send(p, LlmDirection::Write, request, None);
    let split = response
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map_or(response.len(), |i| i + 4);
    let (head, body) = response.split_at(split);
    out.extend(send(p, LlmDirection::Read, head, None));
    if !body.is_empty() {
        out.extend(send(p, LlmDirection::Read, body, None));
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
    let embeddings = b"POST /v1/embeddings HTTP/1.1\r\nHost: api.openai.com\r\nContent-Type: application/json\r\n\r\n{\"model\":\"text-embedding-3-small\",\"input\":\"hi\"}";
    let mut out = exchange(
        &mut p,
        embeddings,
        &h1(
            "application/json",
            r#"{"object":"list","data":[],"usage":{"prompt_tokens":1,"total_tokens":1}}"#,
        ),
    );
    let reads = h1_response(3000);
    out.extend(outcomes(&mut p, &reads, None));
    assert_eq!(count(&out, Outcome::Recorded), 1, "{out:?}");
    assert_eq!(out.last(), Some(&Outcome::Recorded), "{out:?}");
}

/// A multipart upload whose last body writes land after detection gave up at 4 KB, as httpx sends it.
fn upload(p: &mut StreamProcessor, expect_continue: bool) -> Vec<Outcome> {
    let head = format!(
        "POST /v1/audio/transcriptions HTTP/1.1\r\nHost: api.openai.com\r\n\
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
    let head = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n{headers}\r\n\r\n");
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
        openai_sse_reads(100_000),
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
        let logs = LOGS.with(|logs| logs.take());
        assert_eq!(out.last(), Some(&Outcome::Recorded), "{out:?}");
        assert!(logs.contains("[LLM] Detected"), "{logs}");
        assert!(!logs.contains("HBTEST"), "{logs}");
    }
}

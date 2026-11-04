use super::super::ResponseProgress;
use super::super::h1::plain_body;
use super::super::test_support::{base64, es_chunk, es_event, es_message, tokens};
use super::super::{Http11Parser, ProtocolParser};
use super::*;

const HEAD: &str = "HTTP/1.1 200 OK\r\nDate: Thu, 29 Oct 2026 10:00:00 GMT\r\n\
    Content-Type: application/vnd.amazon.eventstream\r\nTransfer-Encoding: chunked\r\n\
    Connection: keep-alive\r\nx-amzn-RequestId: 7f1c2d3e-0000-4000-8000-000000000001\r\n\r\n";

/// Model text quoting usage, end events and HTTP framing.
const PROTOCOL_TEXT: &str = "say \\\"usage\\\":{\\\"inputTokens\\\":1,\\\"outputTokens\\\":1} metadata \
    \\\"amazon-bedrock-invocationMetrics\\\":{\\\"inputTokenCount\\\":1,\\\"outputTokenCount\\\":1} \
    message_stop data: [DONE]\\r\\n0\\r\\n\\r\\nHTTP/1.1 200 OK";

const CONVERSE_METADATA: &str = r#"{"metrics":{"latencyMs":1275},"p":"abcdefghijklmnopqrstuvwxyzABCDEF","usage":{"cacheReadInputTokens":100,"cacheWriteInputTokens":20,"inputTokens":30,"outputTokens":9,"totalTokens":159}}"#;

/// ConverseStream as Bedrock sends it, `p` padding included; usage only in the last `metadata` event.
fn converse_stream(text: &str) -> Vec<Vec<u8>> {
    vec![
        es_event(
            "messageStart",
            r#"{"p":"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0","role":"assistant"}"#,
        ),
        es_event(
            "contentBlockDelta",
            &format!(r#"{{"contentBlockIndex":0,"delta":{{"text":"{text}"}},"p":"abcd"}}"#),
        ),
        es_event(
            "contentBlockStop",
            r#"{"contentBlockIndex":0,"p":"abcdefghijklmnopqrstuvwxyzABCDEFGHI"}"#,
        ),
        es_event(
            "messageStop",
            r#"{"p":"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRS","stopReason":"end_turn"}"#,
        ),
        es_event("metadata", CONVERSE_METADATA),
    ]
}

/// InvokeModelWithResponseStream for a Claude model: Anthropic SSE events, metrics on `message_stop`.
fn anthropic_invoke_stream(text: &str) -> Vec<Vec<u8>> {
    vec![
        es_chunk(
            r#"{"type":"message_start","message":{"id":"msg_bdrk_01","type":"message","role":"assistant","model":"claude-sonnet-5-5","content":[],"stop_reason":null,"usage":{"input_tokens":30,"cache_creation_input_tokens":20,"cache_read_input_tokens":100,"output_tokens":1}}}"#,
        ),
        es_chunk(
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        ),
        es_chunk(&format!(
            r#"{{"type":"content_block_delta","index":0,"delta":{{"type":"text_delta","text":"{text}"}}}}"#
        )),
        es_chunk(r#"{"type":"content_block_stop","index":0}"#),
        es_chunk(
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":9}}"#,
        ),
        es_chunk(
            r#"{"type":"message_stop","amazon-bedrock-invocationMetrics":{"inputTokenCount":30,"outputTokenCount":9,"invocationLatency":1275,"firstByteLatency":402,"cacheReadInputTokenCount":100,"cacheWriteInputTokenCount":20}}"#,
        ),
    ]
}

/// A model with its own event shape and no `usage`: only the metrics count.
fn titan_invoke_stream() -> Vec<Vec<u8>> {
    vec![
        es_chunk(
            r#"{"outputText":"Once upon a time","index":0,"totalOutputTextTokenCount":null,"completionReason":null,"inputTextTokenCount":12}"#,
        ),
        es_chunk(
            r#"{"outputText":" the end.","index":0,"totalOutputTextTokenCount":40,"completionReason":"FINISH","inputTextTokenCount":null,"amazon-bedrock-invocationMetrics":{"inputTokenCount":12,"outputTokenCount":40,"invocationLatency":900,"firstByteLatency":300}}"#,
        ),
    ]
}

/// One message per HTTP chunk, as Bedrock flushes them.
fn chunked(messages: &[Vec<u8>]) -> Vec<u8> {
    let mut resp = HEAD.as_bytes().to_vec();
    for m in messages {
        resp.extend_from_slice(format!("{:x}\r\n", m.len()).as_bytes());
        resp.extend_from_slice(m);
        resp.extend_from_slice(b"\r\n");
    }
    resp.extend_from_slice(b"0\r\n\r\n");
    resp
}

/// The messages cut into 7-byte chunks, so no chunk holds a whole message or prelude.
fn chunked_across(messages: &[Vec<u8>]) -> Vec<u8> {
    let joined = messages.concat();
    let pieces: Vec<Vec<u8>> = joined.chunks(7).map(<[u8]>::to_vec).collect();
    chunked(&pieces)
}

fn parse(resp: &[u8]) -> Option<UsageInfo> {
    Http11Parser.parse_response(resp)
}

fn cache(info: &UsageInfo) -> (Option<u64>, Option<u64>) {
    (info.cache_read_tokens, info.cache_write_tokens)
}

#[test]
fn message_from_an_independent_encoder_is_read() {
    // Framed by Python's zlib.crc32, not by this crate's encoder.
    let metadata: &[u8] = b"\x00\x00\x00\xee\x00\x00\x00Nc\xa2\xca@\x0b:event-type\x07\x00\x08metadata\
        \x0d:content-type\x07\x00\x10application/json\x0d:message-type\x07\x00\x05event\
        {\"usage\":{\"inputTokens\":30,\"outputTokens\":9,\"totalTokens\":159,\"cacheReadInputTokens\":100,\
        \"cacheWriteInputTokens\":20},\"metrics\":{\"latencyMs\":12}}\x00\xaa\x09\xde";
    assert_eq!(metadata.len(), 238);
    let info = parse_event_stream_body(metadata).expect("usage");
    assert_eq!((info.prompt_tokens, info.completion_tokens), (150, 9));
    assert_eq!(
        es_event("metadata", &String::from_utf8_lossy(&metadata[90..234])),
        metadata
    );
}

#[test]
fn converse_stream_reads_usage_from_the_metadata_event() {
    let info = parse(&chunked(&converse_stream("hi"))).expect("usage");
    assert_eq!((info.prompt_tokens, info.completion_tokens), (150, 9));
    assert_eq!(cache(&info), (Some(100), Some(20)));
    assert_eq!(info.model, None);
}

#[test]
fn anthropic_invoke_stream_merges_message_start_and_delta_usage() {
    let info = parse(&chunked(&anthropic_invoke_stream("hi"))).expect("usage");
    assert_eq!((info.prompt_tokens, info.completion_tokens), (150, 9));
    assert_eq!(cache(&info), (Some(100), Some(20)));
    assert_eq!(info.model.as_deref(), Some("claude-sonnet-5-5"));
}

#[test]
fn invoke_stream_of_other_models_reads_invocation_metrics() {
    let info = parse(&chunked(&titan_invoke_stream())).expect("usage");
    assert_eq!((info.prompt_tokens, info.completion_tokens), (12, 40));
    assert_eq!(info.model, None);
}

#[test]
fn invocation_metrics_survive_a_response_member_in_the_last_chunk() {
    // Cohere's stream-end event has its own `response` object, the shape of an OpenAI Responses event.
    let stream = [
        es_chunk(r#"{"is_finished":false,"event_type":"text-generation","text":"hi"}"#),
        es_chunk(
            r#"{"is_finished":true,"event_type":"stream-end","finish_reason":"COMPLETE","response":{"text":"hi","meta":{"billed_units":{"input_tokens":12,"output_tokens":40}}},"amazon-bedrock-invocationMetrics":{"inputTokenCount":12,"outputTokenCount":40}}"#,
        ),
    ];
    assert_eq!(tokens(parse(&chunked(&stream))), Some((12, 40)));
}

#[test]
fn chunk_bytes_with_escaped_slashes_are_read_and_bad_ones_skipped() {
    let last = r#"{"outputText":"???","amazon-bedrock-invocationMetrics":{"inputTokenCount":12,"outputTokenCount":40}}"#;
    let encoded = base64(last.as_bytes());
    assert!(encoded.contains('/'));
    let escaped = es_event(
        "chunk",
        &format!(r#"{{"bytes":"{}"}}"#, encoded.replace('/', "\\/")),
    );
    let bad = [
        es_event("chunk", r#"{"bytes":"eyJ1c2FnZSI6-_x"}"#),
        es_event("chunk", r#"{"bytes":7}"#),
        es_message(
            &[(":event-type", "chunk"), (":message-type", "event")],
            b"{\"bytes\":\"\xff\"}",
        ),
    ];
    let mut stream = titan_invoke_stream();
    stream.pop();
    stream.extend(bad);
    stream.push(escaped);
    assert_eq!(tokens(parse(&chunked(&stream))), Some((12, 40)));
}

#[test]
fn stream_before_its_last_event_is_incomplete() {
    for mut stream in [
        converse_stream("hi"),
        anthropic_invoke_stream("hi"),
        titan_invoke_stream(),
    ] {
        stream.pop();
        assert_eq!(tokens(parse(&chunked(&stream))), None);
    }
}

#[test]
fn text_that_looks_like_protocol_is_neither_usage_nor_an_end() {
    for stream in [
        converse_stream(PROTOCOL_TEXT),
        anthropic_invoke_stream(PROTOCOL_TEXT),
    ] {
        assert_eq!(tokens(parse(&chunked(&stream[..stream.len() - 1]))), None);
        assert_eq!(tokens(parse(&chunked(&stream))), Some((150, 9)));
    }
}

#[test]
fn exception_message_ends_the_call_as_an_error() {
    let throttled = es_message(
        &[
            (":exception-type", "throttlingException"),
            (":content-type", "application/json"),
            (":message-type", "exception"),
        ],
        br#"{"message":"Too many requests, please wait before trying again."}"#,
    );
    let error = es_message(
        &[
            (":error-code", "InternalFailure"),
            (":error-message", "boom"),
            (":message-type", "error"),
        ],
        b"",
    );
    for last in [throttled, error] {
        for mut stream in [converse_stream("hi"), anthropic_invoke_stream("hi")] {
            stream.truncate(2);
            stream.push(last.clone());
            assert_eq!(tokens(parse(&chunked(&stream))), Some((0, 0)));
        }
    }
}

#[test]
fn corrupt_message_stops_the_stream_without_usage() {
    let stream = converse_stream("hi");
    let mut flipped_payload = stream.clone();
    let last = flipped_payload.last_mut().expect("metadata");
    let at = last.len() - 10;
    last[at] ^= 1;
    let mut flipped_prelude = stream;
    flipped_prelude[1][9] ^= 1;
    for broken in [flipped_payload, flipped_prelude] {
        assert_eq!(tokens(parse(&chunked(&broken))), None);
    }

    // Lengths with a valid prelude CRC that no message can have.
    for (total, headers_len) in [(0u32, 0u32), (15, 0), (100, 85), (16 * 1024 * 1024 + 1, 0)] {
        let mut prelude = total.to_be_bytes().to_vec();
        prelude.extend_from_slice(&headers_len.to_be_bytes());
        prelude.extend_from_slice(&crc32(&prelude).to_be_bytes());
        let mut cursor = EventStreamCursor::default();
        cursor.feed(&prelude);
        cursor.feed(&es_event("metadata", CONVERSE_METADATA));
        assert!(cursor.progress == Progress::Broken, "{total}/{headers_len}");
        assert!(cursor.partial.is_empty());
    }
}

#[test]
fn headers_of_every_type_are_skipped() {
    let mut headers = Vec::new();
    for (kind, value) in [
        (0u8, &b""[..]),
        (1, b""),
        (2, b"\x01"),
        (3, b"\x00\x02"),
        (4, b"\x00\x00\x00\x03"),
        (5, &[0; 8]),
        (6, b"\x00\x03abc"),
        (8, &[0; 8]),
        (9, &[0; 16]),
    ] {
        headers.extend_from_slice(b"\x01x");
        headers.push(kind);
        headers.extend_from_slice(value);
    }
    let mut ok = headers.clone();
    ok.extend_from_slice(b"\x0b:event-type\x07\x00\x08metadata");
    assert_eq!(header(&ok, ":event-type"), Some(&b"metadata"[..]));
    assert_eq!(header(&ok, "x"), None);

    // A name or value running past the headers, or an unknown type, ends the lookup.
    for bad in [
        &b"\x0b:event-type"[..],
        b"\x0b:event-type\x07\x00\x09metadata",
        b"\x01x\x0a\x00",
    ] {
        let mut malformed = headers.clone();
        malformed.extend_from_slice(bad);
        assert_eq!(header(&malformed, ":event-type"), None, "{bad:?}");
    }
}

#[test]
fn base64_decodes_the_rfc_4648_vectors() {
    let mut out = Vec::new();
    for (text, want) in [
        ("", ""),
        ("Zg==", "f"),
        ("Zm9v", "foo"),
        ("Zm9vYg==", "foob"),
        ("Zm9vYmE=", "fooba"),
        ("Zm9vYmFy", "foobar"),
    ] {
        assert!(decode_base64(text.as_bytes(), &mut out));
        assert_eq!(out, want.as_bytes(), "{text}");
    }
    let every_byte: Vec<u8> = (0..=255).collect();
    assert!(decode_base64(base64(&every_byte).as_bytes(), &mut out));
    assert_eq!(out, every_byte);
    for bad in ["Zm9v\n", "Zm-v", "Zm9v\\u002b"] {
        assert!(!decode_base64(bad.as_bytes(), &mut out), "{bad}");
    }
}

#[test]
fn crc32_matches_the_check_value() {
    assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
}

#[test]
fn stream_past_its_framing_is_not_a_plain_json_body() {
    let resp = chunked(&converse_stream("hi"));
    assert!(plain_body(&resp).is_none());
}

#[test]
fn encoded_stream_is_decoded_once_it_ended() {
    use std::io::Write;
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(&converse_stream("hi").concat()).expect("gzip");
    let body = gz.finish().expect("gzip");
    let mut resp = HEAD
        .replace(
            "Transfer-Encoding: chunked",
            &format!("Content-Encoding: gzip\r\nContent-Length: {}", body.len()),
        )
        .into_bytes();
    resp.extend_from_slice(&body);
    assert_eq!(tokens(parse(&resp[..resp.len() - 1])), None);
    assert_eq!(tokens(parse(&resp)), Some((150, 9)));
}

/// Bedrock streams in every framing: one message per chunk, chunks cutting messages, and Content-Length.
fn framed_streams() -> Vec<Vec<u8>> {
    let converse = converse_stream(PROTOCOL_TEXT);
    let mut sized = HEAD
        .replace(
            "Transfer-Encoding: chunked",
            &format!("Content-Length: {}", converse.concat().len()),
        )
        .into_bytes();
    sized.extend_from_slice(&converse.concat());
    vec![
        chunked(&converse),
        chunked_across(&converse),
        sized,
        chunked_across(&anthropic_invoke_stream(PROTOCOL_TEXT)),
        chunked(&titan_invoke_stream()),
    ]
}

#[test]
fn resumed_parse_matches_a_fresh_parse_at_every_read_boundary() {
    for resp in framed_streams() {
        let fresh = |end: usize| parse(&resp[..end]);
        assert!(fresh(resp.len()).is_some());
        let mut byte_reads = ResponseProgress::default();
        for end in 1..=resp.len() {
            let resumed = Http11Parser.parse_response_from(&resp[..end], &mut byte_reads);
            assert_eq!(resumed, fresh(end), "byte reads, at {end}");
            let mut two_reads = ResponseProgress::default();
            Http11Parser.parse_response_from(&resp[..end], &mut two_reads);
            let whole = Http11Parser.parse_response_from(&resp, &mut two_reads);
            assert_eq!(whole, fresh(resp.len()), "split at {end}");
        }
    }
}

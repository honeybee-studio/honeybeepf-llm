use super::super::ResponseProgress;
use super::super::h1::plain_body;
use super::super::test_support::{es_event, es_message, tokens};
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
fn stream_before_its_last_event_is_incomplete() {
    let mut stream = converse_stream("hi");
    stream.pop();
    assert_eq!(tokens(parse(&chunked(&stream))), None);
}

#[test]
fn text_that_looks_like_protocol_is_neither_usage_nor_an_end() {
    let stream = converse_stream(PROTOCOL_TEXT);
    assert_eq!(tokens(parse(&chunked(&stream[..4]))), None);
    assert_eq!(tokens(parse(&chunked(&stream))), Some((150, 9)));
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
        let mut stream = converse_stream("hi");
        stream.truncate(2);
        stream.push(last);
        assert_eq!(tokens(parse(&chunked(&stream))), Some((0, 0)));
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
    vec![chunked(&converse), chunked_across(&converse), sized]
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

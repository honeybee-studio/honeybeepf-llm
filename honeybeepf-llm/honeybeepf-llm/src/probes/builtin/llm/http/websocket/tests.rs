use honeybeepf_llm_core::types::ModalityTokens;

use super::*;

/// A server frame: no mask, the shortest length form unless `long` forces the 64-bit one.
fn frame(fin: bool, opcode: u8, payload: &[u8], long: bool) -> Vec<u8> {
    let mut out = vec![u8::from(fin) << 7 | opcode];
    match payload.len() {
        n if long || n > 0xffff => {
            out.push(127);
            out.extend((n as u64).to_be_bytes());
        }
        n if n < 126 => out.push(n as u8),
        n => {
            out.push(126);
            out.extend((n as u16).to_be_bytes());
        }
    }
    out.extend_from_slice(payload);
    out
}

fn text(payload: &str) -> Vec<u8> {
    frame(true, 1, payload.as_bytes(), false)
}

const CREATED: &str = r#"{"type":"response.created","event_id":"event_2","response":{"object":"realtime.response","id":"resp_1","status":"in_progress","output":[],"usage":null}}"#;
const DONE: &str = r#"{"type":"response.done","event_id":"event_9","response":{"object":"realtime.response","id":"resp_1","status":"completed","output":[{"type":"message","content":[{"type":"output_audio","transcript":"Hi there"}]}],"usage":{"total_tokens":330,"input_tokens":250,"output_tokens":80,"input_token_details":{"text_tokens":50,"audio_tokens":200,"image_tokens":0,"cached_tokens":64},"output_token_details":{"text_tokens":20,"audio_tokens":60}},"metadata":null}}"#;

fn audio_delta(len: usize) -> String {
    format!(
        r#"{{"type":"response.output_audio.delta","event_id":"event_5","response_id":"resp_1","delta":"{}"}}"#,
        "A".repeat(len)
    )
}

fn realtime_ended() -> WsEvent {
    WsEvent::Ended(
        Some("resp_1".into()),
        Box::new(UsageInfo {
            prompt_tokens: 250,
            completion_tokens: 80,
            input_modalities: ModalityTokens {
                audio: 200,
                ..Default::default()
            },
            output_modalities: ModalityTokens {
                audio: 60,
                ..Default::default()
            },
            cache_read_tokens: Some(64),
            ..Default::default()
        }),
    )
}

/// Events of `stream` fed in pieces of `size` bytes.
fn read_in(stream: &[u8], size: usize) -> Result<Vec<WsEvent>, WsError> {
    let mut frames = ServerFrames::default();
    let mut events = Vec::new();
    for piece in stream.chunks(size) {
        frames.feed(piece, &mut events)?;
    }
    Ok(events)
}

#[test]
fn a_realtime_session_gives_each_response_start_and_usage_at_every_split() {
    let mut stream = Vec::new();
    for message in [CREATED.to_string(), audio_delta(70_000), DONE.into()] {
        stream.extend(text(&message));
    }
    let expected = [WsEvent::Started("resp_1".into()), realtime_ended()];
    for size in [1, 2, 3, 7, 10, 64, 1000, stream.len()] {
        assert_eq!(
            read_in(&stream, size).as_deref(),
            Ok(&expected[..]),
            "pieces of {size}"
        );
    }
}

#[test]
fn a_fragmented_message_with_a_ping_between_its_frames_is_one_message() {
    let (first, second) = DONE.split_at(40);
    let mut stream = frame(false, 1, first.as_bytes(), false);
    stream.extend(frame(true, 9, b"ping", false));
    stream.extend(frame(true, 0, second.as_bytes(), true));
    assert_eq!(read_in(&stream, 5), Ok(vec![realtime_ended()]));
}

#[test]
fn a_responses_stream_records_the_model_and_usage_of_its_completed_event() {
    let completed = r#"{"type":"response.completed","sequence_number":12,"response":{"id":"resp_7","object":"response","status":"completed","model":"gpt-6-sol","output":[],"usage":{"input_tokens":1200,"input_tokens_details":{"cached_tokens":1000},"output_tokens":30,"output_tokens_details":{"reasoning_tokens":10},"total_tokens":1230}}}"#;
    let mut stream =
        text(r#"{"type":"response.output_text.delta","sequence_number":5,"delta":"do"}"#);
    stream.extend(text(completed));
    let events = read_in(&stream, 9).expect("a readable stream");
    let [WsEvent::Ended(Some(id), usage)] = &events[..] else {
        panic!("{events:?}");
    };
    assert_eq!(&**id, "resp_7");
    assert_eq!(usage.model.as_deref(), Some("gpt-6-sol"));
    assert_eq!(
        (
            usage.prompt_tokens,
            usage.completion_tokens,
            usage.cache_read_tokens
        ),
        (1200, 30, Some(1000))
    );
}

#[test]
fn a_failed_response_ends_with_no_tokens_and_a_huge_one_is_unread() {
    let failed = r#"{"type":"response.done","event_id":"e","response":{"object":"realtime.response","id":"resp_2","status":"failed","output":[],"usage":{"total_tokens":0,"input_tokens":0,"output_tokens":0,"input_token_details":{"cached_tokens":0},"output_token_details":{}}}}"#;
    assert_eq!(
        read_in(&text(failed), 100),
        Ok(vec![WsEvent::Ended(Some("resp_2".into()), Box::default())])
    );
    let huge = DONE.replace("Hi there", &"x".repeat(MAX_USAGE));
    assert_eq!(read_in(&text(&huge), 4096), Ok(vec![WsEvent::Unread]));
}

#[test]
fn frames_a_server_may_not_send_stop_the_reading() {
    let mut masked = text(DONE);
    masked[1] |= 0x80;
    for (name, stream) in [
        ("masked", masked),
        (
            "rsv1, no extension read",
            frame(true, 1 | 0x40, b"{}", false),
        ),
        ("rsv2", frame(true, 1 | 0x20, b"{}", false)),
        ("continuation first", frame(true, 0, b"{}", false)),
        ("long ping", frame(true, 9, &[0; 126], false)),
        ("split close", frame(false, 8, b"", false)),
    ] {
        assert_eq!(read_in(&stream, 3), Err(WsError::Protocol), "{name}");
    }
    let mut huge = vec![0x82, 127];
    huge.extend((MAX_FRAME + 1).to_be_bytes());
    assert_eq!(read_in(&huge, 64), Err(WsError::TooLarge));
}

#[test]
fn random_bytes_never_panic() {
    // A fixed xorshift sequence, so a failure replays.
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    for round in 0..2000 {
        let bytes: Vec<u8> = (0..1 + round % 300)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect();
        let _ = read_in(&bytes, 1 + round % 17);
    }
}

#[test]
fn an_upgrade_to_a_listed_realtime_or_responses_path_is_detected() {
    let realtime = b"GET /v1/realtime?model=gpt-realtime HTTP/1.1\r\nHost: api.openai.com\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\n\r\n";
    assert_eq!(
        detect_upgrade(realtime),
        Some(Upgrade {
            path: "/v1/realtime".into(),
            model: Some("gpt-realtime".into()),
            matched: MatchedBy::Host,
        })
    );
    let azure = b"GET /openai/realtime?api-version=2025-04-01-preview&deployment=rt-east&api-key=SECRET HTTP/1.1\r\nHost: x.openai.azure.com\r\nupgrade: WebSocket\r\n\r\n";
    let detected = detect_upgrade(azure).expect("an Azure upgrade");
    assert_eq!(
        (detected.path.as_str(), detected.model.as_deref()),
        ("/openai/realtime", Some("rt-east"))
    );
    let responses =
        b"GET /v1/responses HTTP/1.1\r\nHost: api.openai.com\r\nUpgrade: websocket\r\n\r\n";
    assert_eq!(detect_upgrade(responses).map(|u| u.model), Some(None));
}

#[test]
fn other_requests_are_not_upgrades() {
    for req in [
        &b"GET /v1/realtime?model=a HTTP/1.1\r\nHost: api.openai.com\r\n\r\n"[..],
        b"GET /v1/realtime HTTP/1.1\r\nHost: example.com\r\nUpgrade: websocket\r\n\r\n",
        b"POST /v1/realtime HTTP/1.1\r\nHost: api.openai.com\r\nUpgrade: websocket\r\n\r\n",
        b"GET /v1/files HTTP/1.1\r\nHost: api.openai.com\r\nUpgrade: websocket\r\n\r\n",
        b"GET /v1/realtime HTTP/1.1\r\nHost: api.openai.com\r\nUpgrade: websocket\r\n",
    ] {
        assert_eq!(
            detect_upgrade(req),
            None,
            "{}",
            String::from_utf8_lossy(req)
        );
    }
}

#[test]
fn the_server_answer_sets_compression_or_refuses() {
    let switched = b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nsec-websocket-extensions: permessage-deflate\r\n\r\n\x81\x02{}";
    let deflate = |no_context_takeover| {
        Some(Deflate {
            no_context_takeover,
        })
    };
    assert_eq!(
        read_handshake(switched),
        Handshake::Switched {
            len: switched.len() - 4,
            deflate: deflate(false),
        }
    );
    let reset = b"HTTP/1.1 101 OK\r\nSec-WebSocket-Extensions: permessage-deflate; server_no_context_takeover; client_max_window_bits=15\r\n\r\n";
    assert!(
        matches!(read_handshake(reset), Handshake::Switched { deflate: d, .. } if d == deflate(true))
    );
    let plain = b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\r\n";
    assert!(matches!(
        read_handshake(plain),
        Handshake::Switched { deflate: None, .. }
    ));
    assert_eq!(read_handshake(b"HTTP/1.1 101 Swi"), Handshake::Partial);
    for refused in [
        &b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n"[..],
        b"HTTP/1.1 101 OK\r\nSec-WebSocket-Extensions: x-webkit-deflate-frame\r\n\r\n",
        b"\x81\x02{}",
    ] {
        assert_eq!(read_handshake(refused), Handshake::Refused);
    }
}

/// Messages compressed as a permessage-deflate server sends them: one stream, the sync tail cut off.
struct Deflater(flate2::Compress);

impl Deflater {
    fn new() -> Self {
        Self(flate2::Compress::new(flate2::Compression::default(), false))
    }

    fn message(&mut self, payload: &str) -> Vec<u8> {
        let mut out = Vec::with_capacity(payload.len() + 64);
        self.0
            .compress_vec(payload.as_bytes(), &mut out, flate2::FlushCompress::Sync)
            .expect("in-memory compression");
        while out.capacity() == out.len() {
            out.reserve(1024);
            self.0
                .compress_vec(&[], &mut out, flate2::FlushCompress::Sync)
                .expect("in-memory compression");
        }
        assert!(out.ends_with(&DEFLATE_TAIL));
        out.truncate(out.len() - DEFLATE_TAIL.len());
        frame(true, 1 | 0x40, &out, false)
    }
}

fn read_compressed(
    stream: &[u8],
    no_context_takeover: bool,
    size: usize,
) -> Result<Vec<WsEvent>, WsError> {
    let mut frames = ServerFrames::new(Some(Deflate {
        no_context_takeover,
    }));
    let mut events = Vec::new();
    for piece in stream.chunks(size) {
        frames.feed(piece, &mut events)?;
    }
    Ok(events)
}

#[test]
fn compressed_messages_share_one_window_unless_the_server_resets_it() {
    let expected = [WsEvent::Started("resp_1".into()), realtime_ended()];
    for no_context_takeover in [false, true] {
        let mut deflater = Deflater::new();
        let mut stream = Vec::new();
        for message in [CREATED.to_string(), audio_delta(20_000), DONE.into()] {
            if no_context_takeover {
                deflater = Deflater::new();
            }
            stream.extend(deflater.message(&message));
        }
        for size in [1, 7, 1000, stream.len()] {
            assert_eq!(
                read_compressed(&stream, no_context_takeover, size).as_deref(),
                Ok(&expected[..]),
                "pieces of {size}"
            );
        }
    }
}

#[test]
fn a_deflate_bomb_stops_at_the_message_cap_and_broken_bytes_stop_the_reading() {
    let bomb = format!(
        r#"{{"type":"response.done","pad":"{}"}}"#,
        "0".repeat(MAX_MESSAGE)
    );
    let stream = Deflater::new().message(&bomb);
    assert!(stream.len() < MAX_MESSAGE / 100, "{} bytes", stream.len());
    assert_eq!(
        read_compressed(&stream, false, 4096),
        Err(WsError::TooLarge)
    );
    let broken = frame(true, 1 | 0x40, &[0xff; 64], false);
    assert_eq!(read_compressed(&broken, false, 64), Err(WsError::Inflate));
}

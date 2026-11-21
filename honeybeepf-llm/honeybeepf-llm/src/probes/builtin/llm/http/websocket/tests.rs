use flate2::{Compress, Compression, FlushCompress};
use honeybeepf_llm_core::types::ModalityTokens;

use super::*;

/// A server frame: no mask, the shortest length form unless `long` forces the 64-bit one.
fn frame(fin: bool, rsv1: bool, opcode: u8, payload: &[u8], long: bool) -> Vec<u8> {
    let mut out = vec![u8::from(fin) << 7 | u8::from(rsv1) << 6 | opcode];
    match payload.len() {
        n if long => {
            out.push(127);
            out.extend((n as u64).to_be_bytes());
        }
        n if n < 126 => out.push(n as u8),
        n if n <= 0xffff => {
            out.push(126);
            out.extend((n as u16).to_be_bytes());
        }
        n => {
            out.push(127);
            out.extend((n as u64).to_be_bytes());
        }
    }
    out.extend_from_slice(payload);
    out
}

fn text(payload: &str) -> Vec<u8> {
    frame(true, false, 1, payload.as_bytes(), false)
}

/// Messages compressed as a permessage-deflate server sends them: one stream, the sync tail cut off.
struct Deflater(Compress);

impl Deflater {
    fn new() -> Self {
        Self(Compress::new(Compression::default(), false))
    }

    fn message(&mut self, payload: &str) -> Vec<u8> {
        let mut out = Vec::with_capacity(payload.len() + 64);
        self.0
            .compress_vec(payload.as_bytes(), &mut out, FlushCompress::Sync)
            .expect("in-memory compression");
        while out.capacity() == out.len() {
            out.reserve(1024);
            self.0
                .compress_vec(&[], &mut out, FlushCompress::Sync)
                .expect("in-memory compression");
        }
        assert!(out.ends_with(&DEFLATE_TAIL));
        out.truncate(out.len() - DEFLATE_TAIL.len());
        frame(true, true, 1, &out, false)
    }
}

const SESSION: &str = r#"{"type":"session.created","event_id":"event_1","session":{"object":"realtime.session","id":"sess_1","model":"gpt-realtime","output_modalities":["audio"]}}"#;
const CREATED: &str = r#"{"type":"response.created","event_id":"event_2","response":{"object":"realtime.response","id":"resp_1","status":"in_progress","output":[],"usage":null}}"#;
const DONE: &str = r#"{"type":"response.done","event_id":"event_9","response":{"object":"realtime.response","id":"resp_1","status":"completed","output":[{"type":"message","content":[{"type":"output_audio","transcript":"Hi there"}]}],"usage":{"total_tokens":330,"input_tokens":250,"output_tokens":80,"input_token_details":{"text_tokens":50,"audio_tokens":200,"image_tokens":0,"cached_tokens":64,"cached_tokens_details":{"text_tokens":0,"audio_tokens":64}},"output_token_details":{"text_tokens":20,"audio_tokens":60}},"metadata":null}}"#;

fn audio_delta(len: usize) -> String {
    format!(
        r#"{{"type":"response.output_audio.delta","event_id":"event_5","response_id":"resp_1","delta":"{}"}}"#,
        "A".repeat(len)
    )
}

fn realtime_usage() -> UsageInfo {
    UsageInfo {
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
    }
}

fn realtime_events() -> Vec<WsEvent> {
    vec![
        WsEvent::Model("gpt-realtime".into()),
        WsEvent::Started("resp_1".into()),
        WsEvent::Ended(Some("resp_1".into()), Box::new(realtime_usage())),
    ]
}

/// Events of `stream` fed in pieces of `size` bytes.
fn read_in(stream: &[u8], deflate: Option<Deflate>, size: usize) -> Result<Vec<WsEvent>, WsError> {
    let mut frames = ServerFrames::new(deflate);
    let mut events = Vec::new();
    for piece in stream.chunks(size) {
        frames.feed(piece, &mut events)?;
    }
    Ok(events)
}

fn assert_every_split(stream: &[u8], deflate: Option<Deflate>, expected: &[WsEvent]) {
    for size in [1, 2, 3, 7, 10, 64, 1000, stream.len()] {
        assert_eq!(
            read_in(stream, deflate, size).as_deref(),
            Ok(expected),
            "pieces of {size}"
        );
    }
}

#[test]
fn a_realtime_session_gives_its_model_response_start_and_usage_at_every_split() {
    let mut stream = Vec::new();
    for message in [
        SESSION.to_string(),
        CREATED.into(),
        audio_delta(70_000),
        DONE.into(),
    ] {
        stream.extend(text(&message));
    }
    assert_every_split(&stream, None, &realtime_events());
}

#[test]
fn compressed_messages_share_one_window_unless_the_server_resets_it() {
    for no_context_takeover in [false, true] {
        let mut deflater = Deflater::new();
        let mut stream = Vec::new();
        for message in [
            SESSION.to_string(),
            CREATED.into(),
            audio_delta(20_000),
            DONE.into(),
        ] {
            if no_context_takeover {
                deflater = Deflater::new();
            }
            stream.extend(deflater.message(&message));
        }
        let deflate = Some(Deflate {
            no_context_takeover,
        });
        assert_every_split(&stream, deflate, &realtime_events());
    }
}

#[test]
fn a_fragmented_message_with_a_ping_between_its_frames_is_one_message() {
    let (first, second) = DONE.split_at(40);
    let mut stream = frame(false, false, 1, first.as_bytes(), false);
    stream.extend(frame(true, false, 9, b"ping", false));
    stream.extend(frame(true, false, 0, second.as_bytes(), true));
    assert_eq!(
        read_in(&stream, None, 5),
        Ok(vec![WsEvent::Ended(
            Some("resp_1".into()),
            Box::new(realtime_usage())
        )])
    );
}

#[test]
fn a_responses_stream_records_the_model_and_usage_of_its_completed_event() {
    let completed = r#"{"type":"response.completed","sequence_number":12,"response":{"id":"resp_7","object":"response","status":"completed","model":"gpt-6-sol","output":[{"type":"message","content":[{"type":"output_text","text":"done"}]}],"usage":{"input_tokens":1200,"input_tokens_details":{"cached_tokens":1000},"output_tokens":30,"output_tokens_details":{"reasoning_tokens":10},"total_tokens":1230}}}"#;
    let delta = r#"{"type":"response.output_text.delta","sequence_number":5,"delta":"do"}"#;
    let mut stream = text(delta);
    stream.extend(text(completed));
    let events = read_in(&stream, None, 9).expect("a readable stream");
    let [WsEvent::Ended(Some(id), usage)] = &events[..] else {
        panic!("{events:?}");
    };
    assert_eq!(&**id, "resp_7");
    assert_eq!(usage.model.as_deref(), Some("gpt-6-sol"));
    assert_eq!((usage.prompt_tokens, usage.completion_tokens), (1200, 30));
    assert_eq!(usage.cache_read_tokens, Some(1000));
    assert_eq!(usage.thoughts_tokens, Some(10));
}

#[test]
fn a_usage_message_too_long_to_keep_is_read_from_its_ends() {
    let transcript = "x".repeat(MAX_WHOLE * 2);
    let done = DONE.replace("Hi there", &transcript);
    assert_every_split(
        &text(&done),
        None,
        &[WsEvent::Ended(
            Some("resp_1".into()),
            Box::new(realtime_usage()),
        )],
    );
}

#[test]
fn a_failed_response_ends_with_no_tokens() {
    let failed = r#"{"type":"response.done","event_id":"e","response":{"object":"realtime.response","id":"resp_2","status":"failed","status_details":{"type":"failed","error":{"type":"server_error"}},"output":[],"usage":{"total_tokens":0,"input_tokens":0,"output_tokens":0,"input_token_details":{"cached_tokens":0},"output_token_details":{}}}}"#;
    assert_eq!(
        read_in(&text(failed), None, 100),
        Ok(vec![WsEvent::Ended(Some("resp_2".into()), Box::default())])
    );
}

#[test]
fn frames_a_server_may_not_send_stop_the_reading() {
    let masked = {
        let mut f = text(DONE);
        f[1] |= 0x80;
        f
    };
    let cases: [(&str, Vec<u8>, Option<Deflate>); 6] = [
        ("masked", masked, None),
        ("rsv2", frame(true, false, 1 | 0x20, b"{}", false), None),
        (
            "continuation first",
            frame(true, false, 0, b"{}", false),
            None,
        ),
        ("long ping", frame(true, false, 9, &[0; 126], false), None),
        ("split close", frame(false, false, 8, b"", false), None),
        (
            "compressed, not agreed",
            frame(true, true, 1, b"\x02\x00", false),
            None,
        ),
    ];
    for (name, stream, deflate) in cases {
        assert_eq!(
            read_in(&stream, deflate, 3),
            Err(WsError::Protocol),
            "{name}"
        );
    }
}

#[test]
fn a_deflate_bomb_stops_at_the_message_cap() {
    let mut deflater = Deflater::new();
    let bomb = format!(
        r#"{{"type":"response.done","pad":"{}"}}"#,
        "0".repeat(MAX_MESSAGE)
    );
    let stream = deflater.message(&bomb);
    assert!(stream.len() < MAX_MESSAGE / 100, "{} bytes", stream.len());
    let deflate = Some(Deflate {
        no_context_takeover: false,
    });
    assert_eq!(read_in(&stream, deflate, 4096), Err(WsError::TooLarge));
}

#[test]
fn broken_compressed_bytes_stop_the_reading() {
    let deflate = Some(Deflate {
        no_context_takeover: false,
    });
    assert_eq!(
        read_in(&frame(true, true, 1, &[0xff; 64], false), deflate, 64),
        Err(WsError::Inflate)
    );
}

#[test]
fn random_bytes_never_panic_and_hold_little() {
    // A fixed xorshift sequence, so a failure replays.
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    for round in 0..2000 {
        let len = 1 + round % 300;
        let bytes: Vec<u8> = (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect();
        let deflate = (round % 2 == 0).then_some(Deflate {
            no_context_takeover: round % 4 == 0,
        });
        let _ = read_in(&bytes, deflate, 1 + round % 17);
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
    assert_eq!(
        read_handshake(switched),
        Handshake::Switched {
            len: switched.len() - 4,
            deflate: Some(Deflate {
                no_context_takeover: false
            }),
        }
    );
    let reset = b"HTTP/1.1 101 OK\r\nSec-WebSocket-Extensions: permessage-deflate; server_no_context_takeover; client_max_window_bits=15\r\n\r\n";
    assert!(matches!(
        read_handshake(reset),
        Handshake::Switched {
            deflate: Some(Deflate {
                no_context_takeover: true
            }),
            ..
        }
    ));
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

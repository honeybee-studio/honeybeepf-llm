use super::*;

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

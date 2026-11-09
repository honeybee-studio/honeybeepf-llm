#![cfg(test)]

use super::*;
use crate::probes::builtin::llm::processor::MAX_RESPONSE_BUFFER_SIZE;

/// A header field as a literal without indexing, new name, not Huffman-coded (names/values < 127 B).
fn lit(name: &[u8], value: &[u8]) -> Vec<u8> {
    let mut out = vec![0x00, name.len() as u8];
    out.extend_from_slice(name);
    out.push(value.len() as u8);
    out.extend_from_slice(value);
    out
}

#[test]
fn preface_needs_the_whole_marker() {
    assert!(starts_preface(PREFACE));
    assert!(!starts_preface(b"P"));
    assert!(!starts_preface(b"POST / HTTP/1.1\r\n"));
}

/// A POST a provider serves is a create; a GET of a background Responses id is a poll; everything else
/// (non-provider host, a stored-object GET) takes no map slot.
#[test]
fn match_request_detects_provider_posts_and_responses_polls() {
    let block = |method: &[u8], authority: &str, path: &str| {
        [
            lit(b":method", method),
            lit(b":authority", authority.as_bytes()),
            lit(b":path", path.as_bytes()),
        ]
        .concat()
    };
    let m = |b: &[u8]| match_request(&mut Decoder::new(), b);
    let openai = block(b"POST", "api.openai.com", "/v1/chat/completions?x=1");
    assert!(matches!(
        m(&openai),
        Some(Matched::Create { path, record_only: false }) if path == "/v1/chat/completions"
    ));
    // A Managed Agents session POST is matched as record-only.
    let session = block(b"POST", "api.anthropic.com", "/v1/sessions");
    assert!(matches!(
        m(&session),
        Some(Matched::Create { path, record_only: true }) if path == "/v1/sessions"
    ));
    assert!(m(&block(b"POST", "noise.internal", "/p")).is_none());
    // A background poll: GET /v1/responses/{id} to a provider host, query stripped.
    assert!(matches!(
        m(&block(b"GET", "api.openai.com", "/v1/responses/resp_1?stream=true")),
        Some(Matched::Poll { id, .. }) if &*id == "resp_1"
    ));
    // Not a poll: no id, a non-provider host, or a GET on another provider path (stored object).
    assert!(m(&block(b"GET", "api.openai.com", "/v1/responses/")).is_none());
    assert!(m(&block(b"GET", "noise.internal", "/v1/responses/resp_1")).is_none());
    assert!(
        m(&block(
            b"GET",
            "api.openai.com",
            "/v1/chat/completions/chatcmpl-1"
        ))
        .is_none()
    );
}

const JSON: &[u8] =
    b"{\"model\":\"gpt-4o\",\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":4}}";

fn frame(kind: u8, flags: u8, stream: u32, payload: &[u8]) -> Vec<u8> {
    let len = (payload.len() as u32).to_be_bytes();
    [&len[1..], &[kind, flags], &stream.to_be_bytes(), payload].concat()
}

/// A matched provider request (HEADERS with END_HEADERS and END_STREAM).
fn req(stream: u32) -> Vec<u8> {
    let block = [
        lit(b":method", b"POST"),
        lit(b":authority", b"api.openai.com"),
        lit(b":path", b"/v1/chat/completions"),
    ]
    .concat();
    frame(1, 0x4 | 0x1, stream, &block)
}

/// Drives one `H2Conn` (preface first), returning how many streams it recorded and the bytes it holds.
fn run(reads: &[&[u8]]) -> (usize, usize) {
    let mut conn = H2Conn::new();
    let mut recorded = 0;
    {
        let mut on = |event| {
            if matches!(event, H2Event::Recorded { .. }) {
                recorded += 1;
            }
        };
        conn.feed(LlmDirection::Write, PREFACE, H2Gap::None, &mut on);
        conn.feed(LlmDirection::Write, &req(1), H2Gap::None, &mut on);
        for r in reads {
            conn.feed(LlmDirection::Read, r, H2Gap::None, &mut on);
        }
    }
    (recorded, conn.buffered)
}

#[test]
fn a_finished_stream_returns_its_bytes_to_the_connection_budget() {
    let head = frame(1, 0x4, 1, &lit(b":status", b"200"));
    let body = frame(0, 0x1, 1, JSON);
    // Recorded once, and the budget is back to zero so later streams on the connection are not starved.
    assert_eq!(run(&[&head, &body]), (1, 0));
}

#[test]
fn a_trailers_block_does_not_discard_the_body() {
    let head = frame(1, 0x4, 1, &lit(b":status", b"200"));
    let body = frame(0, 0, 1, JSON);
    let trailers = frame(1, 0x4 | 0x1, 1, &lit(b"x-trailer", b"1"));
    assert_eq!(run(&[&head, &body, &trailers]), (1, 0));
}

fn fixture(name: &str, dir: &str) -> Vec<u8> {
    let path = format!(
        "{}/../honeybeepf-llm-core/tests/testdata/h2/{name}.{dir}.bin",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// (detections, recordings) from feeding a recording's two directions in `step`-byte chunks.
fn counts(c2s: &[u8], s2c: &[u8], step: usize) -> (usize, usize) {
    let mut conn = H2Conn::new();
    let (mut detected, mut recorded) = (0, 0);
    {
        let mut on = |event| match event {
            H2Event::Detected { .. } => detected += 1,
            H2Event::Recorded { .. } => recorded += 1,
            H2Event::Deferred { .. }
            | H2Event::Poll { .. }
            | H2Event::Unparsed { .. }
            | H2Event::Resynced => {}
        };
        for chunk in c2s.chunks(step) {
            conn.feed(LlmDirection::Write, chunk, H2Gap::None, &mut on);
        }
        for chunk in s2c.chunks(step) {
            conn.feed(LlmDirection::Read, chunk, H2Gap::None, &mut on);
        }
    }
    (detected, recorded)
}

#[test]
fn recorded_streams_are_one_per_request_at_any_split() {
    // JSON, SSE and gzip responses all record their usage now.
    for (name, want) in [
        ("curl-json", (1, 1)),
        ("curl-sse", (1, 1)),
        ("curl-gzip", (1, 1)),
        ("curl-large", (1, 1)),
        ("httpx-reuse", (2, 2)),
        ("node26-openai-sse", (1, 1)),
        ("node26-anthropic-json", (1, 1)),
    ] {
        let (c2s, s2c) = (fixture(name, "c2s"), fixture(name, "s2c"));
        // Byte by byte and whole bracket every split a reassembled frame reader can see.
        for step in [1, usize::MAX] {
            assert_eq!(counts(&c2s, &s2c, step), want, "{name} step {step}");
        }
    }
}

/// Drives a connection (preface + a provider request on stream 1), then the given reads, returning
/// (recorded, closed, too_large) counts.
fn drive(reads: &[(LlmDirection, Vec<u8>)]) -> (usize, usize, usize) {
    let mut conn = H2Conn::new();
    let (mut rec, mut closed, mut big) = (0, 0, 0);
    {
        let mut on = |event| match event {
            H2Event::Recorded { .. } => rec += 1,
            H2Event::Unparsed {
                reason: UnparsedReason::ResponseTooLarge,
            } => big += 1,
            H2Event::Unparsed { .. } => closed += 1,
            H2Event::Detected { .. }
            | H2Event::Deferred { .. }
            | H2Event::Poll { .. }
            | H2Event::Resynced => {}
        };
        conn.feed(LlmDirection::Write, PREFACE, H2Gap::None, &mut on);
        conn.feed(LlmDirection::Write, &req(1), H2Gap::None, &mut on);
        for (dir, bytes) in reads {
            conn.feed(*dir, bytes, H2Gap::None, &mut on);
        }
    }
    (rec, closed, big)
}

#[test]
fn a_reset_stream_is_counted_closed_not_recorded() {
    let rst = frame(3, 0, 1, b"\0\0\0\x08");
    assert_eq!(drive(&[(LlmDirection::Read, rst)]), (0, 1, 0));
}

#[test]
fn goaway_closes_streams_above_the_last_and_spares_those_at_or_below() {
    let goaway = || frame(7, 0, 0, b"\0\0\0\x01\0\0\0\0");
    // Stream 3 is above last_stream=1, so it is closed; stream 1 is spared.
    assert_eq!(
        drive(&[
            (LlmDirection::Write, req(3)),
            (LlmDirection::Read, goaway()),
        ]),
        (0, 1, 0)
    );
    // The spared stream 1 can still finish and record after the GOAWAY.
    let head = frame(1, 0x4, 1, &lit(b":status", b"200"));
    let body = frame(0, 0x1, 1, JSON);
    assert_eq!(
        drive(&[
            (LlmDirection::Write, req(3)),
            (LlmDirection::Read, goaway()),
            (LlmDirection::Read, head),
            (LlmDirection::Read, body),
        ]),
        (1, 1, 0)
    );
}

#[test]
fn a_response_over_the_cap_is_counted_too_large_at_its_end() {
    let head = frame(1, 0x4, 1, &lit(b":status", b"200"));
    let chunk = frame(0, 0, 1, &[b'a'; 16 * 1024]);
    let n = MAX_RESPONSE_BUFFER_SIZE / (16 * 1024) + 1;
    let mut reads = vec![(LlmDirection::Read, head)];
    reads.extend(std::iter::repeat_n((LlmDirection::Read, chunk), n));
    reads.push((LlmDirection::Read, frame(0, 0x1, 1, b"")));
    assert_eq!(drive(&reads), (0, 0, 1));
}

#[test]
fn a_managed_agents_session_stream_is_counted_agent_session_not_recorded() {
    let session_req = |stream, path: &[u8]| {
        let block = [
            lit(b":method", b"POST"),
            lit(b":authority", b"api.anthropic.com"),
            lit(b":path", path),
        ]
        .concat();
        frame(1, 0x4 | 0x1, stream, &block)
    };
    let head = |stream| frame(1, 0x4, stream, &lit(b":status", b"200"));
    // Stream 1's body carries a cumulative session total; it must not be recorded as a message.
    let session_body = frame(
        0,
        0x1,
        1,
        b"{\"usage\":{\"input_tokens\":5000,\"output_tokens\":3200,\"list_cost\":{\"amount\":\"187\"}}}",
    );
    // Stream 3's events send is acked with no usage; it is still counted, completion being the signal.
    let ack_body = frame(0, 0x1, 3, b"{\"data\":[{\"id\":\"evt_1\"}]}");

    let mut conn = H2Conn::new();
    let (mut recorded, mut agent) = (0, 0);
    {
        let mut on = |event| match event {
            H2Event::Recorded { .. } => recorded += 1,
            H2Event::Unparsed {
                reason: UnparsedReason::AgentSession,
            } => agent += 1,
            _ => {}
        };
        conn.feed(LlmDirection::Write, PREFACE, H2Gap::None, &mut on);
        conn.feed(
            LlmDirection::Write,
            &session_req(1, b"/v1/sessions"),
            H2Gap::None,
            &mut on,
        );
        conn.feed(
            LlmDirection::Write,
            &session_req(3, b"/v1/sessions/sess_1/events"),
            H2Gap::None,
            &mut on,
        );
        conn.feed(LlmDirection::Read, &head(1), H2Gap::None, &mut on);
        conn.feed(LlmDirection::Read, &session_body, H2Gap::None, &mut on);
        conn.feed(LlmDirection::Read, &head(3), H2Gap::None, &mut on);
        conn.feed(LlmDirection::Read, &ack_body, H2Gap::None, &mut on);
    }
    assert_eq!((recorded, agent), (0, 2));
}

/// Opens stream 1 and feeds it `response` reads, returning the live `H2Conn` for an idle-sweep check.
fn conn_with_open_stream(response: &[&[u8]]) -> H2Conn {
    let mut conn = H2Conn::new();
    let mut on = |_| {};
    conn.feed(LlmDirection::Write, PREFACE, H2Gap::None, &mut on);
    conn.feed(LlmDirection::Write, &req(1), H2Gap::None, &mut on);
    for r in response {
        conn.feed(LlmDirection::Read, r, H2Gap::None, &mut on);
    }
    conn
}

#[test]
fn a_stalled_response_is_swept_and_counted_closed() {
    // A response that began (DATA seen) then stopped is abandoned; sweeping it frees its buffered bytes.
    let head = frame(1, 0x4, 1, &lit(b":status", b"200"));
    let part = frame(0, 0, 1, b"{\"partial\":");
    let mut conn = conn_with_open_stream(&[&head, &part]);
    let mut closed = 0;
    {
        let mut on = |e| {
            if matches!(e, H2Event::Unparsed { .. }) {
                closed += 1;
            }
        };
        assert_eq!(conn.open_streams(), 1);
        // A test cannot wait out the idle window, so sweep with a clock past it directly.
        let future = Instant::now() + STREAM_IDLE + Duration::from_secs(1);
        sweep_idle(&mut conn.streams, &mut conn.buffered, future, &mut on);
    }
    assert_eq!((conn.open_streams(), closed, conn.buffered), (0, 1, 0));
}

#[test]
fn a_stream_awaiting_its_first_byte_is_not_swept() {
    // A slow time-to-first-byte must not be mistaken for an abandoned stream, or its response is lost.
    let mut conn = conn_with_open_stream(&[]);
    let mut closed = 0;
    {
        let mut on = |e| {
            if matches!(e, H2Event::Unparsed { .. }) {
                closed += 1;
            }
        };
        let future = Instant::now() + STREAM_IDLE + Duration::from_secs(1);
        sweep_idle(&mut conn.streams, &mut conn.buffered, future, &mut on);
    }
    assert_eq!((conn.open_streams(), closed), (1, 0));
}

/// `json` across three DATA frames on `stream`, END_STREAM on the last: a run that re-anchors a reader.
fn data_run(stream: u32, json: &[u8]) -> Vec<u8> {
    let third = json.len() / 3;
    [
        frame(0, 0, stream, &json[..third]),
        frame(0, 0, stream, &json[third..2 * third]),
        frame(0, 0x1, stream, &json[2 * third..]),
    ]
    .concat()
}

/// Preface + a provider request on each of `streams`, then one server feed marked as following a gap,
/// returning (recorded, resynced).
fn after_gap(streams: &[u32], server: &[u8]) -> (usize, usize) {
    let mut conn = H2Conn::new();
    let (mut rec, mut resynced) = (0, 0);
    {
        let mut on = |e| match e {
            H2Event::Recorded { .. } => rec += 1,
            H2Event::Resynced => resynced += 1,
            _ => {}
        };
        conn.feed(LlmDirection::Write, PREFACE, H2Gap::None, &mut on);
        for &s in streams {
            conn.feed(LlmDirection::Write, &req(s), H2Gap::None, &mut on);
        }
        conn.feed(LlmDirection::Read, server, H2Gap::Resync, &mut on);
    }
    (rec, resynced)
}

#[test]
fn a_gap_resyncs_the_server_and_records_from_the_run_after_it() {
    // Bytes are lost mid-response; the reader skips the stray leading bytes, re-anchors on the next run
    // of three DATA frames, and still records the usage (the request was matched before the gap).
    let server = [&[0xff; 5][..], &data_run(1, JSON)].concat();
    assert_eq!(after_gap(&[1], &server), (1, 1));
}

#[test]
fn a_gap_that_resyncs_onto_non_usage_bytes_records_nothing() {
    // A re-anchored run whose DATA is not a usage JSON fabricates no number; only the resync is counted.
    let junk = b"{\"choices\":[]}{\"choices\":[]}{\"choices\":[]}";
    let server = [&[0xff; 5][..], &data_run(1, junk)].concat();
    assert_eq!(after_gap(&[1], &server), (0, 1));
}

#[test]
fn a_gap_resyncs_and_records_one_of_several_multiplexed_streams() {
    // Two requests are open; after a gap the server's run is on stream 3, which records while 1 stays open.
    let server = [&[0xff; 5][..], &data_run(3, JSON)].concat();
    assert_eq!(after_gap(&[1, 3], &server), (1, 1));
}

/// Feeds a mid-join connection its server bytes (lost, as a mid-stream join makes them), returning
/// (recorded, recordings flagged mid-join).
fn midjoin_server(server: &[u8]) -> (usize, usize) {
    let mut conn = H2Conn::new_midjoin();
    let (mut rec, mut flagged) = (0, 0);
    {
        let mut on = |e| {
            if let H2Event::Recorded { midjoin, .. } = e {
                rec += 1;
                flagged += usize::from(midjoin);
            }
        };
        conn.feed(LlmDirection::Read, server, H2Gap::Resync, &mut on);
    }
    (rec, flagged)
}

#[test]
fn a_midjoin_connection_records_a_response_from_its_data_alone() {
    // No request was seen; a run of three server DATA frames carrying a usage JSON is tracked on its
    // DATA and recorded, flagged mid-join (host unknown).
    assert_eq!(midjoin_server(&data_run(1, JSON)), (1, 1));
}

#[test]
fn a_midjoin_connection_records_nothing_from_non_usage_data() {
    // Speculatively tracked DATA that is not a usage JSON records nothing, so it fabricates no number.
    let junk = b"{\"choices\":[]}{\"choices\":[]}{\"choices\":[]}";
    assert_eq!(midjoin_server(&data_run(1, junk)), (0, 0));
}

#[test]
fn non_llm_midjoin_connections_stay_within_a_small_buffer() {
    // gRPC-like framed DATA (a binary length prefix) never looks recordable: across many mid-join
    // connections no stream is tracked and nothing is buffered.
    let grpc = data_run(1, &[0u8; 300]);
    for _ in 0..50 {
        let mut conn = H2Conn::new_midjoin();
        conn.feed(LlmDirection::Read, &grpc, H2Gap::Resync, &mut |_| {});
        assert_eq!((conn.open_streams(), conn.buffered), (0, 0));
    }
    // A JSON-looking (non-usage) body well over the per-connection mid-join cap is held to it, not to the
    // 32 MiB connection budget: the over-cap stream drops its buffer, so little stays buffered.
    let chunk = vec![b' '; 16_000];
    let mut body = {
        let mut head = vec![b'{'];
        head.extend(std::iter::repeat_n(b' ', 15_999));
        frame(0, 0, 1, &head)
    };
    for _ in 0..100 {
        body.extend(frame(0, 0, 1, &chunk));
    }
    let mut conn = H2Conn::new_midjoin();
    conn.feed(LlmDirection::Read, &body, H2Gap::Resync, &mut |_| {});
    assert!(
        conn.buffered < MIDJOIN_CONN_CAP,
        "buffered {}",
        conn.buffered
    );
}

#[test]
fn a_normal_connection_does_not_speculatively_track_server_data() {
    // Without the mid-join flag, server DATA on a stream no request matched is ignored.
    let mut conn = H2Conn::new();
    let mut rec = 0;
    {
        let mut on = |e| {
            if matches!(e, H2Event::Recorded { .. }) {
                rec += 1;
            }
        };
        conn.feed(LlmDirection::Write, PREFACE, H2Gap::None, &mut on);
        conn.feed(LlmDirection::Read, &data_run(1, JSON), H2Gap::None, &mut on);
    }
    assert_eq!(rec, 0);
}

#[test]
fn a_forgotten_client_tracks_later_responses_and_counts_the_unrecordable() {
    // A client-direction gap forgets the request table, so later responses cannot be matched. They are
    // tracked speculatively: one carrying usage records host-unknown, one without counts headers_lost.
    let mut conn = H2Conn::new();
    let (mut rec, mut headers_lost) = (0, 0);
    {
        let mut on = |e| match e {
            H2Event::Recorded { .. } => rec += 1,
            H2Event::Unparsed {
                reason: UnparsedReason::HeadersLost,
            } => headers_lost += 1,
            _ => {}
        };
        conn.feed(LlmDirection::Write, PREFACE, H2Gap::None, &mut on);
        // A gap on the request direction forgets the client HPACK table.
        conn.feed(LlmDirection::Write, &req(1), H2Gap::Resync, &mut on);
        conn.feed(LlmDirection::Read, &data_run(5, JSON), H2Gap::None, &mut on);
        let junk = b"{\"choices\":[]}{\"choices\":[]}{\"choices\":[]}";
        conn.feed(LlmDirection::Read, &data_run(7, junk), H2Gap::None, &mut on);
    }
    assert_eq!((rec, headers_lost), (1, 1));
}

#[test]
fn a_completed_success_stream_without_usage_counts_usage_absent() {
    let head = frame(1, 0x4, 1, &lit(b":status", b"200"));
    let body = frame(0, 0x1, 1, b"{\"embedding\":{\"values\":[0.1]}}");
    let (mut rec, mut usage_absent) = (0, 0);
    let mut conn = H2Conn::new();
    {
        let mut on = |e| match e {
            H2Event::Recorded { .. } => rec += 1,
            H2Event::Unparsed {
                reason: UnparsedReason::UsageAbsent,
            } => usage_absent += 1,
            _ => {}
        };
        conn.feed(LlmDirection::Write, PREFACE, H2Gap::None, &mut on);
        conn.feed(LlmDirection::Write, &req(1), H2Gap::None, &mut on);
        conn.feed(LlmDirection::Read, &head, H2Gap::None, &mut on);
        conn.feed(LlmDirection::Read, &body, H2Gap::None, &mut on);
    }
    assert_eq!((rec, usage_absent), (0, 1));
}

#[test]
fn an_error_stream_without_usage_is_not_usage_absent() {
    let head = frame(1, 0x4, 1, &lit(b":status", b"503"));
    let body = frame(0, 0x1, 1, b"{\"message\":\"unavailable\"}");
    let mut unparsed = 0;
    let mut conn = H2Conn::new();
    {
        let mut on = |e| {
            if matches!(e, H2Event::Unparsed { .. }) {
                unparsed += 1;
            }
        };
        conn.feed(LlmDirection::Write, PREFACE, H2Gap::None, &mut on);
        conn.feed(LlmDirection::Write, &req(1), H2Gap::None, &mut on);
        conn.feed(LlmDirection::Read, &head, H2Gap::None, &mut on);
        conn.feed(LlmDirection::Read, &body, H2Gap::None, &mut on);
    }
    assert_eq!(unparsed, 0);
}

/// A provider request with END_STREAM for `method path` on `stream` (no request body).
fn req_full(method: &[u8], path: &[u8], stream: u32) -> Vec<u8> {
    let block = [
        lit(b":method", method),
        lit(b":authority", b"api.openai.com"),
        lit(b":path", path),
    ]
    .concat();
    frame(1, 0x4 | 0x1, stream, &block)
}

#[test]
fn a_queued_background_create_emits_deferred() {
    // A POST /v1/responses whose response is a queued background object: its id is deferred, not recorded.
    let head = frame(1, 0x4, 1, &lit(b":status", b"200"));
    let queued = br#"{"id":"resp_bg1","object":"response","status":"queued","background":true}"#;
    let mut conn = H2Conn::new();
    let (mut deferred, mut recorded) = (None, 0);
    {
        let mut on = |e| match e {
            H2Event::Deferred { id, .. } => deferred = Some(id),
            H2Event::Recorded { .. } => recorded += 1,
            _ => {}
        };
        conn.feed(LlmDirection::Write, PREFACE, H2Gap::None, &mut on);
        conn.feed(
            LlmDirection::Write,
            &req_full(b"POST", b"/v1/responses", 1),
            H2Gap::None,
            &mut on,
        );
        conn.feed(LlmDirection::Read, &head, H2Gap::None, &mut on);
        conn.feed(
            LlmDirection::Read,
            &frame(0, 0x1, 1, queued),
            H2Gap::None,
            &mut on,
        );
    }
    assert_eq!((deferred.as_deref(), recorded), (Some("resp_bg1"), 0));
}

#[test]
fn a_responses_poll_emits_its_result_against_the_id() {
    // A GET /v1/responses/{id} is a poll: its completed body is surfaced as the result, not recorded here.
    let head = frame(1, 0x4, 1, &lit(b":status", b"200"));
    let done = br#"{"id":"resp_bg1","object":"response","status":"completed","background":true,"usage":{"input_tokens":40,"output_tokens":120,"total_tokens":160,"input_tokens_details":{"cached_tokens":0}}}"#;
    let mut conn = H2Conn::new();
    let mut poll = None;
    {
        let mut on = |e| {
            if let H2Event::Poll { id, usage, .. } = e {
                poll = Some((id, usage.map(|u| (u.prompt_tokens, u.completion_tokens))));
            }
        };
        conn.feed(LlmDirection::Write, PREFACE, H2Gap::None, &mut on);
        conn.feed(
            LlmDirection::Write,
            &req_full(b"GET", b"/v1/responses/resp_bg1", 1),
            H2Gap::None,
            &mut on,
        );
        conn.feed(LlmDirection::Read, &head, H2Gap::None, &mut on);
        conn.feed(
            LlmDirection::Read,
            &frame(0, 0x1, 1, done),
            H2Gap::None,
            &mut on,
        );
    }
    assert_eq!(poll, Some(("resp_bg1".into(), Some((40, 120)))));
}

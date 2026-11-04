//! One HTTP/2 connection: frames demultiplexed into streams, each direction's HPACK table its own.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use honeybeepf_llm_core::h2::frame::{Frame, FrameReader, PREFACE};
use honeybeepf_llm_core::h2::hpack::{Decoder, Text};
use honeybeepf_llm_core::types::{LlmDirection, UsageInfo};

use super::super::registry::PROVIDER_REGISTRY;
use super::stream::H2Stream;
use crate::probes::builtin::llm::processor::UnparsedReason;

/// Streams tracked at once; a connection rarely runs many in parallel (SETTINGS_MAX_CONCURRENT_STREAMS ~100).
const MAX_STREAMS: usize = 128;
/// Response body bytes buffered across a connection's streams; past it, further DATA is dropped so that
/// many large responses on one connection cannot each reach the per-stream cap.
const CONN_BUFFER_CAP: usize = 32 * 1024 * 1024;
/// A stream whose response began but then saw no frame for this long on a still-live connection is dropped
/// and counted, so a stalled response frees its buffered bytes instead of being held forever. A stream still
/// awaiting its first response byte is left alone (a slow time-to-first-byte is not an abandoned stream).
const STREAM_IDLE: Duration = Duration::from_secs(300);

/// What feeding a connection produced, for the processor to log and record.
pub(crate) enum H2Event {
    Detected {
        path: String,
    },
    Recorded {
        usage: UsageInfo,
        start: Instant,
    },
    /// A tracked stream ended without usage (reset, connection going away, idle, or over a cap).
    Unparsed {
        reason: UnparsedReason,
    },
}

/// A frame reader and the HPACK table of one direction's encoder.
struct Dir {
    reader: FrameReader,
    decoder: Decoder,
}

/// The per-connection HTTP/2 state hung on a `(pid, SSL*)` processor, dropped with it on SSL_free.
pub(crate) struct H2Conn {
    client: Dir,
    server: Dir,
    streams: BTreeMap<u32, H2Stream>,
    /// Response body bytes buffered across all streams, capped at `CONN_BUFFER_CAP`.
    buffered: usize,
}

impl H2Conn {
    pub(crate) fn new() -> Self {
        Self {
            client: Dir {
                reader: FrameReader::client(),
                decoder: Decoder::new(),
            },
            server: Dir {
                reader: FrameReader::server(),
                decoder: Decoder::new(),
            },
            streams: BTreeMap::new(),
            buffered: 0,
        }
    }

    /// Read one direction's bytes, emitting an event as each matched stream is detected and finished.
    pub(crate) fn feed(
        &mut self,
        direction: LlmDirection,
        data: &[u8],
        mut on: impl FnMut(H2Event),
    ) {
        let Self {
            client,
            server,
            streams,
            buffered,
        } = self;
        let request = direction == LlmDirection::Write;
        // Disjoint borrows: the reader walks frames while the callback holds the decoder and stream map.
        let Dir { reader, decoder } = if request { client } else { server };
        reader.read(data, |frame| {
            on_frame(frame, request, decoder, streams, buffered, &mut on)
        });
        // A live connection's traffic is the only clock here, so drop idle streams as it flows.
        if !streams.is_empty() {
            sweep_idle(streams, buffered, Instant::now(), &mut on);
        }
    }

    /// Tracked streams still open, each a detected call with no usage yet; counted unparsed at close.
    pub(crate) fn open_streams(&self) -> usize {
        self.streams.len()
    }
}

fn on_frame(
    frame: Frame<'_>,
    request: bool,
    decoder: &mut Decoder,
    streams: &mut BTreeMap<u32, H2Stream>,
    buffered: &mut usize,
    on: &mut impl FnMut(H2Event),
) {
    match frame {
        // Decode the request block (to keep the client table in sync) and track it only when it matched a
        // provider, so non-LLM streams never take a map slot.
        Frame::Headers { stream, block } if request => {
            if let Some(path) = match_request(decoder, block)
                && !streams.contains_key(&stream)
                && streams.len() < MAX_STREAMS
            {
                streams.insert(stream, H2Stream::open(Instant::now()));
                on(H2Event::Detected { path });
            }
        }
        // A response block of a tracked stream picks how its body is read; others are only decoded, to keep
        // the table in sync. A block after DATA is trailers and `response_headers` leaves the body as is.
        Frame::Headers { stream, block } => {
            if streams.contains_key(&stream) {
                let (content_type, coding) = collect_response(decoder, block);
                if let Some(s) = streams.get_mut(&stream) {
                    s.response_headers(content_type.as_deref(), &coding);
                }
            } else {
                let _ = decoder.decode(block, |_| {});
            }
        }
        // The block's table updates were not decoded, so later indexes in this direction would be wrong.
        Frame::HeadersTooLarge { stream } => {
            decoder.forget();
            if let Some(s) = streams.get(&stream) {
                *buffered = buffered.saturating_sub(s.held());
            }
            streams.remove(&stream);
        }
        // Feed every stream (SSE keeps almost nothing); the connection budget only bounds buffered bodies.
        Frame::Data { stream, data } if !request => {
            if let Some(s) = streams.get_mut(&stream) {
                let room = CONN_BUFFER_CAP.saturating_sub(*buffered);
                *buffered = buffered.saturating_add_signed(s.data(data, room));
            }
        }
        Frame::StreamEnd { stream } if !request => {
            if let Some(s) = streams.get_mut(&stream) {
                if let Some(usage) = s.finish() {
                    on(H2Event::Recorded {
                        usage,
                        start: s.start(),
                    });
                } else if s.over_limit() {
                    on(H2Event::Unparsed {
                        reason: UnparsedReason::ResponseTooLarge,
                    });
                }
                *buffered = buffered.saturating_sub(s.held());
            }
            streams.remove(&stream);
        }
        // A server RST_STREAM rejects the request, so the stream ended with no usage. A client RST on its
        // own stream is often routine cleanup after the response (undici does this), so it is left alone;
        // a real client abort is then caught by the idle sweep or at close.
        Frame::Reset { stream } if !request => drop_stream(stream, streams, buffered, on),
        // Only the server's GOAWAY bounds our client-initiated streams (a client GOAWAY's last-stream-id
        // names server-pushed streams, which we never track). Streams above it are abandoned; the rest may
        // still finish.
        Frame::Goaway { last_stream } if !request => {
            let gone: Vec<u32> = streams
                .range(last_stream + 1..)
                .map(|(id, _)| *id)
                .collect();
            for stream in gone {
                drop_stream(stream, streams, buffered, on);
            }
        }
        // Request DATA and END_STREAM carry nothing to record; `Frame` is non-exhaustive.
        _ => {}
    }
}

/// Drop every stream whose response began but then stalled since before `now - STREAM_IDLE`, counting each
/// as closed. A stream still awaiting its first response byte is kept, so a slow call is not lost.
fn sweep_idle(
    streams: &mut BTreeMap<u32, H2Stream>,
    buffered: &mut usize,
    now: Instant,
    on: &mut impl FnMut(H2Event),
) {
    streams.retain(|_, s| {
        let keep = !s.started() || now.duration_since(s.last()) < STREAM_IDLE;
        if !keep {
            *buffered = buffered.saturating_sub(s.held());
            on(H2Event::Unparsed {
                reason: UnparsedReason::ConnectionClosed,
            });
        }
        keep
    });
}

/// Drop a tracked stream that ended before its usage was read, returning its bytes to the budget and
/// counting the call as closed. An untracked stream (never a provider POST) is ignored.
fn drop_stream(
    stream: u32,
    streams: &mut BTreeMap<u32, H2Stream>,
    buffered: &mut usize,
    on: &mut impl FnMut(H2Event),
) {
    if let Some(s) = streams.remove(&stream) {
        *buffered = buffered.saturating_sub(s.held());
        on(H2Event::Unparsed {
            reason: UnparsedReason::ConnectionClosed,
        });
    }
}

/// Decode a request header block (keeping the HPACK table in sync) and return the matched path when it is a
/// POST a provider serves. A block that failed to decode stays unmatched: its fields are unreliable (hpack.rs).
fn match_request(decoder: &mut Decoder, block: &[u8]) -> Option<String> {
    let mut is_post = false;
    let mut authority: Option<Vec<u8>> = None;
    let mut path: Option<Vec<u8>> = None;
    let decoded = decoder.decode(block, |field| {
        let (Text::Known(name), Text::Known(value)) = (field.name, field.value) else {
            return;
        };
        if name == b":method" {
            is_post = value == b"POST";
        } else if name == b":authority" {
            authority = Some(value.to_vec());
        } else if name == b":path" {
            path = Some(value.to_vec());
        }
    });
    if decoded.is_err() || !is_post {
        return None;
    }
    let authority = std::str::from_utf8(authority.as_deref()?).ok()?;
    let path = std::str::from_utf8(path.as_deref()?).ok()?;
    PROVIDER_REGISTRY
        .find_provider(authority, path)
        .map(|_| path.split_once('?').map_or(path, |(p, _)| p).to_string())
}

/// Decode a response header block (keeping the table in sync) and return its content type and the content
/// codings joined with commas, which pick how the body is read.
fn collect_response(decoder: &mut Decoder, block: &[u8]) -> (Option<Vec<u8>>, Vec<u8>) {
    // A real content coding is a short token list; `encoding::decode` takes at most four codings anyway.
    const MAX_CODING: usize = 256;
    let mut content_type = None;
    let mut coding = Vec::new();
    let decoded = decoder.decode(block, |field| {
        let (Text::Known(name), Text::Known(value)) = (field.name, field.value) else {
            return;
        };
        if name == b"content-type" {
            content_type = Some(value.to_vec());
        } else if name == b"content-encoding" && coding.len() < MAX_CODING {
            if !coding.is_empty() {
                coding.push(b',');
            }
            coding.extend_from_slice(value);
        }
    });
    // A block that failed to decode left unreliable fields (hpack.rs); the table is already forgotten.
    if decoded.is_err() {
        return (None, Vec::new());
    }
    (content_type, coding)
}

/// Whether the bytes begin the client connection preface, so the connection is HTTP/2. The whole preface
/// is required, so a one-byte `P` write cannot be mistaken for it (it could start `POST`).
pub(crate) fn starts_preface(data: &[u8]) -> bool {
    data.starts_with(&PREFACE[..])
}

#[cfg(test)]
mod tests {
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

    /// Only a POST a provider serves is tracked, so a non-provider host or a GET takes no map slot.
    #[test]
    fn match_request_detects_only_provider_posts() {
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
        assert_eq!(m(&openai).as_deref(), Some("/v1/chat/completions"));
        assert_eq!(m(&block(b"POST", "noise.internal", "/p")), None);
        assert_eq!(
            m(&block(b"GET", "api.openai.com", "/v1/chat/completions")),
            None
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
            conn.feed(LlmDirection::Write, PREFACE, &mut on);
            conn.feed(LlmDirection::Write, &req(1), &mut on);
            for r in reads {
                conn.feed(LlmDirection::Read, r, &mut on);
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
                H2Event::Unparsed { .. } => {}
            };
            for chunk in c2s.chunks(step) {
                conn.feed(LlmDirection::Write, chunk, &mut on);
            }
            for chunk in s2c.chunks(step) {
                conn.feed(LlmDirection::Read, chunk, &mut on);
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
                H2Event::Detected { .. } => {}
            };
            conn.feed(LlmDirection::Write, PREFACE, &mut on);
            conn.feed(LlmDirection::Write, &req(1), &mut on);
            for (dir, bytes) in reads {
                conn.feed(*dir, bytes, &mut on);
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

    /// Opens stream 1 and feeds it `response` reads, returning the live `H2Conn` for an idle-sweep check.
    fn conn_with_open_stream(response: &[&[u8]]) -> H2Conn {
        let mut conn = H2Conn::new();
        let mut on = |_| {};
        conn.feed(LlmDirection::Write, PREFACE, &mut on);
        conn.feed(LlmDirection::Write, &req(1), &mut on);
        for r in response {
            conn.feed(LlmDirection::Read, r, &mut on);
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
}

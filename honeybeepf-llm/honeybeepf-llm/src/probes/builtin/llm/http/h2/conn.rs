//! One HTTP/2 connection: frames demultiplexed into streams, each direction's HPACK table its own.

use std::collections::BTreeMap;
use std::time::Instant;

use honeybeepf_llm_core::h2::frame::{Frame, FrameReader, PREFACE};
use honeybeepf_llm_core::h2::hpack::{Decoder, Text};
use honeybeepf_llm_core::types::{LlmDirection, UsageInfo};

use super::super::registry::PROVIDER_REGISTRY;
use super::stream::H2Stream;

/// Streams tracked at once; a connection rarely runs many in parallel (SETTINGS_MAX_CONCURRENT_STREAMS ~100).
const MAX_STREAMS: usize = 128;
/// Response body bytes buffered across a connection's streams; past it, further DATA is dropped so that
/// many large responses on one connection cannot each reach the per-stream cap.
const CONN_BUFFER_CAP: usize = 32 * 1024 * 1024;

/// What feeding a connection produced, for the processor to log and record.
pub(crate) enum H2Event {
    Detected { path: String },
    Recorded { usage: UsageInfo, start: Instant },
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
                }
                *buffered = buffered.saturating_sub(s.held());
            }
            streams.remove(&stream);
        }
        // Request DATA and END_STREAM carry nothing to record; `Frame` is non-exhaustive.
        _ => {}
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
}

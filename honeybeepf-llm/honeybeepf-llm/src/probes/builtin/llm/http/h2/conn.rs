//! One HTTP/2 connection: frames demultiplexed into streams, each direction's HPACK table its own.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use honeybeepf_llm_core::h2::frame::{Frame, FrameReader, PREFACE};
use honeybeepf_llm_core::h2::hpack::{Decoder, Text};
use honeybeepf_llm_core::types::{LlmDirection, UsageInfo};

use super::super::body::{BackgroundState, is_response_id};
use super::super::registry::PROVIDER_REGISTRY;
use super::stream::H2Stream;
use crate::probes::builtin::llm::processor::{H2Gap, UnparsedReason};

/// Streams tracked at once; a connection rarely runs many in parallel (SETTINGS_MAX_CONCURRENT_STREAMS ~100).
const MAX_STREAMS: usize = 128;
/// Response body bytes buffered across a connection's streams; past it, further DATA is dropped so that
/// many large responses on one connection cannot each reach the per-stream cap.
const CONN_BUFFER_CAP: usize = 32 * 1024 * 1024;
/// A stream whose response began but then saw no frame for this long on a still-live connection is dropped
/// and counted, so a stalled response frees its buffered bytes instead of being held forever. A stream still
/// awaiting its first response byte is left alone (a slow time-to-first-byte is not an abandoned stream).
const STREAM_IDLE: Duration = Duration::from_secs(300);
/// A mid-join connection's streams are guesses (no request matched them), so its total buffering is held far
/// below `CONN_BUFFER_CAP`; a non-LLM join (internal gRPC, …) must not be able to pile up per connection.
const MIDJOIN_CONN_CAP: usize = 1024 * 1024;
/// Speculative mid-join bytes buffered across every connection on the node, so many non-LLM joins at once
/// (e.g. right after the agent restarts) cannot add up to gigabytes.
const MIDJOIN_NODE_CAP: usize = 64 * 1024 * 1024;
/// Bytes currently buffered by mid-join connections node-wide; each connection reconciles its share per feed.
static MIDJOIN_NODE_BYTES: AtomicUsize = AtomicUsize::new(0);

/// What feeding a connection produced, for the processor to log and record.
pub(crate) enum H2Event {
    Detected {
        path: String,
    },
    Recorded {
        usage: Box<UsageInfo>,
        start: Instant,
        /// The connection was joined mid-stream, so this call's host is unknown and it is lower-confidence.
        midjoin: bool,
    },
    /// A Responses create made with `"background": true` returned queued; its usage arrives in a later poll,
    /// so the call is kept under `id` until then.
    Deferred {
        id: Box<str>,
        start: Instant,
    },
    /// A poll (`GET <responses path>/{id}`) of a background call ended; its result settles the kept call.
    Poll {
        id: Box<str>,
        usage: Option<Box<UsageInfo>>,
        background: Option<BackgroundState>,
    },
    /// A tracked stream ended without usage (reset, connection going away, idle, or over a cap).
    Unparsed {
        reason: UnparsedReason,
    },
    /// A direction re-anchored its frame reader after a capture gap.
    Resynced,
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
    /// Joined mid-stream (no preface seen): the HPACK tables are unknown, so requests cannot be matched;
    /// any response stream is tracked and its usage read from DATA, with the host unknown.
    midjoin: bool,
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
            midjoin: false,
        }
    }

    /// A connection joined after its preface: neither direction expects a preface, and the first feed of
    /// each (marked lost by the processor) re-anchors it and forgets its HPACK table.
    pub(crate) fn new_midjoin() -> Self {
        let mut conn = Self::new();
        conn.client.reader = FrameReader::server();
        conn.midjoin = true;
        conn
    }

    /// Read one direction's bytes, emitting an event as each matched stream is detected and finished.
    /// `gap` says how the bytes are discontinuous: a known lost count is skipped inside the current frame
    /// when it fits, keeping the HPACK table; otherwise the reader re-anchors and that table, now out of
    /// sync, is forgotten (later indexed fields decode as unknown until a fresh literal).
    pub(crate) fn feed(
        &mut self,
        direction: LlmDirection,
        data: &[u8],
        gap: H2Gap,
        mut on: impl FnMut(H2Event),
    ) {
        let Self {
            client,
            server,
            streams,
            buffered,
            midjoin,
        } = self;
        let request = direction == LlmDirection::Write;
        // Disjoint borrows: the reader walks frames while the callback holds the decoder and stream map.
        let Dir { reader, decoder } = if request { client } else { server };
        // A gap whose exact size fits inside the current DATA frame is skipped, keeping framing and the HPACK
        // table; anything else re-anchors and forgets the table.
        let resync = match gap {
            H2Gap::None => false,
            H2Gap::Skip(n) => !reader.skip_gap(n),
            H2Gap::Resync => true,
        };
        if resync {
            reader.resync();
            decoder.forget();
            on(H2Event::Resynced);
            // A forgotten client table can no longer match requests, so from here the connection tracks
            // responses speculatively (host unknown), as a mid-stream join does.
            if request {
                *midjoin = true;
            }
        }
        let midjoin = *midjoin;
        // A normal connection buffers up to the full budget; a mid-join one is held to a small per-connection
        // cap and the node-wide speculative budget, so unmatched (possibly non-LLM) joins cannot pile up.
        let before = *buffered;
        let cap = if midjoin {
            let node_room =
                MIDJOIN_NODE_CAP.saturating_sub(MIDJOIN_NODE_BYTES.load(Ordering::Relaxed));
            MIDJOIN_CONN_CAP.min(before.saturating_add(node_room))
        } else {
            CONN_BUFFER_CAP
        };
        reader.read(data, |frame| {
            on_frame(
                frame, request, midjoin, cap, decoder, streams, buffered, &mut on,
            )
        });
        // A live connection's traffic is the only clock here, so drop idle streams as it flows.
        if !streams.is_empty() {
            sweep_idle(streams, buffered, Instant::now(), &mut on);
        }
        if midjoin {
            reconcile_node_bytes(before, *buffered);
        }
    }

    /// Tracked streams still open, each a detected call with no usage yet; counted unparsed at close.
    pub(crate) fn open_streams(&self) -> usize {
        self.streams.len()
    }
}

impl Drop for H2Conn {
    fn drop(&mut self) {
        // Return this mid-join connection's share of the node-wide speculative budget.
        if self.midjoin && self.buffered > 0 {
            MIDJOIN_NODE_BYTES.fetch_sub(self.buffered, Ordering::Relaxed);
        }
    }
}

/// Apply a mid-join connection's net buffering change over one feed to the node-wide total.
fn reconcile_node_bytes(before: usize, after: usize) {
    if after > before {
        MIDJOIN_NODE_BYTES.fetch_add(after - before, Ordering::Relaxed);
    } else if before > after {
        MIDJOIN_NODE_BYTES.fetch_sub(before - after, Ordering::Relaxed);
    }
}

/// Whether a first DATA chunk could begin a usage JSON object or an SSE body, so a mid-join stream is worth
/// tracking. Length-prefixed binary (gRPC), gzip and anything else is abandoned without buffering.
fn looks_recordable(data: &[u8]) -> bool {
    let start = data
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .map_or(&[][..], |i| &data[i..]);
    start.first() == Some(&b'{') || start.starts_with(b"data:") || start.starts_with(b"event:")
}

#[allow(clippy::too_many_arguments)]
fn on_frame(
    frame: Frame<'_>,
    request: bool,
    midjoin: bool,
    cap: usize,
    decoder: &mut Decoder,
    streams: &mut BTreeMap<u32, H2Stream>,
    buffered: &mut usize,
    on: &mut impl FnMut(H2Event),
) {
    match frame {
        // Decode the request block (to keep the client table in sync) and track it only when it matched a
        // provider, so non-LLM streams never take a map slot.
        Frame::Headers { stream, block } if request => {
            if let Some(matched) = match_request(decoder, block)
                && !streams.contains_key(&stream)
                && streams.len() < MAX_STREAMS
            {
                let (path, s) = match matched {
                    Matched::Create { path, record_only } => {
                        (path, H2Stream::open(Instant::now(), record_only))
                    }
                    Matched::Poll { path, id } => (path, H2Stream::open_poll(Instant::now(), id)),
                };
                streams.insert(stream, s);
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
        // Feed every stream; `cap` bounds buffered bodies. On a mid-stream join the request HEADERS were
        // missed, so a response is tracked on its first DATA (host unknown) only when that DATA could be a
        // usage JSON or SSE body — framed binary (gRPC, gzip) is abandoned at once (`looks_recordable`).
        Frame::Data { stream, data } if !request => {
            if midjoin
                && !streams.contains_key(&stream)
                && streams.len() < MAX_STREAMS
                && looks_recordable(data)
            {
                streams.insert(stream, H2Stream::open_speculative(Instant::now()));
            }
            if let Some(s) = streams.get_mut(&stream) {
                let room = cap.saturating_sub(*buffered);
                *buffered = buffered.saturating_add_signed(s.data(data, room));
            }
        }
        Frame::StreamEnd { stream } if !request => {
            if let Some(s) = streams.remove(&stream) {
                *buffered = buffered.saturating_sub(s.held());
                end_stream(s, on);
            }
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

/// Turn a finished stream into the event the processor records. A speculative stream (host unknown) keeps
/// its old handling; a matched stream routes a background create's `queued` result to `Deferred` and a poll
/// to `Poll`, so its usage settles against the node-wide background-calls map.
fn end_stream(s: H2Stream, on: &mut impl FnMut(H2Event)) {
    if s.record_only() {
        // A Managed Agents session call: counted at its end, its running session total never read.
        on(H2Event::Unparsed {
            reason: UnparsedReason::AgentSession,
        });
        return;
    }
    let (over, speculative, start) = (s.over_limit(), s.speculative(), s.start());
    let poll = s.poll().map(Box::<str>::from);
    let (usage, background) = s.finish();
    if speculative {
        if let Some(usage) = usage {
            on(H2Event::Recorded {
                usage: Box::new(usage),
                start,
                midjoin: true,
            });
        } else if over {
            on(H2Event::Unparsed {
                reason: UnparsedReason::ResponseTooLarge,
            });
        } else {
            // A response tracked without its request (lost headers or a mid-join) that carried no usage:
            // count it, since it may have been an LLM call we could not identify.
            on(H2Event::Unparsed {
                reason: UnparsedReason::HeadersLost,
            });
        }
        return;
    }
    if let Some(id) = poll {
        on(H2Event::Poll {
            id,
            usage: usage.map(Box::new),
            background,
        });
    } else if let Some(usage) = usage {
        on(H2Event::Recorded {
            usage: Box::new(usage),
            start,
            midjoin: false,
        });
    } else if over {
        on(H2Event::Unparsed {
            reason: UnparsedReason::ResponseTooLarge,
        });
    } else if let Some(BackgroundState::Running(id)) = background {
        on(H2Event::Deferred { id, start });
    }
    // A matched stream with no usage and no background state records nothing.
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

/// What a request HEADERS block is, for the stream map.
enum Matched {
    /// A POST a provider serves: a generation call, or a Responses create. `record_only` marks a Managed
    /// Agents session call, counted at its end without reading its running session total.
    Create { path: String, record_only: bool },
    /// A `GET <responses path>/{id}` poll of a background Responses call.
    Poll { path: String, id: Box<str> },
}

/// Decode a request header block (keeping the HPACK table in sync) and classify it: a provider POST, or a
/// GET poll of a background Responses id. A block that failed to decode stays unmatched: its fields are
/// unreliable (hpack.rs).
fn match_request(decoder: &mut Decoder, block: &[u8]) -> Option<Matched> {
    let mut method: Option<Vec<u8>> = None;
    let mut authority: Option<Vec<u8>> = None;
    let mut path: Option<Vec<u8>> = None;
    let decoded = decoder.decode(block, |field| {
        let (Text::Known(name), Text::Known(value)) = (field.name, field.value) else {
            return;
        };
        if name == b":method" {
            method = Some(value.to_vec());
        } else if name == b":authority" {
            authority = Some(value.to_vec());
        } else if name == b":path" {
            path = Some(value.to_vec());
        }
    });
    if decoded.is_err() {
        return None;
    }
    let authority = std::str::from_utf8(authority.as_deref()?).ok()?;
    let path = std::str::from_utf8(path.as_deref()?).ok()?;
    let base = path.split_once('?').map_or(path, |(p, _)| p);
    match method.as_deref() {
        Some(b"POST") => {
            PROVIDER_REGISTRY
                .find_provider(authority, path)
                .map(|p| Matched::Create {
                    path: base.to_string(),
                    record_only: p.record_only,
                })
        }
        Some(b"GET") => responses_poll_id(authority, base).map(|id| Matched::Poll {
            path: base.to_string(),
            id,
        }),
        _ => None,
    }
}

/// The background Responses id in `GET <provider responses path>/{id}` (query already stripped), or None.
/// Restricted to a `/responses` parent so a stored-completion or message retrieval is not taken for a poll.
fn responses_poll_id(authority: &str, base: &str) -> Option<Box<str>> {
    let (parent, id) = base.rsplit_once('/')?;
    (parent.ends_with("/responses")
        && is_response_id(id)
        && PROVIDER_REGISTRY.find_provider(authority, parent).is_some())
    .then(|| id.into())
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
        let queued =
            br#"{"id":"resp_bg1","object":"response","status":"queued","background":true}"#;
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
}

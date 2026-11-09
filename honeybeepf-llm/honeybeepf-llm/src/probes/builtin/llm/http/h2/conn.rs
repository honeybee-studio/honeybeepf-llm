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
const STREAM_IDLE: Duration = Duration::from_mins(5);
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
            );
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
                let (status, content_type, coding) = collect_response(decoder, block);
                if let Some(s) = streams.get_mut(&stream) {
                    s.response_headers(status, content_type.as_deref(), &coding);
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
    let (success, errored) = (s.is_success(), s.errored());
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
    } else if errored {
        // The stream ended on a provider `event: error` frame (a 200 whose SSE aborted), told apart
        // from a usage-absent completion and from capture loss.
        on(H2Event::Unparsed {
            reason: UnparsedReason::StreamError,
        });
    } else if success {
        // A matched 2xx call whose completed stream carried no usage (an embedding with none, or a chunk
        // missed), apart from the capture-loss and too-large reasons; a non-2xx end records nothing.
        on(H2Event::Unparsed {
            reason: UnparsedReason::UsageAbsent,
        });
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
fn collect_response(
    decoder: &mut Decoder,
    block: &[u8],
) -> (Option<u16>, Option<Vec<u8>>, Vec<u8>) {
    // A real content coding is a short token list; `encoding::decode` takes at most four codings anyway.
    const MAX_CODING: usize = 256;
    let mut status = None;
    let mut content_type = None;
    let mut coding = Vec::new();
    let decoded = decoder.decode(block, |field| {
        let (Text::Known(name), Text::Known(value)) = (field.name, field.value) else {
            return;
        };
        if name == b":status" {
            status = std::str::from_utf8(value).ok().and_then(|s| s.parse().ok());
        } else if name == b"content-type" {
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
        return (None, None, Vec::new());
    }
    (status, content_type, coding)
}

/// Whether the bytes begin the client connection preface, so the connection is HTTP/2. The whole preface
/// is required, so a one-byte `P` write cannot be mistaken for it (it could start `POST`).
pub(crate) fn starts_preface(data: &[u8]) -> bool {
    data.starts_with(&PREFACE[..])
}

#[cfg(test)]
mod tests;

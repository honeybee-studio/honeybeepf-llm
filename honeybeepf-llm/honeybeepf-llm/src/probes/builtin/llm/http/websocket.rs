//! WebSocket calls (OpenAI Realtime, Responses over a WebSocket): the upgrade request, the server's 101
//! answer, and the server's frames, read only for the messages that start a response, end it with its usage,
//! or name the session's model. Client frames are never read, and every other message is skipped unkept.

use flate2::{Decompress, FlushDecompress, Status};
use honeybeepf_llm_core::types::UsageInfo;
use serde_json::Value;

use super::{MatchedBy, body::parse_response_json_value, matched_by, registry::PROVIDER_REGISTRY};

/// Longest 101 answer read; real ones are under 1 KB.
const MAX_HANDSHAKE: usize = 16 * 1024;
/// Message bytes read to find its `type`, which OpenAI sends first.
const TYPE_WINDOW: usize = 256;
/// A response-start or session message kept whole; a longer one is skipped.
const MAX_SMALL: usize = 64 * 1024;
/// A usage message kept whole; past this only its first and last bytes are kept.
const MAX_WHOLE: usize = 256 * 1024;
/// Bytes kept from each end of a usage message past `MAX_WHOLE`.
const ENDS_KEEP: usize = 64 * 1024;
/// Longest message, after inflating; a longer one ends the reading, so a deflate bomb costs at most this.
pub(crate) const MAX_MESSAGE: usize = 16 * 1024 * 1024;
/// Inflate output per step.
const INFLATE_STEP: usize = 16 * 1024;
/// What RFC 7692 strips from the end of each compressed message.
const DEFLATE_TAIL: [u8; 4] = [0, 0, 0xff, 0xff];

/// A WebSocket upgrade request to a listed provider's Realtime or Responses path.
#[derive(Debug, PartialEq, Eq)]
pub struct Upgrade {
    /// The path without its query, which can carry a credential (Azure `api-key=`).
    pub path: String,
    /// `model` or `deployment` from the query, the only query parameters read.
    pub model: Option<String>,
    pub matched: MatchedBy,
}

/// The WebSocket upgrade a complete GET head asks for; only a listed host counts, not a shape match.
pub fn detect_upgrade(buffer: &[u8]) -> Option<Upgrade> {
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut req = httparse::Request::new(&mut headers);
    if !req.parse(buffer).ok()?.is_complete() || req.method != Some("GET") {
        return None;
    }
    let header = |name: &str| {
        req.headers
            .iter()
            .find(|h| h.name.eq_ignore_ascii_case(name))
            .map(|h| String::from_utf8_lossy(h.value))
    };
    if !header("Upgrade")?.trim().eq_ignore_ascii_case("websocket") {
        return None;
    }
    let host = header("Host").unwrap_or_default();
    let path = req.path?;
    let provider = PROVIDER_REGISTRY.find_provider(&host, path)?;
    if provider.record_only {
        return None;
    }
    let (path, query) = path.split_once('?').unwrap_or((path, ""));
    let model = query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(key, _)| matches!(*key, "model" | "deployment"))
        .and_then(|(_, value)| super::request_model::from_query(value));
    Some(Upgrade {
        path: path.to_string(),
        model,
        matched: matched_by(Some(provider)),
    })
}

/// How far the server's answer to the upgrade has come.
#[derive(Debug, PartialEq, Eq)]
pub enum Handshake {
    /// The head has not ended yet.
    Partial,
    /// `101 Switching Protocols`: frames follow the head's `len` bytes, compressed when `deflate` is set.
    Switched {
        len: usize,
        deflate: Option<Deflate>,
    },
    /// Any other answer, or an extension that changes the framing in a way not read.
    Refused,
}

/// The permessage-deflate parameters the server agreed to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Deflate {
    /// The server starts each message's compression afresh, so the inflater is reset after each.
    pub no_context_takeover: bool,
}

/// Read the server's answer to the upgrade from the start of `buf`.
pub fn read_handshake(buf: &[u8]) -> Handshake {
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut resp = httparse::Response::new(&mut headers);
    let len = match resp.parse(buf) {
        Ok(httparse::Status::Complete(len)) => len,
        Ok(httparse::Status::Partial) if buf.len() < MAX_HANDSHAKE => return Handshake::Partial,
        _ => return Handshake::Refused,
    };
    if resp.code != Some(101) {
        return Handshake::Refused;
    }
    let mut deflate = None;
    for h in resp.headers.iter() {
        if !h.name.eq_ignore_ascii_case("Sec-WebSocket-Extensions") {
            continue;
        }
        for extension in String::from_utf8_lossy(h.value).split(',') {
            let mut params = extension.split(';').map(str::trim);
            if params.next() != Some("permessage-deflate") || deflate.is_some() {
                return Handshake::Refused;
            }
            deflate = Some(Deflate {
                no_context_takeover: params.any(|p| p == "server_no_context_takeover"),
            });
        }
    }
    Handshake::Switched { len, deflate }
}

/// What one server message told.
#[derive(Debug, PartialEq)]
pub enum WsEvent {
    /// A response started (`response.created`).
    Started(Box<str>),
    /// A response ended with this usage (`response.done`, `response.completed`, `failed`, `incomplete`).
    Ended(Option<Box<str>>, Box<UsageInfo>),
    /// The session's model (`session.created`, `session.updated`).
    Model(String),
}

/// Why the server's frames cannot be read past this point; the connection is then left alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WsError {
    /// Not a frame a server may send: masked, reserved bits or opcodes, a control frame too long or split.
    Protocol,
    /// A message past `MAX_MESSAGE`.
    TooLarge,
    /// The compressed stream is broken.
    Inflate,
}

/// The server's half of a WebSocket, read frame by frame.
pub struct ServerFrames {
    /// Head bytes of the next frame, at most 10 since a server frame has no mask.
    head: [u8; 10],
    head_len: usize,
    /// Payload bytes left in the current frame, and whether it is a control frame.
    left: u64,
    control: bool,
    fin: bool,
    /// The data message the current frame belongs to.
    message: Option<Message>,
    inflater: Option<Box<Inflater>>,
}

struct Inflater {
    state: Decompress,
    no_context_takeover: bool,
}

struct Message {
    compressed: bool,
    /// Bytes of the message so far, after inflating.
    size: usize,
    sink: Sink,
}

/// Where a message's bytes go once its type is known.
enum Sink {
    /// The first bytes, read for the type.
    Typing(Vec<u8>),
    Small(SmallKind, Vec<u8>),
    Usage(UsageBytes),
    Skip,
}

#[derive(Clone, Copy)]
enum SmallKind {
    Started,
    Session,
}

/// A usage message: whole up to `MAX_WHOLE`, then its two ends.
enum UsageBytes {
    Whole(Vec<u8>),
    Ends { head: Vec<u8>, tail: Vec<u8> },
}

impl ServerFrames {
    pub fn new(deflate: Option<Deflate>) -> Self {
        Self {
            head: [0; 10],
            head_len: 0,
            left: 0,
            control: false,
            fin: false,
            message: None,
            inflater: deflate.map(|d| {
                Box::new(Inflater {
                    state: Decompress::new(false),
                    no_context_takeover: d.no_context_takeover,
                })
            }),
        }
    }

    /// Read the next server bytes, adding what their messages told to `events`.
    pub fn feed(&mut self, mut data: &[u8], events: &mut Vec<WsEvent>) -> Result<(), WsError> {
        while !data.is_empty() {
            if self.left == 0 && !self.in_payload() {
                let taken = self.read_head(data)?;
                data = &data[taken..];
                if !self.in_payload() {
                    continue;
                }
                if self.left == 0 {
                    self.end_frame(events)?;
                    continue;
                }
            }
            let n = usize::try_from(self.left).map_or(data.len(), |left| left.min(data.len()));
            let (chunk, rest) = data.split_at(n);
            data = rest;
            self.left -= n as u64;
            if !self.control {
                self.message_bytes(chunk, false)?;
            }
            if self.left == 0 {
                self.end_frame(events)?;
            }
        }
        Ok(())
    }

    /// Whether a frame head has been read and its payload is being read.
    fn in_payload(&self) -> bool {
        self.head_len == usize::MAX
    }

    /// Take head bytes from `data`; returns how many were used. A whole head starts the frame's payload.
    fn read_head(&mut self, data: &[u8]) -> Result<usize, WsError> {
        let mut used = 0;
        while used < data.len() {
            self.head[self.head_len] = data[used];
            self.head_len += 1;
            used += 1;
            let want = match self.head_len {
                1 => continue,
                _ => match self.head[1] & 0x7f {
                    126 => 4,
                    127 => 10,
                    _ => 2,
                },
            };
            if self.head_len == want {
                self.start_frame()?;
                return Ok(used);
            }
        }
        Ok(used)
    }

    fn start_frame(&mut self) -> Result<(), WsError> {
        let [b0, b1] = [self.head[0], self.head[1]];
        let (fin, rsv1, opcode) = (b0 & 0x80 != 0, b0 & 0x40 != 0, b0 & 0x0f);
        // A server never masks; reserved bits other than deflate's are never set.
        if b1 & 0x80 != 0 || b0 & 0x30 != 0 {
            return Err(WsError::Protocol);
        }
        let len = match b1 & 0x7f {
            126 => u64::from(u16::from_be_bytes([self.head[2], self.head[3]])),
            127 => u64::from_be_bytes(self.head[2..10].try_into().map_err(|_| WsError::Protocol)?),
            n => u64::from(n),
        };
        let control = opcode & 0x08 != 0;
        match opcode {
            0x8..=0xa if fin && len <= 125 && !rsv1 => {}
            0x1 | 0x2 if self.message.is_none() && (!rsv1 || self.inflater.is_some()) => {
                self.message = Some(Message {
                    compressed: rsv1,
                    size: 0,
                    sink: Sink::Typing(Vec::new()),
                });
            }
            0x0 if self.message.is_some() && !rsv1 => {}
            _ => return Err(WsError::Protocol),
        }
        if len > MAX_MESSAGE as u64 {
            return Err(WsError::TooLarge);
        }
        self.head_len = usize::MAX;
        self.left = len;
        self.control = control;
        self.fin = fin;
        Ok(())
    }

    fn end_frame(&mut self, events: &mut Vec<WsEvent>) -> Result<(), WsError> {
        self.head_len = 0;
        if self.control || !self.fin {
            return Ok(());
        }
        if self.message.as_ref().is_some_and(|m| m.compressed) {
            self.message_bytes(&DEFLATE_TAIL, true)?;
        }
        if let Some(inflater) = &mut self.inflater
            && inflater.no_context_takeover
        {
            inflater.state.reset(false);
        }
        if let Some(event) = self.message.take().and_then(Message::finish) {
            events.push(event);
        }
        Ok(())
    }

    /// Payload bytes of the current message, inflated first when it is compressed.
    fn message_bytes(&mut self, chunk: &[u8], last: bool) -> Result<(), WsError> {
        let Some(message) = &mut self.message else {
            return Ok(());
        };
        if !message.compressed {
            return message.push(chunk);
        }
        let inflater = self.inflater.as_mut().ok_or(WsError::Protocol)?;
        let mut out = [0u8; INFLATE_STEP];
        let mut input = chunk;
        loop {
            let (before_in, before_out) = (inflater.state.total_in(), inflater.state.total_out());
            let status = inflater
                .state
                .decompress(input, &mut out, FlushDecompress::Sync)
                .map_err(|_| WsError::Inflate)?;
            let read = (inflater.state.total_in() - before_in) as usize;
            let wrote = (inflater.state.total_out() - before_out) as usize;
            input = &input[read..];
            message.push(&out[..wrote])?;
            // Done once the input is used and the output did not fill; a final block ends the stream early.
            if status == Status::StreamEnd || input.is_empty() && wrote < out.len() {
                return Ok(());
            }
            if read == 0 && wrote == 0 {
                return if last { Ok(()) } else { Err(WsError::Inflate) };
            }
        }
    }
}

impl Message {
    fn push(&mut self, bytes: &[u8]) -> Result<(), WsError> {
        self.size += bytes.len();
        if self.size > MAX_MESSAGE {
            return Err(WsError::TooLarge);
        }
        if let Sink::Typing(first) = &mut self.sink {
            first.extend_from_slice(bytes);
            if first.len() >= TYPE_WINDOW {
                self.sink = typed(std::mem::take(first));
            }
            return Ok(());
        }
        match &mut self.sink {
            Sink::Small(_, kept) if kept.len() + bytes.len() > MAX_SMALL => self.sink = Sink::Skip,
            Sink::Small(_, kept) => kept.extend_from_slice(bytes),
            Sink::Usage(kept) => kept.push(bytes),
            Sink::Typing(_) | Sink::Skip => {}
        }
        Ok(())
    }

    /// What the whole message told, if it was one of the kept types.
    fn finish(self) -> Option<WsEvent> {
        let sink = match self.sink {
            Sink::Typing(first) => typed(first),
            sink => sink,
        };
        match sink {
            Sink::Small(SmallKind::Started, bytes) => {
                let event: Value = serde_json::from_slice(&bytes).ok()?;
                let id = event.pointer("/response/id")?.as_str()?;
                Some(WsEvent::Started(id.into()))
            }
            Sink::Small(SmallKind::Session, bytes) => {
                let event: Value = serde_json::from_slice(&bytes).ok()?;
                let model = event.pointer("/session/model")?.as_str()?;
                super::request_model::from_query(model).map(WsEvent::Model)
            }
            Sink::Usage(bytes) => bytes.ended(),
            Sink::Typing(_) | Sink::Skip => None,
        }
    }
}

/// The sink for a message whose first bytes are `first`.
fn typed(first: Vec<u8>) -> Sink {
    let kind = message_type(&first);
    let mut sink = match kind {
        Some("response.created") => Sink::Small(SmallKind::Started, Vec::new()),
        Some("session.created" | "session.updated") => Sink::Small(SmallKind::Session, Vec::new()),
        Some(
            "response.done" | "response.completed" | "response.failed" | "response.incomplete",
        ) => Sink::Usage(UsageBytes::Whole(Vec::new())),
        _ => return Sink::Skip,
    };
    match &mut sink {
        Sink::Small(_, kept) => *kept = first,
        Sink::Usage(kept) => kept.push(&first),
        _ => {}
    }
    sink
}

/// The value of the first `"type"` member in a message's first bytes.
fn message_type(first: &[u8]) -> Option<&str> {
    let window = &first[..first.len().min(TYPE_WINDOW)];
    let at = window.windows(7).position(|w| w == b"\"type\":")? + 7;
    let rest = window[at..].trim_ascii_start().strip_prefix(b"\"")?;
    let end = rest.iter().position(|&b| b == b'"')?;
    std::str::from_utf8(&rest[..end]).ok()
}

impl UsageBytes {
    fn push(&mut self, bytes: &[u8]) {
        match self {
            Self::Whole(kept) if kept.len() + bytes.len() <= MAX_WHOLE => {
                kept.extend_from_slice(bytes)
            }
            Self::Whole(kept) => {
                let mut head = std::mem::take(kept);
                let room = ENDS_KEEP.saturating_sub(head.len()).min(bytes.len());
                head.extend_from_slice(&bytes[..room]);
                let mut tail = head.split_off(ENDS_KEEP.min(head.len()));
                tail.extend_from_slice(&bytes[room..]);
                let cut = tail.len().saturating_sub(ENDS_KEEP);
                tail.drain(..cut);
                *self = Self::Ends { head, tail };
            }
            Self::Ends { tail, .. } => {
                tail.extend_from_slice(bytes);
                if tail.len() > 2 * ENDS_KEEP {
                    tail.drain(..tail.len() - ENDS_KEEP);
                }
            }
        }
    }

    /// The response's id and usage. A message too long to keep is read from its ends: `id`, `status` and
    /// `model` near the start of the response, `usage` its last object.
    fn ended(self) -> Option<WsEvent> {
        let response = match self {
            Self::Whole(bytes) => {
                let mut event: Value = serde_json::from_slice(&bytes).ok()?;
                event.get_mut("response")?.take()
            }
            Self::Ends { head, tail } => {
                let tail = &tail[tail.len().saturating_sub(ENDS_KEEP)..];
                let mut response = serde_json::Map::new();
                for key in ["id", "status", "model"] {
                    if let Some(value) = first_string(&head, key) {
                        response.insert(key.into(), Value::String(value));
                    }
                }
                response.insert("usage".into(), last_object(tail, "usage")?);
                Value::Object(response)
            }
        };
        let id = response.get("id").and_then(Value::as_str).map(Into::into);
        let usage = parse_response_json_value(&response)?;
        Some(WsEvent::Ended(id, Box::new(usage)))
    }
}

/// The first `"key":"value"` string in `bytes`, after the `"response"` member starts.
fn first_string(bytes: &[u8], key: &str) -> Option<String> {
    let from = find(bytes, b"\"response\"")?;
    let needle = format!("\"{key}\":");
    let at = from + find(&bytes[from..], needle.as_bytes())? + needle.len();
    let rest = bytes[at..].trim_ascii_start().strip_prefix(b"\"")?;
    let end = rest.iter().position(|&b| b == b'"')?;
    String::from_utf8(rest[..end].to_vec()).ok()
}

/// The JSON object of the last `"key":` member in `bytes`.
fn last_object(bytes: &[u8], key: &str) -> Option<Value> {
    let needle = format!("\"{key}\":");
    let at = bytes
        .windows(needle.len())
        .rposition(|w| w == needle.as_bytes())?
        + needle.len();
    let mut values =
        serde_json::Deserializer::from_slice(bytes[at..].trim_ascii_start()).into_iter::<Value>();
    values.next()?.ok().filter(Value::is_object)
}

fn find(bytes: &[u8], needle: &[u8]) -> Option<usize> {
    bytes.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests;

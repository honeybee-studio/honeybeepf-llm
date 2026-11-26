//! WebSocket calls (OpenAI Realtime, Responses over a WebSocket): the upgrade request to a listed provider,
//! the server's 101 answer, and the server's frames, read only for the messages that start a response or end
//! it with its usage. Client frames are never read, and every other message is skipped unkept.

use flate2::{Decompress, FlushDecompress, Status};
use honeybeepf_llm_core::types::UsageInfo;
use serde_json::Value;

use super::{MatchedBy, body::parse_response_json_value, matched_by, registry::PROVIDER_REGISTRY};

/// Longest 101 answer read; real ones are under 1 KB.
const MAX_HANDSHAKE: usize = 16 * 1024;
/// Message bytes read to find its `type`, which OpenAI sends first.
const TYPE_WINDOW: usize = 256;
/// A response-start message kept whole; a longer one is skipped.
const MAX_STARTED: usize = 64 * 1024;
/// A usage message kept whole; a longer one is counted as unread.
const MAX_USAGE: usize = 1024 * 1024;
/// Longest frame; a longer one ends the reading.
pub(crate) const MAX_FRAME: u64 = 16 * 1024 * 1024;
/// Longest message after inflating; a longer one ends the reading, so a deflate bomb costs at most this.
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
    /// The server starts each message's compression afresh.
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
    /// A response ended in a message too long to keep.
    Unread,
}

/// Why the server's frames cannot be read past this point; the connection is then left alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WsError {
    /// Not a frame a server may send: masked, reserved bits or opcodes, a control frame too long or split.
    Protocol,
    /// A frame past `MAX_FRAME`, or a message past `MAX_MESSAGE` after inflating.
    TooLarge,
    /// The compressed stream is broken.
    Inflate,
}

/// The server's half of a WebSocket, read frame by frame.
#[derive(Default)]
pub struct ServerFrames {
    /// Head bytes of the next frame, at most 10 since a server frame has no mask.
    head: [u8; 10],
    head_len: usize,
    /// Payload bytes left in the current frame, which is being read when `in_frame` is set.
    left: u64,
    in_frame: bool,
    control: bool,
    fin: bool,
    /// Where the bytes of the data message in progress go.
    message: Option<Sink>,
    /// The message in progress is compressed, and how many bytes it inflated to so far.
    compressed: bool,
    inflated: usize,
    /// Set when the server agreed to permessage-deflate: one window across messages, or reset after each.
    inflater: Option<Box<Inflater>>,
}

struct Inflater {
    state: Decompress,
    no_context_takeover: bool,
}

/// Where a message's bytes go once its type is known.
enum Sink {
    /// The first bytes, read for the type.
    Typing(Vec<u8>),
    Started(Vec<u8>),
    Usage(Vec<u8>),
    TooLong,
    Skip,
}

impl ServerFrames {
    pub fn new(deflate: Option<Deflate>) -> Self {
        Self {
            inflater: deflate.map(|d| {
                Box::new(Inflater {
                    state: Decompress::new(false),
                    no_context_takeover: d.no_context_takeover,
                })
            }),
            ..Self::default()
        }
    }

    /// Read the next server bytes, adding what their messages told to `events`.
    pub fn feed(&mut self, mut data: &[u8], events: &mut Vec<WsEvent>) -> Result<(), WsError> {
        while !data.is_empty() {
            if !self.in_frame {
                let used = self.read_head(data)?;
                data = &data[used..];
                if self.in_frame && self.left == 0 {
                    self.end_frame(events)?;
                }
                continue;
            }
            let n = usize::try_from(self.left).map_or(data.len(), |left| left.min(data.len()));
            let (chunk, rest) = data.split_at(n);
            data = rest;
            self.left -= n as u64;
            if !self.control {
                self.message_bytes(chunk)?;
            }
            if self.left == 0 {
                self.end_frame(events)?;
            }
        }
        Ok(())
    }

    /// Take head bytes from `data`, returning how many were used; a whole head starts the frame.
    fn read_head(&mut self, data: &[u8]) -> Result<usize, WsError> {
        for (used, &byte) in data.iter().enumerate() {
            self.head[self.head_len] = byte;
            self.head_len += 1;
            let want = match (self.head_len, self.head[1] & 0x7f) {
                (1, _) => continue,
                (_, 126) => 4,
                (_, 127) => 10,
                _ => 2,
            };
            if self.head_len == want {
                self.start_frame()?;
                return Ok(used + 1);
            }
        }
        Ok(data.len())
    }

    fn start_frame(&mut self) -> Result<(), WsError> {
        let [b0, b1] = [self.head[0], self.head[1]];
        let (fin, rsv1, opcode) = (b0 & 0x80 != 0, b0 & 0x40 != 0, b0 & 0x0f);
        // A server never masks; only permessage-deflate's reserved bit is read, on a message's first frame.
        if b1 & 0x80 != 0
            || b0 & 0x30 != 0
            || rsv1 && (self.inflater.is_none() || !matches!(opcode, 1 | 2))
        {
            return Err(WsError::Protocol);
        }
        let len = match b1 & 0x7f {
            126 => u64::from(u16::from_be_bytes([self.head[2], self.head[3]])),
            127 => u64::from_be_bytes(self.head[2..10].try_into().map_err(|_| WsError::Protocol)?),
            n => u64::from(n),
        };
        match opcode {
            0x8..=0xa if fin && len <= 125 => {}
            0x1 | 0x2 if self.message.is_none() => {
                self.message = Some(Sink::Typing(Vec::new()));
                (self.compressed, self.inflated) = (rsv1, 0);
            }
            0x0 if self.message.is_some() => {}
            _ => return Err(WsError::Protocol),
        }
        if len > MAX_FRAME {
            return Err(WsError::TooLarge);
        }
        (self.head_len, self.left, self.in_frame) = (0, len, true);
        (self.control, self.fin) = (opcode & 0x08 != 0, fin);
        Ok(())
    }

    fn end_frame(&mut self, events: &mut Vec<WsEvent>) -> Result<(), WsError> {
        self.in_frame = false;
        if self.control || !self.fin {
            return Ok(());
        }
        if self.compressed {
            self.message_bytes(&DEFLATE_TAIL)?;
            if let Some(inflater) = self.inflater.as_mut().filter(|i| i.no_context_takeover) {
                inflater.state.reset(false);
            }
        }
        if let Some(event) = self.message.take().and_then(Sink::finish) {
            events.push(event);
        }
        Ok(())
    }

    /// Payload bytes of the current message, inflated first when it is compressed.
    fn message_bytes(&mut self, chunk: &[u8]) -> Result<(), WsError> {
        let Some(sink) = &mut self.message else {
            return Ok(());
        };
        if !self.compressed {
            sink.push(chunk);
            return Ok(());
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
            self.inflated += wrote;
            if self.inflated > MAX_MESSAGE {
                return Err(WsError::TooLarge);
            }
            sink.push(&out[..wrote]);
            // Done once the input is used and the output did not fill.
            if status == Status::StreamEnd || input.is_empty() && wrote < out.len() {
                return Ok(());
            }
            if read == 0 && wrote == 0 {
                return Err(WsError::Inflate);
            }
        }
    }
}

impl Sink {
    fn push(&mut self, bytes: &[u8]) {
        match self {
            Self::Typing(first) => {
                first.extend_from_slice(bytes);
                if first.len() >= TYPE_WINDOW {
                    *self = typed(std::mem::take(first));
                }
            }
            Self::Started(kept) if kept.len() + bytes.len() > MAX_STARTED => *self = Self::Skip,
            Self::Usage(kept) if kept.len() + bytes.len() > MAX_USAGE => *self = Self::TooLong,
            Self::Started(kept) | Self::Usage(kept) => kept.extend_from_slice(bytes),
            Self::TooLong | Self::Skip => {}
        }
    }

    /// What the whole message told, if it was one of the kept types.
    fn finish(self) -> Option<WsEvent> {
        match self {
            Self::Typing(first) => typed(first).finish_typed(),
            sink => sink.finish_typed(),
        }
    }

    fn finish_typed(self) -> Option<WsEvent> {
        match self {
            Self::Started(bytes) => {
                let event: Value = serde_json::from_slice(&bytes).ok()?;
                Some(WsEvent::Started(
                    event.pointer("/response/id")?.as_str()?.into(),
                ))
            }
            Self::Usage(bytes) => {
                let mut event: Value = serde_json::from_slice(&bytes).ok()?;
                let response = event.get_mut("response")?.take();
                let id = response.get("id").and_then(Value::as_str).map(Into::into);
                let usage = parse_response_json_value(&response)?;
                Some(WsEvent::Ended(id, Box::new(usage)))
            }
            Self::TooLong => Some(WsEvent::Unread),
            Self::Typing(_) | Self::Skip => None,
        }
    }
}

/// The sink for a message whose first bytes are `first`.
fn typed(first: Vec<u8>) -> Sink {
    match message_type(&first) {
        Some("response.created") => Sink::Started(first),
        Some(
            "response.done" | "response.completed" | "response.failed" | "response.incomplete",
        ) => Sink::Usage(first),
        _ => Sink::Skip,
    }
}

/// The value of the first `"type"` member in a message's first bytes.
fn message_type(first: &[u8]) -> Option<&str> {
    let window = &first[..first.len().min(TYPE_WINDOW)];
    let at = window.windows(7).position(|w| w == b"\"type\":")? + 7;
    let rest = window[at..].trim_ascii_start().strip_prefix(b"\"")?;
    let end = rest.iter().position(|&b| b == b'"')?;
    std::str::from_utf8(&rest[..end]).ok()
}

#[cfg(test)]
mod tests;

//! HTTP/2 frames (RFC 9113 §4–6) read from one direction's bytes, split at any point.

/// The client connection preface (RFC 9113 §3.4).
pub const PREFACE: &[u8; 24] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
/// SETTINGS_MAX_FRAME_SIZE until the receiver's SETTINGS say otherwise.
pub const DEFAULT_MAX_FRAME_SIZE: u32 = 16_384;
/// Largest header block kept; a larger one is reported as `HeadersTooLarge`.
pub const MAX_HEADER_BLOCK: usize = 64 * 1024;

/// A frame's type (RFC 9113 §6); an open set, since an unknown type is skipped, not rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FrameType(u8);

impl FrameType {
    const DATA: Self = Self(0);
    const HEADERS: Self = Self(1);
    const PRIORITY: Self = Self(2);
    const RST_STREAM: Self = Self(3);
    const SETTINGS: Self = Self(4);
    const PUSH_PROMISE: Self = Self(5);
    const PING: Self = Self(6);
    const GOAWAY: Self = Self(7);
    const WINDOW_UPDATE: Self = Self(8);
    const CONTINUATION: Self = Self(9);
}

/// A frame's flags byte; `has` tests one flag bit.
#[derive(Clone, Copy, Debug)]
struct Flags(u8);

impl Flags {
    const END_STREAM: u8 = 0x1;
    const END_HEADERS: u8 = 0x4;
    const PADDED: u8 = 0x8;
    const PRIORITY: u8 = 0x20;

    fn has(self, flag: u8) -> bool {
        self.0 & flag != 0
    }
}

/// An event the reader emits, not one per frame: a DATA frame yields several, most frame types none.
#[non_exhaustive]
#[derive(Debug, PartialEq, Eq)]
pub enum Frame<'a> {
    /// A whole header block (HEADERS and any CONTINUATION), padding and priority removed.
    Headers { stream: u32, block: &'a [u8] },
    /// A header block over `MAX_HEADER_BLOCK`; the HPACK table updates in it are lost.
    HeadersTooLarge { stream: u32 },
    /// DATA payload bytes as they arrive, padding removed.
    Data { stream: u32, data: &'a [u8] },
    /// END_STREAM, after the DATA frame or header block that carried it.
    StreamEnd { stream: u32 },
    /// RST_STREAM: the peer ended this stream early (its error code is not read).
    Reset { stream: u32 },
    /// GOAWAY: no stream above `last_stream` will be processed (error and debug data not read).
    Goaway { last_stream: u32 },
}

/// Why a reader stopped; past it, frame boundaries in this direction are unknown.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Broken {
    Preface,
    /// A frame longer than the max frame size.
    TooLarge,
    /// DATA, HEADERS or CONTINUATION on stream 0.
    NoStream,
    /// Padding or priority fields longer than the payload.
    BadPadding,
    /// A CONTINUATION without its HEADERS, or another frame inside a header block.
    Continuation,
    /// Server push is not followed, and its header block would change the HPACK table unseen.
    PushPromise,
}

#[derive(Clone, Copy, Debug)]
enum State {
    /// Preface bytes matched so far.
    Preface(usize),
    Head,
    Payload,
    /// Re-anchoring after a gap: buffering bytes until a run of valid frame heads is found.
    Scan,
    Broken(Broken),
}

/// Whether a scan offset begins a run of three chaining valid heads.
enum Candidate {
    Anchored,
    Rejected,
    NeedMore,
}

/// A frame head read at a scan offset: valid with its payload length, malformed, or short of 9 bytes.
enum HeadScan {
    Ok(usize),
    Bad,
    Short,
}

/// A header block waiting for its END_HEADERS.
#[derive(Clone, Copy, Debug)]
struct Block {
    stream: u32,
    end_stream: bool,
    too_large: bool,
}

/// Frames of one direction of one connection.
#[derive(Clone, Debug)]
pub struct FrameReader {
    state: State,
    max_frame_size: u32,
    head: [u8; 9],
    head_len: usize,
    kind: FrameType,
    flags: Flags,
    stream: u32,
    /// Payload bytes of the current frame still due.
    left: usize,
    /// Pad length and priority bytes still due at the payload's start.
    front: usize,
    /// The pad length byte is still due.
    pad_due: bool,
    /// Padding at the payload's end, known once the pad length byte is read.
    pad: usize,
    block: Option<Block>,
    /// The header block in progress.
    buf: Vec<u8>,
    /// First payload bytes of a GOAWAY (its last-stream-id), gathered across reads.
    ctrl: [u8; 4],
    ctrl_len: usize,
    /// Bytes buffered while re-anchoring after a gap; empty except during a resync.
    scan: Vec<u8>,
    /// First `scan` offset not yet ruled out as the start of a frame-head run.
    scan_from: usize,
}

impl FrameReader {
    /// The client's direction, which opens with the preface.
    pub fn client() -> Self {
        Self {
            state: State::Preface(0),
            ..Self::server()
        }
    }

    /// The server's direction, which opens with a frame.
    pub fn server() -> Self {
        Self {
            state: State::Head,
            max_frame_size: DEFAULT_MAX_FRAME_SIZE,
            head: [0; 9],
            head_len: 0,
            kind: FrameType(0),
            flags: Flags(0),
            stream: 0,
            left: 0,
            front: 0,
            pad_due: false,
            pad: 0,
            block: None,
            buf: Vec::new(),
            ctrl: [0; 4],
            ctrl_len: 0,
            scan: Vec::new(),
            scan_from: 0,
        }
    }

    /// Takes the SETTINGS_MAX_FRAME_SIZE the receiver of this direction sent.
    pub fn set_max_frame_size(&mut self, size: u32) {
        self.max_frame_size = size.clamp(DEFAULT_MAX_FRAME_SIZE, (1 << 24) - 1);
    }

    pub fn broken(&self) -> Option<Broken> {
        match self.state {
            State::Broken(why) => Some(why),
            _ => None,
        }
    }

    /// Recover after a gap in this direction: drop any frame in progress and re-anchor at the next run of
    /// three chaining valid heads, so a lost HEADERS cannot silently desync the rest of the stream for good.
    pub fn resync(&mut self) {
        self.block = None;
        self.buf.clear();
        self.scan.clear();
        self.scan_from = 0;
        self.state = State::Scan;
    }

    /// Skip `n` lost bytes known to fall inside the current DATA frame's payload, keeping framing aligned
    /// (its body just has a hole). Returns false — so the caller resyncs — when the gap is not safely inside
    /// a DATA payload (a header block, or an unknown frame boundary).
    pub fn skip_gap(&mut self, n: usize) -> bool {
        if matches!(self.state, State::Payload)
            && self.kind == FrameType::DATA
            && self.front == 0
            && n <= self.left.saturating_sub(self.pad)
        {
            self.left -= n;
            true
        } else {
            false
        }
    }

    /// Reads all of `input`, passing what it completes to `on` and keeping a cut frame for the next call.
    pub fn read(&mut self, input: &[u8], mut on: impl FnMut(Frame<'_>)) {
        // Once a resync re-anchors, `input` is repointed at the bytes kept past the anchor so they feed the
        // normal state machine in this same call; `tail` owns them and outlives that borrow.
        let mut tail: Vec<u8>;
        let mut input: &[u8] = input;
        while !input.is_empty() {
            match self.state {
                State::Broken(_) => return,
                State::Scan => {
                    self.scan_push(input);
                    let Some(at) = self.anchor() else { return };
                    self.scan.drain(..at);
                    self.state = State::Head;
                    tail = std::mem::take(&mut self.scan);
                    input = &tail;
                }
                State::Preface(seen) => {
                    let n = (PREFACE.len() - seen).min(input.len());
                    if input[..n] != PREFACE[seen..seen + n] {
                        self.state = State::Broken(Broken::Preface);
                        return;
                    }
                    input = &input[n..];
                    self.state = if seen + n == PREFACE.len() {
                        State::Head
                    } else {
                        State::Preface(seen + n)
                    };
                }
                State::Head => {
                    let n = (9 - self.head_len).min(input.len());
                    self.head[self.head_len..self.head_len + n].copy_from_slice(&input[..n]);
                    self.head_len += n;
                    input = &input[n..];
                    if self.head_len == 9 {
                        self.head_len = 0;
                        self.start_frame(&mut on);
                    }
                }
                State::Payload => {
                    let used = self.payload(input, &mut on);
                    input = &input[used..];
                    if self.left == 0 && matches!(self.state, State::Payload) {
                        self.finish_frame(&mut on);
                    }
                }
            }
        }
    }

    fn start_frame(&mut self, on: &mut impl FnMut(Frame<'_>)) {
        let h = self.head;
        let len = (usize::from(h[0]) << 16) | (usize::from(h[1]) << 8) | usize::from(h[2]);
        let (kind, flags) = (FrameType(h[3]), Flags(h[4]));
        let stream = u32::from_be_bytes([h[5], h[6], h[7], h[8]]) & 0x7fff_ffff;
        let padded =
            matches!(kind, FrameType::DATA | FrameType::HEADERS) && flags.has(Flags::PADDED);
        let priority = kind == FrameType::HEADERS && flags.has(Flags::PRIORITY);
        let front = usize::from(padded) + if priority { 5 } else { 0 };
        if let Err(why) = self.check_head(len, kind, stream, front) {
            self.state = State::Broken(why);
            return;
        }
        (self.kind, self.flags, self.stream) = (kind, flags, stream);
        (self.left, self.front, self.pad_due, self.pad) = (len, front, padded, 0);
        if kind == FrameType::HEADERS {
            self.buf.clear();
            self.block = Some(Block {
                stream,
                end_stream: flags.has(Flags::END_STREAM),
                too_large: false,
            });
        }
        if kind == FrameType::GOAWAY {
            (self.ctrl, self.ctrl_len) = ([0; 4], 0);
        }
        self.state = State::Payload;
        if len == 0 {
            self.finish_frame(on);
        }
    }

    /// Head validity in rule order; the first rule a frame breaks names the break.
    fn check_head(
        &self,
        len: usize,
        kind: FrameType,
        stream: u32,
        front: usize,
    ) -> Result<(), Broken> {
        let in_block = self
            .block
            .is_some_and(|b| kind == FrameType::CONTINUATION && stream == b.stream);
        if len > self.max_frame_size as usize {
            return Err(Broken::TooLarge);
        }
        if self.block.is_some() != in_block || (kind == FrameType::CONTINUATION && !in_block) {
            return Err(Broken::Continuation);
        }
        if matches!(
            kind,
            FrameType::DATA | FrameType::HEADERS | FrameType::CONTINUATION
        ) && stream == 0
        {
            return Err(Broken::NoStream);
        }
        if kind == FrameType::PUSH_PROMISE {
            return Err(Broken::PushPromise);
        }
        if front > len {
            return Err(Broken::BadPadding);
        }
        Ok(())
    }

    /// Reads payload bytes from `input`; returns how many it used.
    fn payload(&mut self, input: &[u8], on: &mut impl FnMut(Frame<'_>)) -> usize {
        let mut used = 0;
        while self.front > 0 && used < input.len() {
            if self.pad_due {
                self.pad = usize::from(input[used]);
                self.pad_due = false;
            }
            (self.front, self.left, used) = (self.front - 1, self.left - 1, used + 1);
            if self.front == 0 && self.pad > self.left {
                self.state = State::Broken(Broken::BadPadding);
                return used;
            }
        }
        // Padding is the payload's tail, so once `left` is within it no content is due.
        let content = if self.front > 0 {
            0
        } else {
            self.left.saturating_sub(self.pad)
        };
        let n = content.min(input.len() - used);
        let bytes = &input[used..used + n];
        match (self.kind, &mut self.block) {
            (FrameType::DATA, _) if n > 0 => on(Frame::Data {
                stream: self.stream,
                data: bytes,
            }),
            (FrameType::HEADERS | FrameType::CONTINUATION, Some(b)) if !b.too_large => {
                b.too_large = self.buf.len() + n > MAX_HEADER_BLOCK;
                if !b.too_large {
                    self.buf.extend_from_slice(bytes);
                }
            }
            (FrameType::GOAWAY, _) if n > 0 && self.ctrl_len < 4 => {
                let take = n.min(4 - self.ctrl_len);
                self.ctrl[self.ctrl_len..self.ctrl_len + take].copy_from_slice(&bytes[..take]);
                self.ctrl_len += take;
            }
            _ => {}
        }
        used += n;
        self.left -= n;
        if n == content {
            let pad = self.left.min(input.len() - used);
            self.left -= pad;
            used += pad;
        }
        used
    }

    fn finish_frame(&mut self, on: &mut impl FnMut(Frame<'_>)) {
        self.state = State::Head;
        if self.kind == FrameType::DATA && self.flags.has(Flags::END_STREAM) {
            on(Frame::StreamEnd {
                stream: self.stream,
            });
        }
        if self.kind == FrameType::RST_STREAM && self.stream != 0 {
            on(Frame::Reset {
                stream: self.stream,
            });
        }
        // A GOAWAY shorter than its last-stream-id is malformed; dropping it avoids acting on a garbage id.
        if self.kind == FrameType::GOAWAY && self.ctrl_len == 4 {
            on(Frame::Goaway {
                last_stream: u32::from_be_bytes(self.ctrl) & 0x7fff_ffff,
            });
        }
        if !matches!(self.kind, FrameType::HEADERS | FrameType::CONTINUATION)
            || !self.flags.has(Flags::END_HEADERS)
        {
            return;
        }
        if let Some(b) = self.block.take() {
            if b.too_large {
                on(Frame::HeadersTooLarge { stream: b.stream });
            } else {
                on(Frame::Headers {
                    stream: b.stream,
                    block: &self.buf,
                });
            }
            if b.end_stream {
                on(Frame::StreamEnd { stream: b.stream });
            }
        }
    }

    /// Append scan bytes, keeping only enough tail to hold three frames so the buffer stays bounded.
    fn scan_push(&mut self, input: &[u8]) {
        let cap = (2 * self.max_frame_size as usize + 27).min(256 * 1024);
        self.scan.extend_from_slice(input);
        if self.scan.len() > cap {
            let drop = self.scan.len() - cap;
            self.scan.drain(..drop);
            self.scan_from = self.scan_from.saturating_sub(drop);
        }
    }

    /// The offset of the first run of three chaining valid heads in `scan`, advancing `scan_from` past
    /// offsets whose first head can never be valid.
    fn anchor(&mut self) -> Option<usize> {
        let mut at = self.scan_from;
        while at + 9 <= self.scan.len() {
            match self.chain_at(at) {
                Candidate::Anchored => return Some(at),
                Candidate::NeedMore => at += 1,
                Candidate::Rejected => {
                    if at == self.scan_from {
                        self.scan_from += 1;
                    }
                    at += 1;
                }
            }
        }
        None
    }

    /// Whether three chaining valid heads start at `off`, or more bytes are needed to tell.
    fn chain_at(&self, off: usize) -> Candidate {
        let mut at = off;
        for _ in 0..3 {
            match self.head_scan(at) {
                HeadScan::Bad => return Candidate::Rejected,
                HeadScan::Short => return Candidate::NeedMore,
                HeadScan::Ok(len) => at += 9 + len,
            }
        }
        Candidate::Anchored
    }

    /// A frame head at `off`: its payload length if it is self-consistent, or whether bytes run short.
    fn head_scan(&self, off: usize) -> HeadScan {
        let Some(h) = self.scan.get(off..off + 9) else {
            return HeadScan::Short;
        };
        let len = (usize::from(h[0]) << 16) | (usize::from(h[1]) << 8) | usize::from(h[2]);
        let stream = u32::from_be_bytes([h[5], h[6], h[7], h[8]]) & 0x7fff_ffff;
        if len <= self.max_frame_size as usize
            && plausible_head(FrameType(h[3]), Flags(h[4]), stream)
        {
            HeadScan::Ok(len)
        } else {
            HeadScan::Bad
        }
    }
}

/// Whether `bytes` begin an HTTP/2 frame stream: a run of three chaining valid frame heads, the same test
/// a resync uses. Used to classify a connection joined after its preface; HTTP/1.1 text never forms such a
/// run (its ASCII start reads as a frame far larger than the max).
pub fn looks_like_http2(bytes: &[u8]) -> bool {
    let mut reader = FrameReader::server();
    reader.resync();
    reader.scan_push(bytes);
    reader.anchor().is_some()
}

/// Whether a frame head's type, flags and stream id are self-consistent (RFC 9113 §6), so a chance byte
/// pattern is unlikely to pass as a frame when re-anchoring. We only see client-initiated streams (odd ids)
/// and connection frames (stream 0); server push is skipped, so its head is not a valid anchor either.
fn plausible_head(kind: FrameType, flags: Flags, stream: u32) -> bool {
    // (flag bits defined for the type, whether the stream id must be the connection's 0).
    let (mask, on_connection): (u8, bool) = match kind {
        FrameType::DATA => (Flags::END_STREAM | Flags::PADDED, false),
        FrameType::HEADERS => (
            Flags::END_STREAM | Flags::END_HEADERS | Flags::PADDED | Flags::PRIORITY,
            false,
        ),
        FrameType::PRIORITY => (0, false),
        FrameType::RST_STREAM => (0, false),
        FrameType::SETTINGS => (0x1, true), // ACK
        FrameType::PING => (0x1, true),     // ACK
        FrameType::GOAWAY => (0, true),
        // WINDOW_UPDATE is the one type valid on either a stream or the connection.
        FrameType::WINDOW_UPDATE => return flags.0 == 0 && (stream == 0 || stream & 1 == 1),
        FrameType::CONTINUATION => (Flags::END_HEADERS, false),
        _ => return false,
    };
    let stream_ok = if on_connection {
        stream == 0
    } else {
        stream != 0 && stream & 1 == 1
    };
    flags.0 & !mask == 0 && stream_ok
}

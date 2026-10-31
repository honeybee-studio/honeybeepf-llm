//! HTTP/2 frames (RFC 9113 §4–6) read from one direction's bytes, split at any point.

/// The client connection preface (RFC 9113 §3.4).
pub const PREFACE: &[u8; 24] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
/// SETTINGS_MAX_FRAME_SIZE until the receiver's SETTINGS say otherwise.
pub const DEFAULT_MAX_FRAME_SIZE: u32 = 16_384;
/// Largest header block kept; a larger one is reported as `HeadersTooLarge`.
pub const MAX_HEADER_BLOCK: usize = 64 * 1024;

const DATA: u8 = 0;
const HEADERS: u8 = 1;
const PUSH_PROMISE: u8 = 5;
const CONTINUATION: u8 = 9;

const END_STREAM: u8 = 0x1;
const END_HEADERS: u8 = 0x4;
const PADDED: u8 = 0x8;
const PRIORITY: u8 = 0x20;

/// What a frame, or a piece of a DATA frame, carries; other frame types are skipped.
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
    Broken(Broken),
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
    kind: u8,
    flags: u8,
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
            kind: 0,
            flags: 0,
            stream: 0,
            left: 0,
            front: 0,
            pad_due: false,
            pad: 0,
            block: None,
            buf: Vec::new(),
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

    /// Reads all of `input`, passing what it completes to `on` and keeping a cut frame for the next call.
    pub fn read(&mut self, mut input: &[u8], mut on: impl FnMut(Frame<'_>)) {
        while !input.is_empty() {
            match self.state {
                State::Broken(_) => return,
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
        let (kind, flags) = (h[3], h[4]);
        let stream = u32::from_be_bytes([h[5], h[6], h[7], h[8]]) & 0x7fff_ffff;
        let padded = matches!(kind, DATA | HEADERS) && flags & PADDED != 0;
        let priority = kind == HEADERS && flags & PRIORITY != 0;
        let front = usize::from(padded) + if priority { 5 } else { 0 };
        let in_block = self
            .block
            .is_some_and(|b| kind == CONTINUATION && stream == b.stream);
        let broken = if len > self.max_frame_size as usize {
            Some(Broken::TooLarge)
        } else if self.block.is_some() != in_block || (kind == CONTINUATION && !in_block) {
            Some(Broken::Continuation)
        } else if matches!(kind, DATA | HEADERS | CONTINUATION) && stream == 0 {
            Some(Broken::NoStream)
        } else if kind == PUSH_PROMISE {
            Some(Broken::PushPromise)
        } else if front > len {
            Some(Broken::BadPadding)
        } else {
            None
        };
        if let Some(why) = broken {
            self.state = State::Broken(why);
            return;
        }
        (self.kind, self.flags, self.stream) = (kind, flags, stream);
        (self.left, self.front, self.pad_due, self.pad) = (len, front, padded, 0);
        if kind == HEADERS {
            self.buf.clear();
            self.block = Some(Block {
                stream,
                end_stream: flags & END_STREAM != 0,
                too_large: false,
            });
        }
        self.state = State::Payload;
        if len == 0 {
            self.finish_frame(on);
        }
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
            (DATA, _) if n > 0 => on(Frame::Data {
                stream: self.stream,
                data: bytes,
            }),
            (HEADERS | CONTINUATION, Some(b)) if !b.too_large => {
                b.too_large = self.buf.len() + n > MAX_HEADER_BLOCK;
                if !b.too_large {
                    self.buf.extend_from_slice(bytes);
                }
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
        if self.kind == DATA && self.flags & END_STREAM != 0 {
            on(Frame::StreamEnd {
                stream: self.stream,
            });
        }
        if !matches!(self.kind, HEADERS | CONTINUATION) || self.flags & END_HEADERS == 0 {
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
}

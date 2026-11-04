//! Per-connection classes in the kernel's SSL_CLASS map, which drop non-LLM calls before the ringbuf.

/// What the kernel does with a connection's calls; a connection without an entry is `Unknown` and sends every call.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SslClass {
    Unknown = 0,
    /// Last request is a POST, or a GET of a Responses object whose usage may arrive only there.
    Send = 1,
    /// Last request is another method: drop until the next request.
    SkipRequest = 2,
    /// Server side, or a Host no provider serves: drop for good.
    SkipConnection = 3,
    /// HTTP/2 preface seen: send for good.
    H2 = 4,
}

/// An SSL_CLASS value: the class in the low byte, the entry's creation time (ns) above it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClassEntry(pub u64);

impl ClassEntry {
    const LOW_BYTE: u64 = 0xff;
    /// Added to Send by the first Read, so a later non-POST head starts a new request rather than being body bytes.
    const READ_SEEN: u64 = 0x80;

    /// An `Unknown` entry created at `born_ns`.
    #[inline(always)]
    pub fn new(born_ns: u64) -> Self {
        Self(born_ns & !Self::LOW_BYTE)
    }

    /// The entry's class, Send whether or not its response was read.
    pub fn class(self) -> SslClass {
        match self.0 & Self::LOW_BYTE {
            1 | 0x81 => SslClass::Send,
            2 => SslClass::SkipRequest,
            3 => SslClass::SkipConnection,
            4 => SslClass::H2,
            _ => SslClass::Unknown,
        }
    }

    #[inline(always)]
    fn born(self) -> u64 {
        self.0 & !Self::LOW_BYTE
    }

    /// Whether the low byte is exactly `class`, so a Send already read is not `Send` here.
    // A raw byte compare: matching on class() compiles to more eBPF instructions.
    #[inline(always)]
    fn is(self, class: SslClass) -> bool {
        self.0 & Self::LOW_BYTE == class as u64
    }

    #[inline(always)]
    fn with(self, class: SslClass) -> Self {
        Self(self.born() | class as u64)
    }

    /// SkipConnection entry replacing this one after an event at `event_ns`; None when the entry is newer than it.
    pub fn skip_connection(self, event_ns: u64) -> Option<Self> {
        // Userspace lags the kernel: a newer entry is a later connection that reuses the SSL* address.
        (self.born() <= event_ns).then_some(self.with(SslClass::SkipConnection))
    }
}

/// First bytes of a GET Write that `requests_response` reads, as little-endian words: enough for
/// `GET /openai/v1/responses/`.
pub type GetHead = [u64; 4];

/// Whether a GET head asks for a stored Responses object, which carries a background call's usage.
#[inline(always)]
pub fn requests_response(head: &GetHead) -> bool {
    // Where `/responses/` starts after `GET `, `/v1`, `/openai`, `/openai/v1`; the prefix bytes are not
    // compared, so another prefix of the same length only sends a GET userspace ignores.
    responses_at(head, 4)
        || responses_at(head, 7)
        || responses_at(head, 11)
        || responses_at(head, 14)
}

/// Whether `/responses/` starts at byte `at`, read by shifting aligned words: eBPF rejects unaligned stack loads.
#[inline(always)]
fn responses_at(head: &GetHead, at: usize) -> bool {
    const P: &[u8; 11] = b"/responses/";
    const FIRST: u64 = u64::from_le_bytes([P[0], P[1], P[2], P[3], P[4], P[5], P[6], P[7]]);
    const REST: u64 = u32::from_le_bytes([P[8], P[9], P[10], 0]) as u64;
    word_at(head, at) == FIRST && word_at(head, at + 8) & 0xff_ffff == REST
}

/// The 8 bytes from byte `at`; `at` is a constant of at most 22, so the indexes fold away.
#[inline(always)]
fn word_at(head: &GetHead, at: usize) -> u64 {
    let (i, shift) = (at / 8, (at % 8) * 8);
    if shift == 0 {
        head[i]
    } else {
        head[i] >> shift | head[i + 1] << (64 - shift)
    }
}

/// Entry after a `len`-byte Write whose first 8 bytes are `head` (zeros when shorter).
// `gets_response` is a closure so the probe reads past 8 bytes only for a GET; `Fn`, as a match guard can't move.
#[inline(always)]
pub fn after_write(
    entry: ClassEntry,
    head: [u8; 8],
    len: u32,
    gets_response: impl Fn() -> bool,
) -> ClassEntry {
    // Word compares: eBPF has no memcmp.
    const fn word(b: &[u8; 4]) -> u32 {
        u32::from_le_bytes(*b)
    }
    const POST: u32 = word(b"POST");
    const PRI: u32 = word(b"PRI ");
    const PREFACE_TAIL: u32 = word(b"* HT");
    const HTTP: u32 = word(b"HTTP");
    const GET: u32 = word(b"GET ");
    const PUT: u32 = word(b"PUT ");
    const HEAD: u32 = word(b"HEAD");
    const DELETE: u32 = word(b"DELE");
    const PATCH: u32 = word(b"PATC");
    const OPTIONS: u32 = word(b"OPTI");
    const CONNECT: u32 = word(b"CONN");

    if entry.is(SslClass::SkipConnection) || entry.is(SslClass::H2) {
        return entry;
    }
    let [a, b, c, d, e, f, g, h] = head;
    // A request head split before its method, or after a stray CRLF: send until the next head.
    if len < 8 || (a == b'\r' && b == b'\n') {
        return if entry.is(SslClass::SkipRequest) {
            entry.with(SslClass::Unknown)
        } else {
            entry
        };
    }
    match word(&[a, b, c, d]) {
        POST if e == b' ' => entry.with(SslClass::Send),
        PRI if word(&[e, f, g, h]) == PREFACE_TAIL => entry.with(SslClass::H2),
        // Only as the first Write: later it could be body bytes of a client request.
        HTTP if entry.is(SslClass::Unknown) && e == b'/' && f == b'1' && g == b'.' => {
            entry.with(SslClass::SkipConnection)
        }
        GET if gets_response() => entry.with(SslClass::Send),
        GET | PUT | HEAD | DELETE | PATCH | OPTIONS | CONNECT if !entry.is(SslClass::Send) => {
            entry.with(SslClass::SkipRequest)
        }
        _ => entry,
    }
}

/// Entry after a Read: the first response byte of a sent request marks it read.
#[inline(always)]
pub fn after_read(entry: ClassEntry) -> ClassEntry {
    if entry.is(SslClass::Send) {
        ClassEntry(entry.0 | ClassEntry::READ_SEEN)
    } else {
        entry
    }
}

/// Whether a call on a connection with this entry is dropped in the kernel.
#[inline(always)]
pub fn drops(entry: ClassEntry) -> bool {
    entry.is(SslClass::SkipRequest) || entry.is(SslClass::SkipConnection)
}

#[cfg(test)]
mod tests {
    use super::SslClass::{H2, Send, SkipConnection, SkipRequest, Unknown};
    use super::*;

    fn head(s: &[u8]) -> [u8; 8] {
        let mut h = [0u8; 8];
        let n = s.len().min(8);
        h[..n].copy_from_slice(&s[..n]);
        h
    }

    /// What the probe reads for a GET: nothing from a Write shorter than the words.
    fn get_head(s: &[u8]) -> Option<GetHead> {
        let bytes = s.get(..size_of::<GetHead>())?;
        Some(core::array::from_fn(|i| {
            u64::from_le_bytes(core::array::from_fn(|j| bytes[i * 8 + j]))
        }))
    }

    fn entry(class: SslClass) -> ClassEntry {
        ClassEntry::new(0).with(class)
    }

    fn after(e: ClassEntry, s: &[u8]) -> ClassEntry {
        after_write(e, head(s), s.len() as u32, || {
            get_head(s).is_some_and(|h| requests_response(&h))
        })
    }

    fn class_after(class: SslClass, s: &[u8]) -> SslClass {
        after(entry(class), s).class()
    }

    #[test]
    fn requests_are_classed_by_method() {
        assert_eq!(class_after(Unknown, b"POST /v1/chat"), Send);
        for req in [
            &b"GET /healthz"[..],
            b"PUT /x H",
            b"HEAD / H",
            b"DELETE /",
            b"PATCH /x",
            b"OPTIONS ",
            b"CONNECT ",
        ] {
            assert_eq!(class_after(Unknown, req), SkipRequest, "{req:?}");
            assert_eq!(class_after(SkipRequest, req), SkipRequest, "{req:?}");
        }
        assert_eq!(class_after(SkipRequest, b"POST /v1/messages"), Send);
        assert_eq!(class_after(Unknown, b"POSTX /"), Unknown);
    }

    #[test]
    fn responses_retrieval_get_is_sent() {
        for req in [
            &b"GET /v1/responses/resp_bg1 HTTP/1.1\r\nHost: api.openai.com\r\n\r\n"[..],
            b"GET /v1/responses/resp_bg1?stream=true&starting_after=2 HTTP/1.1\r\n\r\n",
            b"GET /openai/responses/resp_1?api-version=2025-04-01-preview HTTP/1.1\r\n",
            b"GET /openai/v1/responses/resp_1 HTTP/1.1\r\nHost: x.openai.azure.com\r\n",
            b"GET /responses/resp_1 HTTP/1.1\r\nHost: litellm:4000\r\n\r\n",
        ] {
            assert_eq!(class_after(Unknown, req), Send, "{req:?}");
            assert_eq!(class_after(SkipRequest, req), Send, "{req:?}");
            // A poll after the POST's response was read.
            assert_eq!(after(after_read(entry(Send)), req), entry(Send), "{req:?}");
        }
        // The prefix is matched by length only.
        let other = b"GET /v2/responses/resp_1 HTTP/1.1\r\nHost: api.openai.com\r\n";
        assert_eq!(class_after(Unknown, other), Send);
    }

    #[test]
    fn other_gets_on_provider_paths_stay_skipped() {
        for req in [
            &b"GET /v1/files/file-1/content HTTP/1.1\r\nHost: api.openai.com\r\n"[..],
            b"GET /v1/models HTTP/1.1\r\nHost: api.openai.com\r\n\r\n",
            b"GET /v1/responses HTTP/1.1\r\nHost: api.openai.com\r\n\r\n",
            b"GET /v1/responsesX/resp_1 HTTP/1.1\r\nHost: api.openai.com\r\n",
            b"GET /v1/chat/completions/chatcmpl-1 HTTP/1.1\r\nHost: a\r\n",
            b"GET /api/openai/v1/responses/resp_1 HTTP/1.1\r\nHost: a\r\n",
        ] {
            assert_eq!(class_after(Unknown, req), SkipRequest, "{req:?}");
        }
        // A Write shorter than the 32 bytes the probe reads is not inspected.
        assert_eq!(
            class_after(Unknown, b"GET /openai/v1/responses/resp_1"),
            SkipRequest
        );
        assert_eq!(
            class_after(Unknown, b"GET /openai/v1/responses/resp_1 "),
            Send
        );
        // Only GET: other methods on the same path read or delete stored objects.
        let delete = b"DELETE /v1/responses/resp_1 HTTP/1.1\r\nHost: api.openai.com\r\n";
        assert_eq!(class_after(Unknown, delete), SkipRequest);
    }

    #[test]
    fn server_response_skips_only_as_first_write() {
        assert_eq!(class_after(Unknown, b"HTTP/1.1 200 OK"), SkipConnection);
        assert_eq!(class_after(Unknown, b"HTTP/1.0 200 OK"), SkipConnection);
        // Body bytes of a client request that happen to look like a response.
        assert_eq!(class_after(Send, b"HTTP/1.1 "), Send);
        assert_eq!(class_after(SkipRequest, b"HTTP/1.1 "), SkipRequest);
    }

    #[test]
    fn non_post_demotes_send_only_after_its_response_was_read() {
        // Body continuation or a pipelined request: the POST's response must still be sent.
        assert_eq!(class_after(Send, b"GET /v1/models"), Send);
        let read = after_read(entry(Send));
        assert_eq!(after(read, b"GET /v1/models").class(), SkipRequest);
        assert_eq!(after(read, b"{\"model\""), read);
        assert!(!drops(read));
        // Only a POST is marked read, and a new POST starts unread.
        assert_eq!(after_read(entry(SkipRequest)), entry(SkipRequest));
        assert_eq!(after(read, b"POST /v1/chat"), entry(Send));
    }

    #[test]
    fn sticky_classes_and_other_bytes_keep_the_class() {
        assert_eq!(class_after(Unknown, b"PRI * HTTP/2.0"), H2);
        assert_eq!(class_after(H2, b"GET / HT"), H2);
        assert_eq!(class_after(SkipConnection, b"POST /v1"), SkipConnection);
        // Body bytes, frames and non-HTTP protocols leave the class (or no entry) as it is.
        for other in [
            &b"\x00\x00\x12\x04\x00\x00\x00\x00\x00"[..],
            b"{\"a\":1, \"b\":2}",
        ] {
            assert_eq!(class_after(Unknown, other), Unknown, "{other:?}");
            assert_eq!(class_after(SkipRequest, other), SkipRequest);
        }
        assert!(drops(entry(SkipRequest)) && drops(entry(SkipConnection)));
        assert!(!drops(entry(Unknown)) && !drops(entry(Send)) && !drops(entry(H2)));
    }

    #[test]
    fn request_head_split_before_its_method_is_sent() {
        // After a GET, a POST written as "POST" + rest, or after a stray CRLF, must not stay dropped.
        for short in [&b"POST"[..], b"", b"\r\nPOST /v1/chat"] {
            assert_eq!(class_after(SkipRequest, short), Unknown, "{short:?}");
        }
        let read = after_read(entry(Send));
        assert_eq!(after(read, b"0\r\n\r\n"), read);
        assert_eq!(class_after(Send, b"\r\n"), Send);
        assert_eq!(class_after(SkipConnection, b"POST"), SkipConnection);
    }

    #[test]
    fn host_verdict_skips_only_the_connection_the_event_came_from() {
        let sent = ClassEntry::new(5_000_000_000).with(Send);
        // The POST that made the entry, and a later one on the same connection.
        for event_ns in [5_000_000_000, 9_000_000_000] {
            let skipped = sent.skip_connection(event_ns);
            assert_eq!(skipped.map(ClassEntry::class), Some(SkipConnection));
            assert_eq!(skipped.map(ClassEntry::born), Some(sent.born()));
        }
        // Freed and reused by a new connection before userspace read the event.
        assert_eq!(sent.skip_connection(4_000_000_000), None);
    }
}

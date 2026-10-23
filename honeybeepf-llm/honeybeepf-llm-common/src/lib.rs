#![no_std]

pub const MAX_SSL_BUF_SIZE: usize = 4096;
/// Events one SSL call is split into; 4 x 4 KB covers a full 16 KB TLS record.
pub const MAX_SSL_FRAGMENTS: usize = 4;
pub const MAX_SSL_CAPTURE: usize = MAX_SSL_BUF_SIZE * MAX_SSL_FRAGMENTS;

// Indices into the LLM_STATS per-CPU counter map (kernel increments, userspace sums).
pub const LLM_STAT_ENTER: u32 = 0; // entry uprobe hits (SSL_read/write/_ex, SSL_do_handshake, SSL_free)
pub const LLM_STAT_SUBMIT: u32 = 1; // events submitted to SSL_EVENTS (every fragment)
pub const LLM_STAT_RESERVE_FAIL: u32 = 2; // SSL_EVENTS full at the first fragment, call lost
pub const LLM_STAT_DISCARD: u32 = 3; // discarded (ret <= 0, no session, read failure)
pub const LLM_STAT_EMPTY: u32 = 4; // submitted without payload (buf_filled = 0), SSL_free included
pub const LLM_STAT_TRUNCATED: u32 = 5; // call sent with bytes missing (> capture_limit, or no room later)
pub const LLM_STAT_HANDSHAKE: u32 = 6; // handshake events submitted
pub const LLM_STAT_FRAGMENT: u32 = 7; // fragments after the first (offset > 0), subset of submit
pub const LLM_STAT_FILTERED: u32 = 8; // calls dropped before reserving because SSL_CLASS skips them
pub const LLM_STAT_COUNT: usize = 9;
pub const LLM_STAT_NAMES: [&str; LLM_STAT_COUNT] = [
    "enter",
    "submit",
    "reserve_fail",
    "discard",
    "empty",
    "truncated",
    "handshake",
    "fragment",
    "filtered",
];

// SSL_CLASS classes per (tgid, SSL*); a connection without an entry sends every call.
pub const SSL_CLASS_SEND: u8 = 1; // last request is a POST, the only method HTTP/1.1 detection takes
pub const SSL_CLASS_SKIP_REQ: u8 = 2; // last request is another method: drop until the next request
pub const SSL_CLASS_SKIP_CONN: u8 = 3; // server side, or a Host no provider serves: drop for good
pub const SSL_CLASS_H2: u8 = 4; // HTTP/2 preface seen: send for good
/// Added to SEND by the first Read, so a later non-POST head starts a new request rather than being body bytes.
pub const SSL_CLASS_READ_SEEN: u8 = 0x80;
/// An SSL_CLASS value keeps the class in this low byte and the entry's creation time (ns) above it.
pub const SSL_CLASS_MASK: u64 = 0xff;

/// SKIP_CONN value replacing `entry` after an event at `event_ns`; None when the entry is newer than it.
pub fn ssl_class_skip_conn(entry: u64, event_ns: u64) -> Option<u64> {
    // Userspace lags the kernel: a newer entry is a later connection that reuses the SSL* address.
    let born = entry & !SSL_CLASS_MASK;
    (born <= event_ns).then_some(born | SSL_CLASS_SKIP_CONN as u64)
}

/// Class after a `len`-byte Write whose first 8 bytes are `head` (zeros when shorter); 0 is no entry.
#[inline(always)]
pub fn ssl_class_after_write(class: u8, head: [u8; 8], len: u32) -> u8 {
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

    if class == SSL_CLASS_SKIP_CONN || class == SSL_CLASS_H2 {
        return class;
    }
    let [a, b, c, d, e, f, g, h] = head;
    // A request head split before its method, or after a stray CRLF: send until the next head.
    if len < 8 || (a == b'\r' && b == b'\n') {
        return if class == SSL_CLASS_SKIP_REQ {
            0
        } else {
            class
        };
    }
    match word(&[a, b, c, d]) {
        POST if e == b' ' => SSL_CLASS_SEND,
        PRI if word(&[e, f, g, h]) == PREFACE_TAIL => SSL_CLASS_H2,
        // Only as the first Write: later it could be body bytes of a client request.
        HTTP if class == 0 && e == b'/' && f == b'1' && g == b'.' => SSL_CLASS_SKIP_CONN,
        GET | PUT | HEAD | DELETE | PATCH | OPTIONS | CONNECT if class != SSL_CLASS_SEND => {
            SSL_CLASS_SKIP_REQ
        }
        _ => class,
    }
}

/// Whether a call on a connection of this class is dropped in the kernel.
#[inline(always)]
pub fn ssl_class_drops(class: u8) -> bool {
    class == SSL_CLASS_SKIP_REQ || class == SSL_CLASS_SKIP_CONN
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct EventMetadata {
    pub pid: u32,
    pub _pad: u32,
    pub cgroup_id: u64,
    pub timestamp: u64,
}

#[cfg(feature = "user")]
unsafe impl aya::Pod for EventMetadata {}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct ConnectionEvent {
    pub metadata: EventMetadata,
    pub dest_addr: u32,
    pub dest_port: u16,
    pub address_family: u16,
}

#[cfg(feature = "user")]
unsafe impl aya::Pod for ConnectionEvent {}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct CommonConfig {
    pub probe_llm: u8,
    pub _pad: [u8; 7],
    pub probe_interval: u32,
}

#[cfg(feature = "user")]
unsafe impl aya::Pod for CommonConfig {}

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LlmDirection {
    Read = 0,
    Write = 1,
    Handshake = 2,
    Unknown = 255,
}

impl From<u8> for LlmDirection {
    fn from(v: u8) -> Self {
        match v {
            0 => Self::Read,
            1 => Self::Write,
            2 => Self::Handshake,
            _ => Self::Unknown,
        }
    }
}

/// Bytes of one SSL call sent to userspace; requests need only their head, which detection reads.
pub fn capture_limit(dir: LlmDirection) -> usize {
    match dir {
        LlmDirection::Read => MAX_SSL_CAPTURE,
        _ => MAX_SSL_BUF_SIZE,
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct LlmEvent {
    pub metadata: EventMetadata,
    pub is_handshake: u8, // 0 or 1 - separate from rw since handshake can occur during read/write
    pub rw: u8,           // Uses LlmDirection
    pub truncated: u8,    // 1 when len > capture_limit(rw): bytes past it are never sent
    pub closed: u8,       // 1 for SSL_free: the connection is gone and carries no data
    pub len: u32,         // total bytes of the SSL call, the same on each of its fragments
    pub buf_filled: u32,
    pub offset: u32,  // position of buf[0] within the call's data
    pub ssl_ptr: u64, // the call's SSL*, which identifies the connection
    pub buf: [u8; MAX_SSL_BUF_SIZE],
    pub latency_ns: u64,
    pub comm: [u8; 16],
}

// Manual Default implementation for LlmEvent (derives don't work well with large arrays in no_std)
impl Default for LlmEvent {
    fn default() -> Self {
        Self {
            metadata: EventMetadata::default(),
            is_handshake: 0,
            rw: 0,
            truncated: 0,
            closed: 0,
            len: 0,
            buf_filled: 0,
            offset: 0,
            ssl_ptr: 0,
            buf: [0u8; MAX_SSL_BUF_SIZE],
            latency_ns: 0,
            comm: [0u8; 16],
        }
    }
}

// No padding (ssl_ptr sits at offset 40); a bigger event means fewer ringbuf slots.
const _: () = assert!(core::mem::size_of::<LlmEvent>() == 4168);
const _: () = assert!(core::mem::offset_of!(LlmEvent, ssl_ptr) == 40);

#[cfg(feature = "user")]
unsafe impl aya::Pod for LlmEvent {}

/// Lightweight event emitted on sched_process_exec to trigger SSL re-discovery.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ExecEvent {
    pub pid: u32,
    pub _pad: u32,
}

#[cfg(feature = "user")]
unsafe impl aya::Pod for ExecEvent {}

/// Emitted when a process opens a `libssl.so*` path for reading.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct LibOpenEvent {
    pub pid: u32,
    /// Bytes in `path`, without the NUL.
    pub len: u32,
    /// `LIB_OPEN_SEEN` key; userspace writes it back once the file is handled.
    pub seen_key: [u64; 2],
    /// bpf_ktime_get_ns() at the open.
    pub ts: u64,
    pub path: [u8; MAX_FILENAME_LEN],
}

const _: () = assert!(core::mem::size_of::<LibOpenEvent>() == 4 + 4 + 16 + 8 + MAX_FILENAME_LEN);

// SAFETY: repr(C), only integer fields and no padding (checked by the assert above).
#[cfg(feature = "user")]
unsafe impl aya::Pod for LibOpenEvent {}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ProcessExitEvent {
    pub pid: u32,
    pub ppid: u32,
    pub exit_code: i32,
    pub _pad: u32,
    pub start_time: u64,
    pub exit_time: u64,
    pub cgroup_id: u64,
    pub comm: [u8; 16],
}

#[cfg(feature = "user")]
unsafe impl aya::Pod for ProcessExitEvent {}
// ============================================================
// File Access Events
// ============================================================

pub const MAX_FILENAME_LEN: usize = 256;
pub const MAX_COMM_LEN: usize = 16;
/// Maximum length of suffix patterns for wildcard matching (e.g., ".ssh/authorized_keys")
pub const MAX_SUFFIX_LEN: usize = 64;

/// File access event emitted on sys_enter_openat tracepoint
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FileAccessEvent {
    pub metadata: EventMetadata,
    pub tid: u32,
    pub flags: u32,
    pub mode: u32,
    pub ino: u64,
    pub dev: u32,
    pub _pad2: u32,
    pub comm: [u8; MAX_COMM_LEN],
    pub filename: [u8; MAX_FILENAME_LEN],
}

impl Default for FileAccessEvent {
    fn default() -> Self {
        Self {
            metadata: EventMetadata::default(),
            tid: 0,
            flags: 0,
            mode: 0,
            ino: 0,
            dev: 0,
            _pad2: 0,
            comm: [0u8; MAX_COMM_LEN],
            filename: [0u8; MAX_FILENAME_LEN],
        }
    }
}

#[cfg(feature = "user")]
unsafe impl aya::Pod for FileAccessEvent {}

#[cfg(test)]
mod tests {
    use super::*;

    fn head(s: &[u8]) -> [u8; 8] {
        let mut h = [0u8; 8];
        let n = s.len().min(8);
        h[..n].copy_from_slice(&s[..n]);
        h
    }

    fn after(class: u8, s: &[u8]) -> u8 {
        ssl_class_after_write(class, head(s), s.len() as u32)
    }

    #[test]
    fn requests_are_classed_by_method() {
        assert_eq!(after(0, b"POST /v1/chat"), SSL_CLASS_SEND);
        for req in [
            &b"GET /healthz"[..],
            b"PUT /x H",
            b"HEAD / H",
            b"DELETE /",
            b"PATCH /x",
            b"OPTIONS ",
            b"CONNECT ",
        ] {
            assert_eq!(after(0, req), SSL_CLASS_SKIP_REQ, "{req:?}");
            assert_eq!(
                after(SSL_CLASS_SKIP_REQ, req),
                SSL_CLASS_SKIP_REQ,
                "{req:?}"
            );
        }
        assert_eq!(
            after(SSL_CLASS_SKIP_REQ, b"POST /v1/messages"),
            SSL_CLASS_SEND
        );
        assert_eq!(after(0, b"POSTX /"), 0);
    }

    #[test]
    fn server_response_skips_only_as_first_write() {
        assert_eq!(after(0, b"HTTP/1.1 200 OK"), SSL_CLASS_SKIP_CONN);
        assert_eq!(after(0, b"HTTP/1.0 200 OK"), SSL_CLASS_SKIP_CONN);
        // Body bytes of a client request that happen to look like a response.
        assert_eq!(after(SSL_CLASS_SEND, b"HTTP/1.1 "), SSL_CLASS_SEND);
        assert_eq!(after(SSL_CLASS_SKIP_REQ, b"HTTP/1.1 "), SSL_CLASS_SKIP_REQ);
    }

    #[test]
    fn non_post_demotes_send_only_after_its_response_was_read() {
        // Body continuation or a pipelined request: the POST's response must still be sent.
        assert_eq!(after(SSL_CLASS_SEND, b"GET /v1/models"), SSL_CLASS_SEND);
        let read = SSL_CLASS_SEND | SSL_CLASS_READ_SEEN;
        assert_eq!(after(read, b"GET /v1/models"), SSL_CLASS_SKIP_REQ);
        assert_eq!(after(read, b"{\"model\""), read);
        assert!(!ssl_class_drops(read));
    }

    #[test]
    fn sticky_classes_and_other_bytes_keep_the_class() {
        assert_eq!(after(0, b"PRI * HTTP/2.0"), SSL_CLASS_H2);
        assert_eq!(after(SSL_CLASS_H2, b"GET / HT"), SSL_CLASS_H2);
        assert_eq!(after(SSL_CLASS_SKIP_CONN, b"POST /v1"), SSL_CLASS_SKIP_CONN);
        // Body bytes, frames and non-HTTP protocols leave the class (or no entry) as it is.
        for other in [
            &b"\x00\x00\x12\x04\x00\x00\x00\x00\x00"[..],
            b"{\"a\":1, \"b\":2}",
        ] {
            assert_eq!(after(0, other), 0, "{other:?}");
            assert_eq!(after(SSL_CLASS_SKIP_REQ, other), SSL_CLASS_SKIP_REQ);
        }
        assert!(ssl_class_drops(SSL_CLASS_SKIP_REQ) && ssl_class_drops(SSL_CLASS_SKIP_CONN));
        assert!(
            !ssl_class_drops(0)
                && !ssl_class_drops(SSL_CLASS_SEND)
                && !ssl_class_drops(SSL_CLASS_H2)
        );
    }

    #[test]
    fn request_head_split_before_its_method_is_sent() {
        // After a GET, a POST written as "POST" + rest, or after a stray CRLF, must not stay dropped.
        for short in [&b"POST"[..], b"", b"\r\nPOST /v1/chat"] {
            assert_eq!(after(SSL_CLASS_SKIP_REQ, short), 0, "{short:?}");
        }
        let read = SSL_CLASS_SEND | SSL_CLASS_READ_SEEN;
        assert_eq!(after(read, b"0\r\n\r\n"), read);
        assert_eq!(after(SSL_CLASS_SEND, b"\r\n"), SSL_CLASS_SEND);
        assert_eq!(after(SSL_CLASS_SKIP_CONN, b"POST"), SSL_CLASS_SKIP_CONN);
    }

    #[test]
    fn host_verdict_skips_only_the_connection_the_event_came_from() {
        let born = 5_000_000_000u64 & !SSL_CLASS_MASK;
        let entry = born | SSL_CLASS_SEND as u64;
        // The POST that made the entry, and a later one on the same connection.
        for event_ns in [5_000_000_000, 9_000_000_000] {
            assert_eq!(
                ssl_class_skip_conn(entry, event_ns),
                Some(born | SSL_CLASS_SKIP_CONN as u64)
            );
        }
        // Freed and reused by a new connection before userspace read the event.
        assert_eq!(ssl_class_skip_conn(entry, 4_000_000_000), None);
    }
}

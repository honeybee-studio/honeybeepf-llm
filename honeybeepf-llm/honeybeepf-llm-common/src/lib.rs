#![no_std]

pub const MAX_SSL_BUF_SIZE: usize = 4096;
/// Events one SSL call is split into; 4 x 4 KB covers a full 16 KB TLS record.
pub const MAX_SSL_FRAGMENTS: usize = 4;
pub const MAX_SSL_CAPTURE: usize = MAX_SSL_BUF_SIZE * MAX_SSL_FRAGMENTS;

// Indices into the LLM_STATS per-CPU counter map (kernel increments, userspace sums).
pub const LLM_STAT_ENTER: u32 = 0; // entry uprobe hits (SSL_read/write/_ex, SSL_do_handshake)
pub const LLM_STAT_SUBMIT: u32 = 1; // events submitted to SSL_EVENTS (every fragment)
pub const LLM_STAT_RESERVE_FAIL: u32 = 2; // SSL_EVENTS full at the first fragment, call lost
pub const LLM_STAT_DISCARD: u32 = 3; // discarded (ret <= 0, no session, read failure)
pub const LLM_STAT_EMPTY: u32 = 4; // submitted without payload (buf_filled = 0)
pub const LLM_STAT_TRUNCATED: u32 = 5; // call sent with bytes missing (> capture_limit, or no room later)
pub const LLM_STAT_HANDSHAKE: u32 = 6; // handshake events submitted
pub const LLM_STAT_FRAGMENT: u32 = 7; // fragments after the first (offset > 0), subset of submit
pub const LLM_STAT_COUNT: usize = 8;
pub const LLM_STAT_NAMES: [&str; LLM_STAT_COUNT] = [
    "enter",
    "submit",
    "reserve_fail",
    "discard",
    "empty",
    "truncated",
    "handshake",
    "fragment",
];

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
    pub _pad: u8,
    pub len: u32, // total bytes of the SSL call, the same on each of its fragments
    pub buf_filled: u32,
    pub offset: u32, // position of buf[0] within the call's data
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
            _pad: 0,
            len: 0,
            buf_filled: 0,
            offset: 0,
            buf: [0u8; MAX_SSL_BUF_SIZE],
            latency_ns: 0,
            comm: [0u8; 16],
        }
    }
}

// Fragment fields fill alignment padding; a bigger event means fewer ringbuf slots.
const _: () = assert!(core::mem::size_of::<LlmEvent>() == 4160);

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

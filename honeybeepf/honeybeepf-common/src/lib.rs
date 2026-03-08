#![no_std]

pub const MAX_SSL_BUF_SIZE: usize = 4096;

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

#[repr(C)]
#[derive(Clone, Copy)]
pub struct LlmEvent {
    pub metadata: EventMetadata,
    pub is_handshake: u8, // 0 or 1 - separate from rw since handshake can occur during read/write
    pub rw: u8,           // Uses LlmDirection
    pub len: u32,
    pub buf_filled: u32,
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
            len: 0,
            buf_filled: 0,
            buf: [0u8; MAX_SSL_BUF_SIZE],
            latency_ns: 0,
            comm: [0u8; 16],
        }
    }
}

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

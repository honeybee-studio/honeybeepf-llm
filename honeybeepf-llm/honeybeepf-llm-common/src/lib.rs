#![no_std]

pub const MAX_SSL_BUF_SIZE: usize = 4096;
/// Events one SSL call is split into; 4 x 4 KB covers a full 16 KB TLS record.
pub const MAX_SSL_FRAGMENTS: usize = 4;
pub const MAX_SSL_CAPTURE: usize = MAX_SSL_BUF_SIZE * MAX_SSL_FRAGMENTS;

/// Key into the plaintext endpoint map: destination IPv4 and port in network byte order, so the
/// kernel (`remote_ip4` is already network order, port via [`sock_ops_remote_port`]) and userspace
/// (`to_be()` on both) build the same bits.
pub const fn plain_endpoint_key(ip_be: u32, port_be: u16) -> u64 {
    ((ip_be as u64) << 16) | (port_be as u64)
}

/// Network-order destination port from a context's `remote_port`: both `bpf_sock_ops` and
/// `sk_msg_md` pack the be16 port into the high 16 bits (little-endian `skc_dport << 16`), so the
/// low bits are zero. Recovers the htons(port) bits a userspace key is built from.
pub const fn sock_ops_remote_port(remote_port: u32) -> u16 {
    (u32::from_be(remote_port) as u16).to_be()
}

/// Source port from a context's `local_port`: both `bpf_sock_ops` and `sk_msg_md` store it in host
/// byte order, so the low 16 bits are the port number.
pub const fn flow_local_port(local_port: u32) -> u16 {
    local_port as u16
}

/// 4-tuple identifying a TCP connection, built identically from `bpf_sock_ops` (at connect) and
/// `sk_msg_md` (at send) so both key `PLAIN_CONNS` the same without the socket cookie (a helper
/// `sk_msg` may not call). No padding: four fields pack to 12 bytes, so the key bytes are stable.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PlainFlowKey {
    pub local_ip4: u32,
    pub remote_ip4: u32,
    pub local_port: u16,
    pub remote_port: u16,
}

/// Build the flow key from a context's raw 4-tuple fields; `*_ip4` are network order on both
/// contexts, and the ports are normalized by [`sock_ops_remote_port`] and [`flow_local_port`].
pub const fn plain_flow_key(
    local_ip4: u32,
    remote_ip4: u32,
    local_port: u32,
    remote_port: u32,
) -> PlainFlowKey {
    PlainFlowKey {
        local_ip4,
        remote_ip4,
        local_port: flow_local_port(local_port),
        remote_port: sock_ops_remote_port(remote_port),
    }
}

/// Plaintext parser stream id, built the same way on send and receive. The parser key is `(pid, id)`,
/// and within one pid `remote_ip4`, `local_port` and `remote_port` already identify the connection,
/// so `local_ip4` is left out; packing instead of hashing keeps the value exact to test.
pub const fn plain_stream_id(key: &PlainFlowKey) -> u64 {
    ((key.remote_ip4 as u64) << 32) | ((key.local_port as u64) << 16) | key.remote_port as u64
}

/// Full max-entries the eBPF object declares for the plaintext maps; the loader keeps them this big
/// only when capture is on. Hash maps prealloc, so PLAIN_CONNS alone costs ~6.5 MB at full size.
pub const PLAIN_ENDPOINTS_ENTRIES: u32 = 8192;
pub const PLAIN_SOCKS_ENTRIES: u32 = 65536;
pub const PLAIN_CONNS_ENTRIES: u32 = 65536;

/// Load-time max-entries for the three plaintext hash/sock maps.
pub struct PlainMapEntries {
    pub endpoints: u32,
    pub socks: u32,
    pub conns: u32,
}

/// Full sizes when plaintext capture is configured; one slot each when off, so a TLS-only node does
/// not prealloc the ~8 MB these maps cost at full size.
pub const fn plain_map_entries(configured: bool) -> PlainMapEntries {
    if configured {
        PlainMapEntries {
            endpoints: PLAIN_ENDPOINTS_ENTRIES,
            socks: PLAIN_SOCKS_ENTRIES,
            conns: PLAIN_CONNS_ENTRIES,
        }
    } else {
        PlainMapEntries {
            endpoints: 1,
            socks: 1,
            conns: 1,
        }
    }
}

/// A counter in the LLM_STATS per-CPU map (the kernel increments, user space sums). The discriminant is
/// the map index and `name` is the `stage` label of `llm_events_total`.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LlmStat {
    /// Entry uprobe hits (SSL_read/write/_ex, SSL_do_handshake, SSL_free).
    Enter,
    /// Events submitted to SSL_EVENTS (every fragment).
    Submit,
    /// SSL_EVENTS full at the first fragment, call lost.
    ReserveFail,
    /// Discarded (ret <= 0, no session, read failure).
    Discard,
    /// Submitted without payload (buf_filled = 0), SSL_free included.
    Empty,
    /// Call sent with bytes missing (> capture_limit, or no room later).
    Truncated,
    /// Handshake events submitted.
    Handshake,
    /// Fragments after the first (offset > 0), a subset of `Submit`.
    Fragment,
    /// Calls dropped before reserving because SSL_CLASS skips them.
    Filtered,
    /// Plaintext receive skb past the 16 KB capture window (loopback, GRO).
    PlainRecvTruncated,
}

impl LlmStat {
    /// Every counter, in map-index order.
    pub const ALL: [Self; 10] = [
        Self::Enter,
        Self::Submit,
        Self::ReserveFail,
        Self::Discard,
        Self::Empty,
        Self::Truncated,
        Self::Handshake,
        Self::Fragment,
        Self::Filtered,
        Self::PlainRecvTruncated,
    ];
    pub const COUNT: usize = Self::ALL.len();

    /// The `stage` label value.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Enter => "enter",
            Self::Submit => "submit",
            Self::ReserveFail => "reserve_fail",
            Self::Discard => "discard",
            Self::Empty => "empty",
            Self::Truncated => "truncated",
            Self::Handshake => "handshake",
            Self::Fragment => "fragment",
            Self::Filtered => "filtered",
            Self::PlainRecvTruncated => "plain_recv_truncated",
        }
    }
}

pub mod ssl_class;

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

/// Fixed-size prefix of an SSL event; `buf` follows it, so a record can be sent as header + the
/// payload bytes it actually holds rather than the whole struct.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct LlmEventHeader {
    pub metadata: EventMetadata,
    pub ssl_ptr: u64, // the call's SSL*, which identifies the connection
    pub latency_ns: u64,
    pub len: u32, // total bytes of the SSL call, the same on each of its fragments
    pub buf_filled: u32, // 0 when the record carries no payload (SSL_free, handshake, read error)
    pub offset: u32, // position of buf[0] within the call's data
    pub is_handshake: u8, // 0 or 1 - separate from rw since handshake can occur during read/write
    pub rw: u8,   // Uses LlmDirection
    pub truncated: u8, // 1 when len > capture_limit(rw): bytes past it are never sent
    pub closed: u8, // 1 for SSL_free: the connection is gone and carries no data
    pub comm: [u8; 16],
}

impl LlmEventHeader {
    /// Payload bytes this fragment carries in `buf`, from the capture window; 0 when `buf_filled` is 0.
    pub fn payload_len(&self) -> usize {
        if self.buf_filled == 0 {
            return 0;
        }
        let avail = self.len.saturating_sub(self.offset) as usize;
        if avail < MAX_SSL_BUF_SIZE {
            avail
        } else {
            MAX_SSL_BUF_SIZE
        }
    }
}

#[cfg(feature = "user")]
unsafe impl aya::Pod for LlmEventHeader {}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct LlmEvent {
    pub header: LlmEventHeader,
    pub buf: [u8; MAX_SSL_BUF_SIZE],
}

// Manual Default (derives don't work well with large arrays in no_std).
impl Default for LlmEvent {
    fn default() -> Self {
        Self {
            header: LlmEventHeader::default(),
            buf: [0u8; MAX_SSL_BUF_SIZE],
        }
    }
}

#[cfg(feature = "user")]
impl LlmEvent {
    /// The payload bytes in `buf`, sized by the header's capture window.
    pub fn payload(&self) -> &[u8] {
        &self.buf[..self.header.payload_len()]
    }
}

// No padding: the header packs to 72 B and buf follows it; a bigger event means fewer ringbuf slots.
const _: () = assert!(core::mem::size_of::<LlmEventHeader>() == 72);
const _: () = assert!(core::mem::offset_of!(LlmEventHeader, ssl_ptr) == 24);
const _: () = assert!(core::mem::size_of::<LlmEvent>() == 4168);
const _: () = assert!(core::mem::offset_of!(LlmEvent, buf) == 72);

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

    #[test]
    fn llm_stats_are_listed_in_map_index_order_with_unique_names() {
        for (i, stat) in LlmStat::ALL.iter().enumerate() {
            assert_eq!(
                *stat as usize, i,
                "{stat:?} is out of order in LlmStat::ALL"
            );
            let earlier = &LlmStat::ALL[..i];
            assert!(
                earlier.iter().all(|s| s.name() != stat.name()),
                "duplicate stage name {}",
                stat.name()
            );
        }
        assert_eq!(LlmStat::PlainRecvTruncated.name(), "plain_recv_truncated");
    }

    #[test]
    fn remote_port_reads_the_high_16_bits() {
        // Port 18090 = 0x46AA; the kernel stores htons(18090)=0xAA46 in the high half.
        assert_eq!(sock_ops_remote_port(0xAA46_0000), 0xAA46);
        // And the key then matches a userspace-built one (ip network-order, port htons).
        let ip_be = u32::from_be_bytes([127, 0, 0, 1]).to_be();
        assert_eq!(
            plain_endpoint_key(ip_be, sock_ops_remote_port(0xAA46_0000)),
            plain_endpoint_key(ip_be, 18090u16.to_be()),
        );
    }

    #[test]
    fn flow_key_normalizes_ports_from_raw_context_fields() {
        // Raw 4-tuple as both contexts present it: ip4 network order, remote_port be16 in the high
        // 16 bits, local_port host order. 10.0.0.5:33000 -> 127.0.0.1:18090.
        let local_ip = u32::from_be_bytes([10, 0, 0, 5]).to_be();
        let remote_ip = u32::from_be_bytes([127, 0, 0, 1]).to_be();
        let key = plain_flow_key(local_ip, remote_ip, 33000, 0xAA46_0000);
        assert_eq!(key.remote_port, 18090u16.to_be()); // htons(18090) = 0xAA46
        assert_eq!(key.local_port, 33000);
        assert_eq!(key.local_ip4, local_ip);
        assert_eq!(key.remote_ip4, remote_ip);
    }

    #[test]
    fn stream_id_packs_remote_ip_and_ports_and_ignores_local_ip() {
        let remote_ip = u32::from_be_bytes([127, 0, 0, 1]).to_be();
        let key = plain_flow_key(
            u32::from_be_bytes([10, 0, 0, 5]).to_be(),
            remote_ip,
            33000,
            0xAA46_0000,
        );
        let want = ((remote_ip as u64) << 32) | (33000u64 << 16) | 18090u16.to_be() as u64;
        assert_eq!(plain_stream_id(&key), want);
        // Same connection from the other local_ip4 still yields the same id (local_ip4 is dropped).
        let other = plain_flow_key(
            u32::from_be_bytes([10, 0, 0, 9]).to_be(),
            remote_ip,
            33000,
            0xAA46_0000,
        );
        assert_eq!(plain_stream_id(&other), want);
    }

    #[test]
    fn plain_map_entries_are_one_slot_when_off_and_full_when_on() {
        let off = plain_map_entries(false);
        assert_eq!((off.endpoints, off.socks, off.conns), (1, 1, 1));
        let on = plain_map_entries(true);
        assert_eq!(
            (on.endpoints, on.socks, on.conns),
            (
                PLAIN_ENDPOINTS_ENTRIES,
                PLAIN_SOCKS_ENTRIES,
                PLAIN_CONNS_ENTRIES
            ),
        );
    }
}

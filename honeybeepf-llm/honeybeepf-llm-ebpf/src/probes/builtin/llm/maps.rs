use aya_ebpf::{
    macros::map,
    maps::{HashMap, LruHashMap, PerCpuArray, ProgramArray, RingBuf},
};
use honeybeepf_llm_common::LlmStat;

pub const MAX_ENTRIES: u32 = 10240;
/// Open TLS connections tracked per node; an evicted one is sent in full again.
pub const SSL_CLASS_ENTRIES: u32 = 32768;
pub const SSL_RINGBUF_SIZE: u32 = 8 * 1024 * 1024; // 8MB
pub const EXEC_RINGBUF_SIZE: u32 = 64 * 1024; // 64KB

#[map]
pub static SSL_EVENTS: RingBuf = RingBuf::with_byte_size(SSL_RINGBUF_SIZE, 0);

#[map]
pub static START_NS: HashMap<u32, u64> = HashMap::with_max_entries(MAX_ENTRIES, 0);

/// (buf, SSL*) of the call in progress; return probes can't read the arguments.
#[map]
pub static BUFS: HashMap<u32, [u64; 2]> = HashMap::with_max_entries(MAX_ENTRIES, 0);

#[map]
pub static READBYTES_PTRS: HashMap<u32, u64> = HashMap::with_max_entries(MAX_ENTRIES, 0);

/// [start ns, buf, *tls.Conn] of each Go `(*Conn).Read` in progress, keyed by [tgid, goroutine]: a
/// goroutine can resume on another thread before Read returns. LRU, since a goroutine can end inside it.
#[map]
pub static GO_READS: LruHashMap<[u64; 2], [u64; 3]> = LruHashMap::with_max_entries(MAX_ENTRIES, 0);

/// Most TLS records kept per thread between two TCP sends; a client's first send holds its handshake
/// record and two full records.
pub const SEAL_QUEUE: usize = 4;

/// The TLS records a thread sealed with a context of unknown socket since its last TCP send, each as
/// [buf, len, context]. A null `buf` has no data: `len` 0 starts or ends a TLS session, any other only binds.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SealQueue {
    pub len: u64,
    pub records: [[u64; 3]; SEAL_QUEUE],
}

#[map]
pub static AEAD_SEALS: LruHashMap<u32, SealQueue> = LruHashMap::with_max_entries(MAX_ENTRIES, 0);

/// TCP socket of each cipher context, keyed by [tgid, context].
#[map]
pub static AEAD_CTX: LruHashMap<[u64; 2], u64> = LruHashMap::with_max_entries(MAX_ENTRIES, 0);

/// [buf, len, context] of the TLS record a thread is opening.
#[map]
pub static AEAD_OPENS: LruHashMap<u32, [u64; 3]> = LruHashMap::with_max_entries(MAX_ENTRIES, 0);

/// Socket of each thread's last TCP receive, in the processes of `AEAD_TGIDS`.
#[map]
pub static AEAD_RECV_SOCK: LruHashMap<u32, u64> = LruHashMap::with_max_entries(MAX_ENTRIES, 0);

/// Processes that called a probed AEAD function, or that userspace saw start from a probed executable.
#[map]
pub static AEAD_TGIDS: LruHashMap<u32, u8> = LruHashMap::with_max_entries(MAX_ENTRIES, 0);

/// `ssl_class::ClassEntry` of each connection, keyed by [tgid, SSL*]; userspace adds SkipConnection by Host.
#[map]
pub static SSL_CLASS: LruHashMap<[u64; 2], u64> =
    LruHashMap::with_max_entries(SSL_CLASS_ENTRIES, 0);

/// Event pipeline counters, indexed by `honeybeepf_llm_common::LlmStat`.
#[map]
pub static LLM_STATS: PerCpuArray<u64> = PerCpuArray::with_max_entries(LlmStat::COUNT as u32, 0);

/// Always empty; a program with a tail call gets no per-CPU private stack from the verifier (6.13+).
#[map]
pub static NO_PRIV_STACK: ProgramArray = ProgramArray::with_max_entries(1, 0);

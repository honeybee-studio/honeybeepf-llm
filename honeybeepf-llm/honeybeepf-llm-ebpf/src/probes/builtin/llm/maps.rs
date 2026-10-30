use aya_ebpf::{
    macros::map,
    maps::{HashMap, LruHashMap, PerCpuArray, ProgramArray, RingBuf},
};
use honeybeepf_llm_common::LLM_STAT_COUNT;

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

/// `ssl_class::ClassEntry` of each connection, keyed by [tgid, SSL*]; userspace adds SkipConnection by Host.
#[map]
pub static SSL_CLASS: LruHashMap<[u64; 2], u64> =
    LruHashMap::with_max_entries(SSL_CLASS_ENTRIES, 0);

/// Event pipeline counters, indexed by `honeybeepf_llm_common::LLM_STAT_*`.
#[map]
pub static LLM_STATS: PerCpuArray<u64> = PerCpuArray::with_max_entries(LLM_STAT_COUNT as u32, 0);

/// Always empty; a program with a tail call gets no per-CPU private stack from the verifier (6.13+).
#[map]
pub static NO_PRIV_STACK: ProgramArray = ProgramArray::with_max_entries(1, 0);

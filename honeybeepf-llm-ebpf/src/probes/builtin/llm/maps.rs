use aya_ebpf::{
    macros::map,
    maps::{HashMap, RingBuf},
};

pub const MAX_ENTRIES: u32 = 10240;
pub const SSL_RINGBUF_SIZE: u32 = 8 * 1024 * 1024; // 8MB
pub const EXEC_RINGBUF_SIZE: u32 = 64 * 1024; // 64KB

#[map]
pub static SSL_EVENTS: RingBuf = RingBuf::with_byte_size(SSL_RINGBUF_SIZE, 0);

#[map]
pub static START_NS: HashMap<u32, u64> = HashMap::with_max_entries(MAX_ENTRIES, 0);

#[map]
pub static BUFS: HashMap<u32, u64> = HashMap::with_max_entries(MAX_ENTRIES, 0);

#[map]
pub static READBYTES_PTRS: HashMap<u32, u64> = HashMap::with_max_entries(MAX_ENTRIES, 0);

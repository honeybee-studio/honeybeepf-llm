//! Keeps each connection's SSL_CLASS entry and drops the calls its class skips, before any ringbuf reserve.

use aya_ebpf::helpers::{bpf_get_current_pid_tgid, bpf_probe_read_user};
use honeybeepf_llm_common::{
    LlmDirection,
    ssl_class::{self, ClassEntry},
};

use super::{helpers::SslCall, maps::SSL_CLASS};

/// Whether the connection's class drops this call; a Write first reclasses it by its first 8 bytes.
#[inline(always)]
pub fn drops(call: &SslCall, rw: u8) -> bool {
    let key = [call.metadata.pid as u64, call.ssl];
    // SAFETY: the value is copied out at once, before an update or eviction can free it.
    let entry = unsafe { SSL_CLASS.get(&key) }
        .copied()
        .map_or(ClassEntry::new(call.metadata.timestamp), ClassEntry);
    let next = if rw == LlmDirection::Write as u8 {
        let head = if call.len >= 8 {
            // SAFETY: the helper checks the user address; a fault returns Err, read as no head.
            unsafe { bpf_probe_read_user(call.buf_addr as *const [u8; 8]) }.unwrap_or([0; 8])
        } else {
            [0; 8]
        };
        ssl_class::after_write(entry, head, call.len)
    } else {
        ssl_class::after_read(entry)
    };
    if next != entry {
        let _ = SSL_CLASS.insert(&key, &next.0, 0);
    }
    ssl_class::drops(next)
}

/// Drop the connection's class, since its SSL* address can be reused by the next connection.
#[inline(always)]
pub fn forget(ssl: u64) {
    let tgid = bpf_get_current_pid_tgid() >> 32;
    let _ = SSL_CLASS.remove(&[tgid, ssl]);
}

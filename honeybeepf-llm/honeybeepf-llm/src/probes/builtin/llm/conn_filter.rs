//! Userspace half of the kernel filter: a request head to a host no provider serves skips its connection.

use anyhow::{Context, Result};
use aya::{
    Ebpf,
    maps::{HashMap as BpfHashMap, MapData},
};
use honeybeepf_llm_common::{LlmEvent, MAX_SSL_BUF_SIZE, ssl_class::ClassEntry};
use honeybeepf_llm_core::types::LlmDirection;

use super::{StreamMap, http, processor::StreamProcessor};

/// `BPF_EXIST` from linux/bpf.h: an entry SSL_free removed since the lookup is not brought back.
const BPF_EXIST: u64 = 2;

/// Writes connection verdicts into the kernel's SSL_CLASS map ([tgid, SSL*] -> class and creation time).
pub struct ConnFilter {
    map: BpfHashMap<MapData, [u64; 2], u64>,
}

impl ConnFilter {
    pub fn new(bpf: &mut Ebpf) -> Result<Self> {
        let map = bpf
            .take_map("SSL_CLASS")
            .context("Failed to get SSL_CLASS map")?;
        Ok(Self {
            map: BpfHashMap::try_from(map)?,
        })
    }

    /// Whether the event opens a request to a non-provider host and was consumed: its stream dropped, its
    /// connection skipped in the kernel.
    pub fn skip_if_foreign(&mut self, event: &LlmEvent, streams: &StreamMap) -> bool {
        if !opens_foreign_request(event) {
            return false;
        }
        let key = (event.metadata.pid, event.ssl_ptr);
        {
            let mut map = streams.lock().unwrap_or_else(|e| e.into_inner());
            // A stream with an LLM call in progress keeps going, so it is still recorded or counted.
            if map.get(&key).is_some_and(StreamProcessor::is_llm) {
                return false;
            }
            map.remove(&key);
        }
        self.skip_connection(event);
        true
    }

    fn skip_connection(&mut self, event: &LlmEvent) {
        let key = [u64::from(event.metadata.pid), event.ssl_ptr];
        if let Some(entry) = self
            .map
            .get(&key, 0)
            .ok()
            .and_then(|v| ClassEntry(v).skip_connection(event.metadata.timestamp))
        {
            let _ = self.map.insert(key, entry.0, BPF_EXIST);
        }
    }
}

/// The SSL call bytes this event carries.
pub(super) fn payload(event: &LlmEvent) -> &[u8] {
    if event.buf_filled == 0 {
        return &[];
    }
    let len = std::cmp::min(
        event.len.saturating_sub(event.offset) as usize,
        MAX_SSL_BUF_SIZE,
    );
    &event.buf[..len]
}

/// Whether the event starts a request to a host no provider serves; an HTTP/1.1 connection keeps one Host.
pub(super) fn opens_foreign_request(event: &LlmEvent) -> bool {
    LlmDirection::from(event.rw) == LlmDirection::Write
        && event.offset == 0
        && http::Http11Parser::has_foreign_host(payload(event))
}

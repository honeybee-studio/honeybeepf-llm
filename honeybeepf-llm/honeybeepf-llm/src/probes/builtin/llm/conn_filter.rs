//! Userspace half of the kernel filter: a request head to a host no provider serves skips its connection.

use anyhow::{Context, Result};
use aya::{
    Ebpf,
    maps::{HashMap as BpfHashMap, MapData},
};
use honeybeepf_llm_common::{LlmEventHeader, ssl_class::ClassEntry};
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
    pub fn skip_if_foreign(
        &mut self,
        header: &LlmEventHeader,
        data: &[u8],
        streams: &StreamMap,
    ) -> bool {
        if !opens_foreign_request(header, data) {
            return false;
        }
        let key = (header.metadata.pid, header.ssl_ptr);
        {
            let mut map = streams.lock().unwrap_or_else(|e| e.into_inner());
            // A stream with an LLM call in progress keeps going, so it is still recorded or counted.
            if map.get(&key).is_some_and(StreamProcessor::is_llm) {
                return false;
            }
            map.remove(&key);
        }
        self.skip_connection(header);
        true
    }

    fn skip_connection(&mut self, header: &LlmEventHeader) {
        let key = [u64::from(header.metadata.pid), header.ssl_ptr];
        if let Some(entry) = self
            .map
            .get(&key, 0)
            .ok()
            .and_then(|v| ClassEntry(v).skip_connection(header.metadata.timestamp))
        {
            let _ = self.map.insert(key, entry.0, BPF_EXIST);
        }
    }
}

/// Whether the event starts a request to a host no provider serves; an HTTP/1.1 connection keeps one Host.
pub(super) fn opens_foreign_request(header: &LlmEventHeader, data: &[u8]) -> bool {
    LlmDirection::from(header.rw) == LlmDirection::Write
        && header.offset == 0
        && http::Http11Parser::has_foreign_host(data)
}

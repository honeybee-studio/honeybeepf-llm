pub mod http;
pub mod processor;
#[cfg(test)]
mod secret_leak_tests;

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};

use anyhow::{Context, Result};
use aya::{
    Ebpf,
    maps::{MapData, PerCpuArray},
};
use honeybeepf_llm_common::{LLM_STAT_COUNT, LLM_STAT_NAMES, LlmEvent};
use honeybeepf_llm_core::types::LlmDirection;
use log::{info, warn};
use processor::{Outcome, StreamProcessor};
use regex::Regex;

use crate::probes::{
    DynamicProbe, IdentityResolver, Probe, ProcessInfo, attach_uprobe, detach_uprobe,
    discovery::{FileId, file_identity, find_libraries_all},
    spawn_ringbuf_handler,
};
// Re-export exec watch types and functions for lib.rs
pub use crate::probes::{
    ExecNotify, ExecPidQueue, LibOpenQueue, setup_exec_watch, setup_lib_open_watch,
};

// Timing constants
const CLEANUP_INTERVAL_SECS: u64 = 30; // How often to run cleanup
const CONNECTION_RETENTION_SECS: u64 = 300; // Keep idle connections for 5 minutes
const STATS_LOG_INTERVAL_SECS: u64 = 10; // How often to log event counters (only when changed)

/// Kernel-side event pipeline counters (LLM_STATS map), set once when the probe attaches.
static EVENT_STATS: OnceLock<PerCpuArray<MapData, u64>> = OnceLock::new();

/// Totals of the kernel event counters, summed over CPUs, in `LLM_STAT_NAMES` order.
pub fn event_stats() -> Option<[u64; LLM_STAT_COUNT]> {
    let map = EVENT_STATS.get()?;
    let mut totals = [0u64; LLM_STAT_COUNT];
    for (i, total) in totals.iter_mut().enumerate() {
        let values = map.get(&(i as u32), 0).ok()?;
        *total = values.iter().sum();
    }
    Some(totals)
}

/// Log the kernel event counters as one line.
pub fn log_event_stats() {
    if let Some(totals) = event_stats() {
        let line = LLM_STAT_NAMES
            .iter()
            .zip(totals.iter())
            .map(|(name, v)| format!("{}={}", name, v))
            .collect::<Vec<_>>()
            .join(" ");
        info!("[LLM stats] {}", line);
    }
}

/// (entry program, return program, function); each pair only works together.
const REQUIRED_PROBES: [(&str, &str, &str); 2] = [
    ("probe_ssl_rw_enter", "probe_ssl_read_exit", "SSL_read"),
    ("probe_ssl_rw_enter", "probe_ssl_write_exit", "SSL_write"),
];

/// Handshake events are dropped in userspace; _ex variants are not in every OpenSSL build.
const OPTIONAL_PROBES: [(&str, &str, &str); 3] = [
    (
        "probe_ssl_do_handshake_enter",
        "probe_ssl_do_handshake_exit",
        "SSL_do_handshake",
    ),
    (
        "probe_ssl_rw_ex_enter",
        "probe_ssl_write_ex_exit",
        "SSL_write_ex",
    ),
    (
        "probe_ssl_rw_ex_enter",
        "probe_ssl_read_ex_exit",
        "SSL_read_ex",
    ),
];

pub fn attach_probes_to_path(bpf: &mut Ebpf, libssl_path: &str) -> Result<()> {
    // Roll back on failure so a retry on the same file doesn't stack duplicate probes.
    let mut attached = Vec::with_capacity(REQUIRED_PROBES.len() * 2);
    for (enter, exit, func) in REQUIRED_PROBES {
        for prog in [enter, exit] {
            match attach_uprobe(bpf, prog, func, libssl_path) {
                Ok(link) => attached.push((prog, link)),
                Err(e) => {
                    for (prog, link) in attached {
                        if let Err(de) = detach_uprobe(bpf, prog, link) {
                            warn!("Failed to roll back {} on {}: {}", prog, libssl_path, de);
                        }
                    }
                    return Err(e);
                }
            }
        }
    }

    for (enter, exit, func) in OPTIONAL_PROBES {
        if let Ok(link) = attach_uprobe(bpf, enter, func, libssl_path)
            && attach_uprobe(bpf, exit, func, libssl_path).is_err()
        {
            let _ = detach_uprobe(bpf, enter, link);
        }
    }

    // Without it a connection's stream is only dropped after CONNECTION_RETENTION_SECS idle.
    if let Err(e) = attach_uprobe(bpf, "probe_ssl_free", "SSL_free", libssl_path) {
        warn!("SSL_free not probed in {}: {}", libssl_path, e);
    }

    Ok(())
}

/// One path per distinct file not in `known`, preferring host paths over `/proc/<pid>/root`.
fn unique_new_files<'a>(
    paths: impl IntoIterator<Item = &'a String>,
    known: &HashSet<FileId>,
) -> Vec<(&'a str, FileId)> {
    // A /proc/<pid>/root path disappears when that process exits before the attach.
    let mut paths: Vec<&String> = paths.into_iter().collect();
    paths.sort_by_key(|p| p.starts_with("/proc/"));
    let mut out: Vec<(&str, FileId)> = Vec::new();
    for path in paths {
        let Some(id) = file_identity(path) else {
            continue;
        };
        if !known.contains(&id) && !out.iter().any(|(_, seen)| *seen == id) {
            out.push((path, id));
        }
    }
    out
}

pub struct LlmProbe {
    /// Library files already tried (attached or failed), so each file is attempted once.
    known_targets: Arc<Mutex<HashSet<FileId>>>,
}

impl Default for LlmProbe {
    fn default() -> Self {
        Self {
            known_targets: Arc::new(Mutex::new(HashSet::new())),
        }
    }
}

impl DynamicProbe for LlmProbe {
    fn on_exec(&self, bpf: &mut Ebpf, process_info: &ProcessInfo) -> Result<()> {
        // Collect candidates while holding the lock briefly
        let candidates = {
            let known = self.known_targets.lock().unwrap_or_else(|e| e.into_inner());
            let ssl_libs = process_info.libs.iter().filter(|path| {
                // Match against basename with a strict prefix check
                std::path::Path::new(path)
                    .file_name()
                    .and_then(|f| f.to_str())
                    .map(|f| f.starts_with("libssl.so"))
                    .unwrap_or(false)
            });
            unique_new_files(ssl_libs, &known)
        };

        // Attach probes without holding the lock
        for (path, id) in candidates {
            info!(
                "[Re-discovery] New SSL library found: {} (PID: {}, ino {})",
                path,
                process_info.pid,
                id.ino()
            );
            let result = attach_probes_to_path(bpf, path);
            self.known_targets
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(id);
            if let Err(e) = result {
                warn!("[Re-discovery] Failed to attach to {}: {}", path, e);
            }
        }
        Ok(())
    }
}

/// (pid, SSL*): one stream per TLS connection, whichever threads read and write it.
type StreamKey = (u32, u64);
type StreamMap = Arc<Mutex<HashMap<StreamKey, StreamProcessor>>>;

/// Feed one ringbuf event to its connection's stream, or end the stream on SSL_free.
fn route_event(
    streams: &mut HashMap<StreamKey, StreamProcessor>,
    event: &LlmEvent,
    // Called only for events that use it: a pod lookup miss reads /proc.
    #[cfg(feature = "k8s")] pod_info: impl FnOnce() -> Option<Arc<crate::k8s::PodInfo>>,
) -> Outcome {
    let key = (event.metadata.pid, event.ssl_ptr);
    if event.closed != 0 {
        return match streams.remove(&key) {
            Some(mut stream) if stream.is_llm() => stream.close(
                event.metadata.pid,
                #[cfg(feature = "k8s")]
                pod_info().as_ref(),
            ),
            _ => Outcome::Pending,
        };
    }
    let data_len = std::cmp::min(
        event.len.saturating_sub(event.offset) as usize,
        honeybeepf_llm_common::MAX_SSL_BUF_SIZE,
    );
    if event.buf_filled == 0 || data_len == 0 {
        return Outcome::Pending;
    }
    streams.entry(key).or_default().handle_fragment(
        LlmDirection::from(event.rw),
        &event.buf[..data_len],
        event.offset,
        event.len,
        event.truncated != 0,
        event.metadata.pid,
        #[cfg(feature = "k8s")]
        pod_info(),
    )
}

impl Probe for LlmProbe {
    fn attach(&self, bpf: &mut Ebpf, _resolver: IdentityResolver) -> Result<()> {
        let targets = find_all_targets()?;

        // Keep going: libssl opened later is attached by re-discovery and needs the reader below.
        if targets.is_empty() {
            warn!("No libssl loaded yet; waiting for processes that load it.");
        }

        let stats = PerCpuArray::try_from(
            bpf.take_map("LLM_STATS")
                .context("Failed to get LLM_STATS map")?,
        )?;
        let _ = EVENT_STATS.set(stats);

        // Skip libcrypto for SSL_* probes as they usually don't contain them
        let ssl_libs = targets.iter().filter(|path| !path.contains("libcrypto"));
        let candidates = {
            let known = self.known_targets.lock().unwrap_or_else(|e| e.into_inner());
            unique_new_files(ssl_libs, &known)
        };

        for (path, id) in candidates {
            info!(
                "Attaching LLM (SSL) probes to detected library: {} (ino {})",
                path,
                id.ino()
            );
            let result = attach_probes_to_path(bpf, path);
            self.known_targets
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(id);
            if let Err(e) = result {
                warn!("Failed to attach to {}: {}", path, e);
            }
        }

        let state: StreamMap = Arc::new(Mutex::new(HashMap::new()));
        let handler_state = state.clone();

        spawn_ringbuf_handler(bpf, "SSL_EVENTS", move |event: LlmEvent| {
            if event.is_handshake == 1 {
                return;
            }

            let mut map = handler_state.lock().unwrap_or_else(|e| e.into_inner());
            route_event(
                &mut map,
                &event,
                #[cfg(feature = "k8s")]
                || _resolver.resolve_pod(event.metadata.pid, event.metadata.cgroup_id),
            );
        })?;

        start_stats_log_task(state.clone());
        start_cleanup_task(state);

        Ok(())
    }
}

fn start_cleanup_task(state: StreamMap) {
    let shutdown = crate::probes::shutdown_flag();
    std::thread::spawn(move || {
        while !shutdown.load(std::sync::atomic::Ordering::Relaxed) {
            std::thread::sleep(Duration::from_secs(CLEANUP_INTERVAL_SECS));
            let mut map = state.lock().unwrap_or_else(|e| e.into_inner());
            let now = std::time::Instant::now();

            map.retain(|_, v| {
                now.duration_since(v.last_activity()).as_secs() < CONNECTION_RETENTION_SECS
            });
        }
    });
}

/// Log the kernel event counters and the open stream count every STATS_LOG_INTERVAL_SECS when they changed.
fn start_stats_log_task(state: StreamMap) {
    let shutdown = crate::probes::shutdown_flag();
    std::thread::spawn(move || {
        let (mut last, mut last_streams) = (None, 0);
        while !shutdown.load(std::sync::atomic::Ordering::Relaxed) {
            std::thread::sleep(Duration::from_secs(STATS_LOG_INTERVAL_SECS));
            let current = event_stats();
            if current != last {
                log_event_stats();
                last = current;
            }
            let streams = state.lock().unwrap_or_else(|e| e.into_inner()).len();
            if streams != last_streams {
                info!("[LLM streams] {} open", streams);
                last_streams = streams;
            }
        }
    });
}

/// Find all SSL libraries across running processes and system defaults.
pub fn find_all_targets() -> Result<HashSet<String>> {
    let ssl_pattern = Regex::new(r"libssl\.so\..*|libcrypto\.so\..*")?;
    find_libraries_all(&ssl_pattern, Some("libssl.so"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use honeybeepf_llm_common::{EventMetadata, MAX_SSL_BUF_SIZE};

    const OPENAI_REQUEST: &[u8] = b"POST /v1/chat/completions HTTP/1.1\r\nHost: api.openai.com\r\nContent-Type: application/json\r\n\r\n{\"model\":\"gpt-4o\",\"stream\":true,\"messages\":[]}";
    const ANTHROPIC_REQUEST: &[u8] = b"POST /v1/messages HTTP/1.1\r\nHost: api.anthropic.com\r\nContent-Type: application/json\r\n\r\n{\"model\":\"claude-test\",\"stream\":true,\"messages\":[]}";
    const SSE_HEAD: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n";
    const OPENAI_DELTA: &[u8] = b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n";
    const OPENAI_END: &[u8] = b"data: {\"model\":\"gpt-4o\",\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":4}}\n\ndata: [DONE]\n\n";
    const ANTHROPIC_START: &[u8] = b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m1\",\"model\":\"claude-test\",\"usage\":{\"input_tokens\":125,\"output_tokens\":1}}}\n\n";
    const ANTHROPIC_END: &[u8] = b"event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":15}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";

    const OPENAI: u64 = 0x5555_0000_1000;
    const ANTHROPIC: u64 = 0x5555_0000_2000;

    /// One SSL call of `tid` on connection `ssl`, sent as a single event.
    fn call(tid: u32, ssl: u64, rw: LlmDirection, data: &[u8]) -> LlmEvent {
        let mut buf = [0u8; MAX_SSL_BUF_SIZE];
        buf[..data.len()].copy_from_slice(data);
        LlmEvent {
            metadata: EventMetadata {
                pid: 7,
                _pad: tid,
                ..Default::default()
            },
            rw: rw as u8,
            len: data.len() as u32,
            buf_filled: 1,
            ssl_ptr: ssl,
            buf,
            ..Default::default()
        }
    }

    fn free(tid: u32, ssl: u64) -> LlmEvent {
        LlmEvent {
            closed: 1,
            ..call(tid, ssl, LlmDirection::Unknown, b"")
        }
    }

    fn route_all(events: &[LlmEvent]) -> Vec<Outcome> {
        let mut streams = HashMap::new();
        events
            .iter()
            .map(|e| {
                route_event(
                    &mut streams,
                    e,
                    #[cfg(feature = "k8s")]
                    || None,
                )
            })
            .collect()
    }

    fn recorded(out: &[Outcome]) -> usize {
        out.iter().filter(|o| **o == Outcome::Recorded).count()
    }

    /// Node's event loop or asyncio: two streaming calls on one thread, their reads interleaved.
    #[test]
    fn test_two_connections_on_one_thread_are_both_recorded() {
        use LlmDirection::{Read, Write};
        let out = route_all(&[
            call(1, OPENAI, Write, OPENAI_REQUEST),
            call(1, ANTHROPIC, Write, ANTHROPIC_REQUEST),
            call(1, OPENAI, Read, SSE_HEAD),
            call(1, ANTHROPIC, Read, SSE_HEAD),
            call(1, ANTHROPIC, Read, ANTHROPIC_START),
            call(1, OPENAI, Read, OPENAI_DELTA),
            call(1, ANTHROPIC, Read, ANTHROPIC_END),
            call(1, OPENAI, Read, OPENAI_END),
        ]);
        assert_eq!(recorded(&out), 2, "{out:?}");
    }

    /// One connection, request written by one thread and response read by another.
    #[test]
    fn test_connection_shared_by_two_threads_is_recorded() {
        use LlmDirection::{Read, Write};
        let out = route_all(&[
            call(1, OPENAI, Write, OPENAI_REQUEST),
            call(2, OPENAI, Read, SSE_HEAD),
            call(2, OPENAI, Read, OPENAI_DELTA),
            call(2, OPENAI, Read, OPENAI_END),
        ]);
        assert_eq!(out.last(), Some(&Outcome::Recorded), "{out:?}");
    }

    /// SSL_free ends an aborted stream (unparsed), so a reused SSL* starts a new one.
    #[test]
    fn test_freed_connection_does_not_leak_into_reused_address() {
        use LlmDirection::{Read, Write};
        let out = route_all(&[
            call(1, OPENAI, Write, OPENAI_REQUEST),
            call(1, OPENAI, Read, SSE_HEAD),
            call(1, OPENAI, Read, OPENAI_DELTA),
            free(1, OPENAI),
            call(1, OPENAI, Write, ANTHROPIC_REQUEST),
            call(1, OPENAI, Read, SSE_HEAD),
            call(1, OPENAI, Read, ANTHROPIC_START),
            call(1, OPENAI, Read, ANTHROPIC_END),
        ]);
        assert_eq!(out[3], Outcome::Unparsed, "{out:?}");
        assert_eq!(out.last(), Some(&Outcome::Recorded), "{out:?}");
    }

    #[test]
    fn test_free_removes_the_stream() {
        let mut streams = HashMap::new();
        for e in [
            call(
                1,
                OPENAI,
                LlmDirection::Write,
                b"GET /healthz HTTP/1.1\r\n\r\n",
            ),
            free(1, OPENAI),
        ] {
            route_event(
                &mut streams,
                &e,
                #[cfg(feature = "k8s")]
                || None,
            );
        }
        assert!(streams.is_empty());
    }

    /// Two paths to one file (like two processes' `/proc/<pid>/root/...`) attach once.
    #[test]
    fn test_unique_new_files_dedupes_same_inode() -> std::io::Result<()> {
        let dir = std::env::temp_dir().join(format!("hb-llm-dedupe-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        let lib = dir.join("libssl.so.3");
        let link = dir.join("libssl.so.3.hardlink");
        let other = dir.join("libssl.so.1.1");
        std::fs::write(&lib, b"lib")?;
        let _ = std::fs::remove_file(&link);
        std::fs::hard_link(&lib, &link)?;
        std::fs::write(&other, b"other")?;

        let paths: Vec<String> = [&lib, &link, &other]
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect();

        let new = unique_new_files(&paths, &HashSet::new());
        assert_eq!(new.len(), 2, "same inode attached twice: {:?}", new);
        assert_eq!(new[0].0, paths[0]);
        assert_eq!(new[1].0, paths[2]);

        // Re-discovery for a new process skips files already attached.
        let known: HashSet<FileId> = new.iter().map(|(_, id)| *id).collect();
        assert!(unique_new_files(&paths, &known).is_empty());

        std::fs::remove_dir_all(&dir)
    }

    /// A byte-identical copy is a separate inode with its own uprobes, so it must not merge.
    #[test]
    fn test_unique_new_files_keeps_identical_copy() -> std::io::Result<()> {
        let dir = std::env::temp_dir().join(format!("hb-llm-copy-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        let lib = dir.join("libssl.so.3");
        let copy = dir.join("libssl.so.3.copy");
        std::fs::write(&lib, b"lib")?;
        std::fs::copy(&lib, &copy)?;

        let paths: Vec<String> = [&lib, &copy]
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect();
        assert_eq!(unique_new_files(&paths, &HashSet::new()).len(), 2);

        std::fs::remove_dir_all(&dir)
    }

    #[test]
    fn test_unique_new_files_skips_missing_path() {
        let paths = vec!["/nonexistent/libssl.so.3".to_string()];
        assert!(unique_new_files(&paths, &HashSet::new()).is_empty());
    }
}

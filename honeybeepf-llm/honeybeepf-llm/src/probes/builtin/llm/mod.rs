mod conn_filter;
pub mod http;
pub mod processor;

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
use conn_filter::{ConnFilter, payload};
use honeybeepf_llm_common::{LLM_STAT_COUNT, LLM_STAT_NAMES, LlmEvent};
use honeybeepf_llm_core::types::LlmDirection;
use log::{info, warn};
use processor::{Outcome, StreamProcessor};
use regex::Regex;

use crate::probes::{
    DynamicProbe, IdentityResolver, Probe, ProcessInfo, TracepointConfig, attach_tracepoint,
    attach_uprobe, detach_uprobe,
    discovery::{
        FileId, exported_function_offsets, file_identity, find_executables_all, find_libraries_all,
    },
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

/// (program, function) that ends a connection's stream; optional like the probes above.
const CLOSE_PROBE: (&str, &str) = ("probe_ssl_free", "SSL_free");

/// Clears an exiting thread's session entries; one per node, not per probed file.
const TASK_EXIT_PROBE: TracepointConfig = TracepointConfig {
    program_name: "probe_ssl_task_exit",
    category: "sched",
    name: "sched_process_exit",
};

/// Every function in the probe tables, resolved in one pass over the file's symbols.
const PROBED_FUNCTIONS: [&str; 6] = [
    "SSL_read",
    "SSL_write",
    "SSL_do_handshake",
    "SSL_write_ex",
    "SSL_read_ex",
    "SSL_free",
];

/// Attach the SSL probes to `libssl_path`; `Ok(false)` if it does not export SSL_read and SSL_write.
pub fn attach_probes_to_path(bpf: &mut Ebpf, libssl_path: &str) -> Result<bool> {
    let offsets = exported_function_offsets(libssl_path, &PROBED_FUNCTIONS)?;
    let Some(required) = REQUIRED_PROBES
        .iter()
        .map(|&(enter, exit, func)| offsets.get(func).map(|&off| (enter, exit, func, off)))
        .collect::<Option<Vec<_>>>()
    else {
        return Ok(false);
    };

    // Roll back on failure so a retry on the same file doesn't stack duplicate probes.
    let mut attached = Vec::with_capacity(REQUIRED_PROBES.len() * 2);
    for (enter, exit, func, offset) in required {
        for prog in [enter, exit] {
            match attach_uprobe(bpf, prog, func, libssl_path, offset) {
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
        let Some(&offset) = offsets.get(func) else {
            continue;
        };
        if let Ok(link) = attach_uprobe(bpf, enter, func, libssl_path, offset)
            && attach_uprobe(bpf, exit, func, libssl_path, offset).is_err()
        {
            let _ = detach_uprobe(bpf, enter, link);
        }
    }

    // Without it a connection's stream is only dropped after CONNECTION_RETENTION_SECS idle.
    let (prog, func) = CLOSE_PROBE;
    if let Err(e) = offsets
        .get(func)
        .context("not exported")
        .and_then(|&offset| attach_uprobe(bpf, prog, func, libssl_path, offset))
    {
        warn!("{} not probed in {}: {}", func, libssl_path, e);
    }

    Ok(true)
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
    /// Files already tried (attached, failed or exporting no SSL functions), so each is attempted once.
    known_targets: Arc<Mutex<HashSet<FileId>>>,
    /// Also probe main executables with OpenSSL built in (official Node.js builds).
    executables: bool,
}

impl LlmProbe {
    pub fn new(executables: bool) -> Self {
        Self {
            known_targets: Arc::new(Mutex::new(HashSet::new())),
            executables,
        }
    }

    fn attach_new<'a, I>(&self, bpf: &mut Ebpf, paths: I) -> Vec<(&'a str, FileId, Result<bool>)>
    where
        I: IntoIterator<Item = &'a String> + Clone,
    {
        attach_new_files(&self.known_targets, paths, |path| {
            attach_probes_to_path(bpf, path)
        })
    }
}

/// Rounds of picking another path to a file whose chosen path vanished during the attach.
const ATTACH_ROUNDS: usize = 3;

/// Try each file not tried before, holding the lock only to pick candidates and record them.
fn attach_new_files<'a, I>(
    known: &Mutex<HashSet<FileId>>,
    paths: I,
    mut attach: impl FnMut(&str) -> Result<bool>,
) -> Vec<(&'a str, FileId, Result<bool>)>
where
    I: IntoIterator<Item = &'a String> + Clone,
{
    let mut out = Vec::new();
    for _ in 0..ATTACH_ROUNDS {
        let candidates = {
            let known = known.lock().unwrap_or_else(|e| e.into_inner());
            unique_new_files(paths.clone(), &known)
        };
        let mut vanished = false;
        for (path, id) in candidates {
            let result = attach(path);
            // A /proc/<pid>/root path dies with its process; recording the file would skip it for good.
            if result.is_err() && !std::path::Path::new(path).exists() {
                vanished = true;
                continue;
            }
            known.lock().unwrap_or_else(|e| e.into_inner()).insert(id);
            out.push((path, id, result));
        }
        if !vanished {
            break;
        }
    }
    out
}

/// Most executables export no SSL functions, so only an attach or an error gets a log line.
fn log_executable(path: &str, id: FileId, result: Result<bool>) {
    match result {
        Ok(true) => info!(
            "Attaching LLM (SSL) probes to executable with built-in OpenSSL: {} (ino {})",
            path,
            id.ino()
        ),
        Ok(false) => {}
        Err(e) => warn!("Failed to attach to executable {}: {}", path, e),
    }
}

impl DynamicProbe for LlmProbe {
    fn on_exec(&self, bpf: &mut Ebpf, process_info: &ProcessInfo) -> Result<()> {
        let ssl_libs = process_info.libs.iter().filter(|path| {
            // Match against basename with a strict prefix check
            std::path::Path::new(path)
                .file_name()
                .and_then(|f| f.to_str())
                .map(|f| f.starts_with("libssl.so"))
                .unwrap_or(false)
        });
        for (path, id, result) in self.attach_new(bpf, ssl_libs) {
            match result {
                Ok(true) => info!(
                    "[Re-discovery] New SSL library found: {} (PID: {}, ino {})",
                    path,
                    process_info.pid,
                    id.ino()
                ),
                Ok(false) => warn!(
                    "[Re-discovery] Failed to attach to {}: SSL_read/SSL_write not exported",
                    path
                ),
                Err(e) => warn!("[Re-discovery] Failed to attach to {}: {}", path, e),
            }
        }

        let exe = process_info.exe.iter().filter(|_| self.executables);
        for (path, id, result) in self.attach_new(bpf, exe) {
            log_executable(path, id, result);
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
    let data = payload(event);
    if data.is_empty() {
        return Outcome::Pending;
    }
    streams.entry(key).or_default().handle_fragment(
        LlmDirection::from(event.rw),
        data,
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
        http::report_provider_config();
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

        // Without it, every thread killed inside an SSL call keeps its session map entries until restart.
        if let Err(e) = attach_tracepoint(bpf, TASK_EXIT_PROBE) {
            warn!("{:#}; session entries of killed threads are not cleared", e);
        }

        // Skip libcrypto for SSL_* probes as they usually don't contain them
        let ssl_libs = targets.iter().filter(|path| !path.contains("libcrypto"));
        for (path, id, result) in self.attach_new(bpf, ssl_libs) {
            match result {
                Ok(true) => info!(
                    "Attaching LLM (SSL) probes to detected library: {} (ino {})",
                    path,
                    id.ino()
                ),
                Ok(false) => warn!(
                    "Failed to attach to {}: SSL_read/SSL_write not exported",
                    path
                ),
                Err(e) => warn!("Failed to attach to {}: {}", path, e),
            }
        }

        if self.executables {
            let exes = find_executables_all();
            for (path, id, result) in self.attach_new(bpf, &exes) {
                log_executable(path, id, result);
            }
        }

        let state: StreamMap = Arc::new(Mutex::new(HashMap::new()));
        let handler_state = state.clone();
        let mut conn_filter = ConnFilter::new(bpf)?;

        spawn_ringbuf_handler(bpf, "SSL_EVENTS", move |event: LlmEvent| {
            if event.is_handshake == 1 || conn_filter.skip_if_foreign(&event, &handler_state) {
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
    use conn_filter::opens_foreign_request;
    use honeybeepf_llm_common::{EventMetadata, MAX_SSL_BUF_SIZE};
    use processor::UnparsedReason;

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
        assert_eq!(
            out[3],
            Outcome::Unparsed(UnparsedReason::ConnectionClosed),
            "{out:?}"
        );
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

    #[test]
    fn test_only_a_request_head_to_a_non_provider_host_skips_the_connection() {
        use LlmDirection::{Read, Write};
        let noise =
            b"POST /api/data HTTP/1.1\r\nHost: noise.internal\r\nContent-Length: 2\r\n\r\nxx";
        assert!(opens_foreign_request(&call(1, OPENAI, Write, noise)));
        let continued = LlmEvent {
            offset: MAX_SSL_BUF_SIZE as u32,
            ..call(1, OPENAI, Write, noise)
        };
        for e in [
            continued,
            call(1, OPENAI, Write, OPENAI_REQUEST),
            call(
                1,
                OPENAI,
                Write,
                b"GET /v1/models HTTP/1.1\r\nHost: api.openai.com\r\n\r\n",
            ),
            call(
                1,
                OPENAI,
                Write,
                b"POST /api/data HTTP/1.1\r\nHost: noise.int",
            ),
            call(1, OPENAI, Write, b"POST /api/data HTTP/1.1\r\n\r\n"),
            call(1, OPENAI, Write, b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n"),
            call(
                1,
                OPENAI,
                Read,
                b"HTTP/1.1 200 OK\r\nHost: noise.internal\r\n\r\n",
            ),
        ] {
            assert!(!opens_foreign_request(&e), "{:?}", payload(&e));
        }
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

    /// When the chosen path's process exits mid-attach, another path to the same file is tried.
    #[test]
    fn test_attach_new_files_retries_vanished_path() -> std::io::Result<()> {
        let dir = std::env::temp_dir().join(format!("hb-llm-vanish-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        let first = dir.join("a-node");
        let second = dir.join("b-node");
        std::fs::write(&first, b"elf")?;
        let _ = std::fs::remove_file(&second);
        std::fs::hard_link(&first, &second)?;
        let paths: Vec<String> = [&first, &second]
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect();

        let known = Mutex::new(HashSet::new());
        let mut tried = Vec::new();
        let out = attach_new_files(&known, &paths, |path| {
            tried.push(path.to_string());
            if path == paths[0] {
                std::fs::remove_file(path)?;
                anyhow::bail!("No such file or directory");
            }
            Ok(true)
        });

        assert_eq!(tried, paths);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, paths[1]);
        assert!(matches!(out[0].2, Ok(true)));
        assert_eq!(known.lock().map(|k| k.len()).unwrap_or(0), 1);

        // A failure on a path that still exists is recorded, so the file is not retried.
        let known = Mutex::new(HashSet::new());
        let out = attach_new_files(&known, &paths[1..], |_| anyhow::bail!("verifier"));
        assert_eq!(out.len(), 1);
        assert_eq!(known.lock().map(|k| k.len()).unwrap_or(0), 1);

        std::fs::remove_dir_all(&dir)
    }

    /// Each uprobe program in the eBPF object is tabled, and its function is in PROBED_FUNCTIONS.
    #[test]
    fn test_probe_tables_cover_every_uprobe_program() -> Result<()> {
        use object::{Object, ObjectSection, ObjectSymbol, SymbolKind};

        let mut tabled: HashMap<&str, &str> = HashMap::from([CLOSE_PROBE]);
        for &(enter, exit, func) in REQUIRED_PROBES.iter().chain(&OPTIONAL_PROBES) {
            tabled.insert(enter, func);
            tabled.insert(exit, func);
        }
        for func in tabled.values() {
            assert!(PROBED_FUNCTIONS.contains(func), "{func} is never resolved");
        }

        let data = aya::include_bytes_aligned!(concat!(env!("OUT_DIR"), "/honeybeepf-llm"));
        let obj = object::File::parse(data)?;
        let mut programs = HashSet::new();
        for sym in obj.symbols().filter(|s| s.kind() == SymbolKind::Text) {
            let Some(index) = sym.section_index() else {
                continue;
            };
            let section = obj.section_by_index(index)?.name()?;
            if section.starts_with("uprobe") || section.starts_with("uretprobe") {
                programs.insert(sym.name()?);
            }
        }
        let tabled: HashSet<&str> = tabled.into_keys().collect();
        assert_eq!(programs, tabled, "uprobe programs vs probe tables");
        Ok(())
    }

    /// Each uprobe program tail-calls into NO_PRIV_STACK, so a 6.13+ verifier gives it no per-CPU private stack.
    #[test]
    fn test_every_uprobe_program_has_a_tail_call() -> Result<()> {
        use object::{Object, ObjectSection, ObjectSymbol, RelocationTarget, SymbolKind};
        const BPF_CALL: u8 = 0x85;
        const BPF_FUNC_TAIL_CALL: i32 = 12;

        let data = aya::include_bytes_aligned!(concat!(env!("OUT_DIR"), "/honeybeepf-llm"));
        let obj = object::File::parse(data)?;
        let (mut checked, mut without) = (0, Vec::new());
        for sym in obj.symbols().filter(|s| s.kind() == SymbolKind::Text) {
            let Some(index) = sym.section_index() else {
                continue;
            };
            let section = obj.section_by_index(index)?;
            let name = section.name()?;
            if !name.starts_with("uprobe") && !name.starts_with("uretprobe") {
                continue;
            }
            let start = usize::try_from(sym.address())?;
            let end = start + usize::try_from(sym.size())?;
            let code = section.data()?.get(start..end).unwrap_or_default();
            // A helper call has src_reg 0; a bpf-to-bpf call has src_reg 1.
            let (insns, _) = code.as_chunks::<8>();
            let tail_call = insns.iter().any(|insn| {
                insn[0] == BPF_CALL
                    && insn[1] >> 4 == 0
                    && i32::from_le_bytes([insn[4], insn[5], insn[6], insn[7]])
                        == BPF_FUNC_TAIL_CALL
            });
            let uses_map = section.relocations().any(|(at, rel)| {
                (start as u64..end as u64).contains(&at)
                    && matches!(rel.target(), RelocationTarget::Symbol(i)
                        if obj.symbol_by_index(i).and_then(|s| s.name()).is_ok_and(|n| n == "NO_PRIV_STACK"))
            });
            checked += 1;
            if !tail_call || !uses_map {
                without.push(sym.name()?);
            }
        }
        assert!(checked > 0, "no uprobe program found in the eBPF object");
        // The loader sets this global by name and does not fail when it is missing.
        assert!(
            obj.symbols().any(|s| s.name() == Ok("TAIL_CALL_OK")),
            "TAIL_CALL_OK not in the eBPF object"
        );
        assert!(
            without.is_empty(),
            "uprobe programs without a tail call into NO_PRIV_STACK: {without:?}"
        );
        Ok(())
    }

    /// The task exit program is in the eBPF object as a tracepoint program.
    #[test]
    fn test_task_exit_probe_is_a_tracepoint_program() -> Result<()> {
        use object::{Object, ObjectSection, ObjectSymbol};

        let data = aya::include_bytes_aligned!(concat!(env!("OUT_DIR"), "/honeybeepf-llm"));
        let obj = object::File::parse(data)?;
        let sym = obj
            .symbols()
            .find(|s| s.name() == Ok(TASK_EXIT_PROBE.program_name))
            .context("task exit program not in the eBPF object")?;
        let index = sym.section_index().context("symbol without a section")?;
        let section = obj.section_by_index(index)?.name()?;
        assert!(section.starts_with("tracepoint"), "section {section}");
        Ok(())
    }

    #[test]
    fn test_unique_new_files_skips_missing_path() {
        let paths = vec!["/nonexistent/libssl.so.3".to_string()];
        assert!(unique_new_files(&paths, &HashSet::new()).is_empty());
    }
}

mod discovery;
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
use honeybeepf_llm_common::{LLM_STAT_COUNT, LLM_STAT_NAMES, LlmEvent};
use honeybeepf_llm_core::types::LlmDirection;
use log::{info, warn};
use processor::StreamProcessor;

use crate::probes::{
    DynamicProbe, IdentityResolver, Probe, ProcessInfo, attach_uprobe, detach_uprobe,
    discovery::{FileId, exported_function_offsets, file_identity, find_executables_all},
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

/// Every function in the probe tables, resolved in one pass over the file's symbols.
const PROBED_FUNCTIONS: [&str; 5] = [
    "SSL_read",
    "SSL_write",
    "SSL_do_handshake",
    "SSL_write_ex",
    "SSL_read_ex",
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

// Shared state
type StreamMap = Arc<Mutex<HashMap<(u32, u32), StreamProcessor>>>;

impl Probe for LlmProbe {
    fn attach(&self, bpf: &mut Ebpf, _resolver: IdentityResolver) -> Result<()> {
        let targets = discovery::find_all_targets()?;

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

        spawn_ringbuf_handler(bpf, "SSL_EVENTS", move |event: LlmEvent| {
            let direction = LlmDirection::from(event.rw);
            if event.is_handshake == 1 {
                return;
            }
            let data_len = std::cmp::min(
                event.len.saturating_sub(event.offset) as usize,
                honeybeepf_llm_common::MAX_SSL_BUF_SIZE,
            );
            if event.buf_filled == 0 || data_len == 0 {
                return;
            }

            // Resolve pod identity (result is used for future telemetry enrichment)
            #[cfg(feature = "k8s")]
            let pod_info = _resolver.resolve_pod(event.metadata.pid, event.metadata.cgroup_id);

            let key = (event.metadata.pid, event.metadata._pad);
            let mut map = handler_state.lock().unwrap_or_else(|e| e.into_inner());
            let processor = map.entry(key).or_default();

            processor.handle_fragment(
                direction,
                &event.buf[..data_len],
                event.offset,
                event.len,
                event.truncated != 0,
                event.metadata.pid,
                #[cfg(feature = "k8s")]
                pod_info,
            );
        })?;

        start_cleanup_task(state);
        start_stats_log_task();

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

/// Log the kernel event counters every STATS_LOG_INTERVAL_SECS when they changed.
fn start_stats_log_task() {
    let shutdown = crate::probes::shutdown_flag();
    std::thread::spawn(move || {
        let mut last = None;
        while !shutdown.load(std::sync::atomic::Ordering::Relaxed) {
            std::thread::sleep(Duration::from_secs(STATS_LOG_INTERVAL_SECS));
            let current = event_stats();
            if current != last {
                log_event_stats();
                last = current;
            }
        }
    });
}

/// Find all SSL targets. Convenience wrapper around `discovery::find_all_targets`.
pub fn find_all_targets() -> Result<HashSet<String>> {
    discovery::find_all_targets()
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// Offsets come from one symbol pass, so it must cover every probed function.
    #[test]
    fn test_probed_functions_cover_probe_tables() {
        for (_, _, func) in REQUIRED_PROBES.iter().chain(&OPTIONAL_PROBES) {
            assert!(PROBED_FUNCTIONS.contains(func), "{func} is never resolved");
        }
    }

    #[test]
    fn test_unique_new_files_skips_missing_path() {
        let paths = vec!["/nonexistent/libssl.so.3".to_string()];
        assert!(unique_new_files(&paths, &HashSet::new()).is_empty());
    }
}

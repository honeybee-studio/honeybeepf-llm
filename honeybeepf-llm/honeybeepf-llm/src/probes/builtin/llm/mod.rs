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
    discovery::{FileId, file_identity},
    spawn_ringbuf_handler,
};
// Re-export exec watch types and functions for lib.rs
pub use crate::probes::{ExecNotify, ExecPidQueue, setup_exec_watch};

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
                path, process_info.pid, id.ino
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

// Shared state
type StreamMap = Arc<Mutex<HashMap<(u32, u32), StreamProcessor>>>;

impl Probe for LlmProbe {
    fn attach(&self, bpf: &mut Ebpf, _resolver: IdentityResolver) -> Result<()> {
        let targets = discovery::find_all_targets()?;

        if targets.is_empty() {
            warn!("No targets found. LLM probing disabled.");
            return Ok(());
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
                path, id.ino
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
            let direction = LlmDirection::from(event.rw);
            if event.is_handshake == 1 {
                return;
            }
            if event.buf_filled == 0 || event.len == 0 {
                return;
            }

            // Resolve pod identity (result is used for future telemetry enrichment)
            #[cfg(feature = "k8s")]
            let pod_info = _resolver.resolve_pod(event.metadata.pid, event.metadata.cgroup_id);

            let key = (event.metadata.pid, event.metadata._pad);
            let mut map = handler_state.lock().unwrap_or_else(|e| e.into_inner());
            let processor = map.entry(key).or_default();

            let data_len =
                std::cmp::min(event.len as usize, honeybeepf_llm_common::MAX_SSL_BUF_SIZE);

            #[cfg(feature = "k8s")]
            processor.handle_event(
                direction,
                &event.buf[..data_len],
                event.metadata.pid,
                pod_info,
            );

            #[cfg(not(feature = "k8s"))]
            processor.handle_event(direction, &event.buf[..data_len], event.metadata.pid);
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

    #[test]
    fn test_unique_new_files_skips_missing_path() {
        let paths = vec!["/nonexistent/libssl.so.3".to_string()];
        assert!(unique_new_files(&paths, &HashSet::new()).is_empty());
    }
}

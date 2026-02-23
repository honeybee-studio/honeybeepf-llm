pub mod discovery;
pub mod http;
pub mod processor;
pub mod types;

use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result};
use aya::{
    Ebpf,
    programs::{TracePoint, UProbe},
};
use honeybeepf_llm_common::{ExecEvent, LlmEvent};
use log::{info, warn};
use processor::StreamProcessor;
use tokio::sync::Notify;
use types::LlmDirection;

use crate::probes::{IdentityResolver, Probe, spawn_ringbuf_handler};

// Queue and timing constants
const MAX_EXEC_QUEUE_SIZE: usize = 1024; // Max pending exec PIDs
const CLEANUP_INTERVAL_SECS: u64 = 30; // How often to run cleanup
const CONNECTION_RETENTION_SECS: u64 = 300; // Keep idle connections for 5 minutes

pub fn attach_probes_to_path(bpf: &mut Ebpf, libssl_path: &str) -> Result<()> {
    // SSL_read/SSL_write need BOTH entry (to save buf ptr) and exit (to read data + emit event)
    attach_uprobe(bpf, "probe_ssl_rw_enter", "SSL_read", libssl_path)?;
    attach_uprobe(bpf, "probe_ssl_read_exit", "SSL_read", libssl_path)?;
    attach_uprobe(bpf, "probe_ssl_rw_enter", "SSL_write", libssl_path)?;
    attach_uprobe(bpf, "probe_ssl_write_exit", "SSL_write", libssl_path)?;

    // Handshake
    attach_uprobe(
        bpf,
        "probe_ssl_do_handshake_enter",
        "SSL_do_handshake",
        libssl_path,
    )?;
    attach_uprobe(
        bpf,
        "probe_ssl_do_handshake_exit",
        "SSL_do_handshake",
        libssl_path,
    )?;

    // Extended variants (optional — not all OpenSSL builds export these)
    let _ = attach_uprobe(bpf, "probe_ssl_rw_ex_enter", "SSL_write_ex", libssl_path);
    let _ = attach_uprobe(bpf, "probe_ssl_write_ex_exit", "SSL_write_ex", libssl_path);
    let _ = attach_uprobe(bpf, "probe_ssl_rw_ex_enter", "SSL_read_ex", libssl_path);
    let _ = attach_uprobe(bpf, "probe_ssl_read_ex_exit", "SSL_read_ex", libssl_path);

    Ok(())
}

/// Scan only specific PIDs for new SSL libraries and attach probes to any not yet in `known`.
pub fn attach_new_targets_for_pids(
    bpf: &mut Ebpf,
    known: &mut HashSet<String>,
    pids: &[u32],
) -> Result<()> {
    let targets = discovery::find_targets_for_pids(pids)?;

    for path in targets {
        if path.contains("libcrypto") {
            continue;
        }
        if known.contains(&path) {
            continue;
        }

        info!("[Re-discovery] New SSL library found: {}", path);
        match attach_probes_to_path(bpf, &path) {
            Ok(()) => {
                known.insert(path);
            }
            Err(e) => {
                warn!("[Re-discovery] Failed to attach to {}: {}", path, e);
            }
        }
    }

    Ok(())
}

/// Shared queue of PIDs from exec events.
pub type ExecPidQueue = Arc<Mutex<VecDeque<u32>>>;

/// Notifier to wake up the main loop immediately when new exec events arrive.
pub type ExecNotify = Arc<Notify>;

/// Set up the `sched_process_exec` tracepoint and return a queue that collects
/// PIDs of newly exec'd processes. The caller drains this queue to do targeted scans.
/// Also returns a Notify that gets triggered on each new exec event.
pub fn setup_exec_watch(bpf: &mut Ebpf) -> Result<(ExecPidQueue, ExecNotify)> {
    let program: &mut TracePoint = bpf
        .program_mut("probe_exec")
        .context("Failed to find probe_exec program")?
        .try_into()?;
    program.load()?;
    program.attach("sched", "sched_process_exec")?;

    let queue: ExecPidQueue = Arc::new(Mutex::new(VecDeque::new()));
    let notify: ExecNotify = Arc::new(Notify::new());
    let handler_queue = queue.clone();
    let handler_notify = notify.clone();

    spawn_ringbuf_handler(bpf, "EXEC_EVENTS", move |event: ExecEvent| {
        let mut q = handler_queue.lock().unwrap_or_else(|e| e.into_inner());
        // Cap queue to avoid unbounded growth under extreme exec rates
        if q.len() < MAX_EXEC_QUEUE_SIZE {
            q.push_back(event.pid);
        }
        // Notify the main loop immediately
        handler_notify.notify_one();
    })?;

    info!("Exec watch active: will trigger targeted SSL re-discovery on new processes");
    Ok((queue, notify))
}

pub struct LlmProbe;

// Shared state
type StreamMap = Arc<Mutex<HashMap<(u32, u32), StreamProcessor>>>;

impl Probe for LlmProbe {
    fn attach(&self, bpf: &mut Ebpf, _resolver: IdentityResolver) -> Result<()> {
        let targets = discovery::find_all_targets()?;

        if targets.is_empty() {
            warn!("No targets found. LLM probing disabled.");
            return Ok(());
        }

        for path in &targets {
            // Skip libcrypto for SSL_* probes as they usually don't contain them
            if path.contains("libcrypto") {
                info!("Skipping SSL probes for libcrypto: {}", path);
                continue;
            }

            info!("Attaching LLM (SSL) probes to detected library: {}", path);
            if let Err(e) = attach_probes_to_path(bpf, path) {
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

fn attach_uprobe(bpf: &mut Ebpf, prog_name: &str, func_name: &str, path: &str) -> Result<()> {
    let program: &mut UProbe = bpf
        .program_mut(prog_name)
        .with_context(|| format!("Failed to find program {}", prog_name))?
        .try_into()?;

    // Check if loaded using fd()
    if program.fd().is_err() {
        program.load()?;
    }

    program
        .attach(Some(func_name), 0, path, None)
        .with_context(|| format!("Failed to attach {} to {}", prog_name, func_name))?;

    Ok(())
}

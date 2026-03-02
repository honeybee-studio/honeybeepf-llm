use std::{
    collections::VecDeque,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result};
use aya::{
    Ebpf,
    maps::RingBuf,
    programs::{TracePoint, UProbe},
};
use honeybeepf_common::ExecEvent;
use log::{info, warn};
use tokio::sync::Notify;

static SHUTDOWN: once_cell::sync::Lazy<Arc<AtomicBool>> =
    once_cell::sync::Lazy::new(|| Arc::new(AtomicBool::new(false)));

pub fn shutdown_flag() -> Arc<AtomicBool> {
    SHUTDOWN.clone()
}

pub fn request_shutdown() {
    SHUTDOWN.store(true, Ordering::Relaxed);
}

pub mod builtin;
pub mod custom;
pub mod discovery;

pub trait Probe {
    fn attach(&self, bpf: &mut Ebpf) -> Result<()>;
}

/// Shared information about a process to avoid redundant I/O in discovery.
pub struct ProcessInfo {
    pub pid: u32,
    pub libs: std::collections::HashSet<String>,
}

/// A probe that needs to react to new process execution events (e.g. uprobes on lazy-loaded libs).
pub trait DynamicProbe: Probe {
    /// Called when a new process is executed, providing shared process info.
    fn on_exec(&self, bpf: &mut Ebpf, process_info: &ProcessInfo) -> Result<()>;
}

pub struct TracepointConfig<'a> {
    pub program_name: &'a str,
    pub category: &'a str,
    pub name: &'a str,
}

pub const POLL_INTERVAL_MS: u64 = 10;
const MAX_EXEC_QUEUE_SIZE: usize = 1024;

/// Shared queue of PIDs from exec events.
pub type ExecPidQueue = Arc<Mutex<VecDeque<u32>>>;

/// Notifier to wake up the main loop immediately when new exec events arrive.
pub type ExecNotify = Arc<Notify>;

fn tracepoint_exists(category: &str, name: &str) -> bool {
    const TRACEFS_MOUNT_POINTS: [&str; 2] = ["/sys/kernel/tracing", "/sys/kernel/debug/tracing"];

    TRACEFS_MOUNT_POINTS.iter().any(|base| {
        Path::new(base)
            .join("events")
            .join(category)
            .join(name)
            .exists()
    })
}

pub fn attach_tracepoint(bpf: &mut Ebpf, config: TracepointConfig) -> Result<bool> {
    if !tracepoint_exists(config.category, config.name) {
        warn!(
            "Tracepoint {}:{} not available; skipping {}",
            config.category, config.name, config.program_name
        );
        return Ok(false);
    }

    info!("Loading program {}", config.program_name);
    let program: &mut TracePoint = bpf
        .program_mut(config.program_name)
        .with_context(|| format!("Failed to find {} program", config.program_name))?
        .try_into()?;
    program.load()?;
    program
        .attach(config.category, config.name)
        .with_context(|| format!("Failed to attach {}", config.name))?;
    Ok(true)
}

pub fn spawn_ringbuf_handler<T, F>(bpf: &mut Ebpf, map_name: &str, handler: F) -> Result<()>
where
    T: Copy + Send + 'static,
    F: Fn(T) + Send + 'static,
{
    let mut ring_buf = RingBuf::try_from(bpf.take_map(map_name).context("Failed to get map")?)?;
    let shutdown = shutdown_flag();

    tokio::task::spawn_blocking(move || {
        while !shutdown.load(Ordering::Relaxed) {
            let mut has_work = false;
            while let Some(item) = ring_buf.next() {
                has_work = true;
                if item.len() >= std::mem::size_of::<T>() {
                    let event = unsafe { (item.as_ptr() as *const T).read_unaligned() };
                    handler(event);
                }
            }
            if !has_work {
                std::thread::sleep(Duration::from_millis(POLL_INTERVAL_MS));
            }
        }
    });
    Ok(())
}

/// Attach a uprobe to a function in a shared library.
/// Loads the program if not already loaded.
pub fn attach_uprobe(bpf: &mut Ebpf, prog_name: &str, func_name: &str, path: &str) -> Result<()> {
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

/// Set up the `sched_process_exec` tracepoint and return a queue that collects
/// PIDs of newly exec'd processes. The caller drains this queue to do targeted scans.
/// Also returns a Notify that gets triggered on each new exec event.
pub fn setup_exec_watch(bpf: &mut Ebpf) -> Result<(ExecPidQueue, ExecNotify)> {
    let program: &mut TracePoint = bpf
        .program_mut("probe_exec")
        .context("Failed to find probe_exec program")?
        .try_into()?;
    if program.fd().is_err() {
        program.load()?;
    }
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

    info!("Exec watch active: will trigger targeted re-discovery on new processes");
    Ok((queue, notify))
}

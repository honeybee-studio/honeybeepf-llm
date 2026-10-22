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
    Btf, Ebpf,
    maps::{HashMap as AyaHashMap, MapData, RingBuf},
    programs::{FEntry, KProbe, TracePoint, UProbe, uprobe::UProbeLinkId},
};
use honeybeepf_llm_common::{ExecEvent, LibOpenEvent};
use log::{info, warn};
use tokio::sync::Notify;

#[cfg(feature = "k8s")]
use crate::k8s::{PodInfo, PodResolver};

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

/// Process identity resolver. Wraps optional K8s PodResolver.
///
/// When the `k8s` feature is enabled and a resolver is provided,
/// resolves PIDs to Kubernetes pod metadata. Otherwise, returns None.
#[derive(Clone)]
pub struct IdentityResolver {
    #[cfg(feature = "k8s")]
    inner: Option<Arc<PodResolver>>,
}

impl IdentityResolver {
    /// Create a resolver with no K8s integration.
    pub fn none() -> Self {
        Self {
            #[cfg(feature = "k8s")]
            inner: None,
        }
    }

    /// Create a resolver backed by a K8s PodResolver.
    #[cfg(feature = "k8s")]
    pub fn with_pod_resolver(resolver: Arc<PodResolver>) -> Self {
        Self {
            inner: Some(resolver),
        }
    }

    /// Resolve a PID + cgroup_id to pod metadata.
    ///
    /// Returns None if K8s feature is disabled, no resolver is configured,
    /// or the PID doesn't belong to a known pod.
    #[cfg(feature = "k8s")]
    pub fn resolve_pod(&self, pid: u32, cgroup_id: u64) -> Option<Arc<PodInfo>> {
        self.inner.as_ref()?.resolve(pid, cgroup_id)
    }

    #[cfg(not(feature = "k8s"))]
    pub fn resolve_pod(&self, _pid: u32, _cgroup_id: u64) -> Option<()> {
        None
    }
}

pub trait Probe {
    fn attach(&self, bpf: &mut Ebpf, resolver: IdentityResolver) -> Result<()>;
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

pub fn spawn_ringbuf_handler<T, F>(bpf: &mut Ebpf, map_name: &str, mut handler: F) -> Result<()>
where
    T: Copy + Send + 'static,
    F: FnMut(T) + Send + 'static,
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
pub fn attach_uprobe(
    bpf: &mut Ebpf,
    prog_name: &str,
    func_name: &str,
    path: &str,
) -> Result<UProbeLinkId> {
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
        .with_context(|| format!("Failed to attach {} to {}", prog_name, func_name))
}

/// Load (if needed) and attach a kprobe program to a kernel function.
pub fn attach_kprobe(bpf: &mut Ebpf, prog_name: &str, func_name: &str) -> Result<()> {
    let program: &mut KProbe = bpf
        .program_mut(prog_name)
        .with_context(|| format!("Failed to find program {}", prog_name))?
        .try_into()?;
    if program.fd().is_err() {
        program.load()?;
    }
    program
        .attach(func_name, 0)
        .with_context(|| format!("Failed to attach {} to {}", prog_name, func_name))?;
    Ok(())
}

/// Load and attach an fentry program; needs kernel BTF (and 6.0+ on arm64).
pub fn attach_fentry(bpf: &mut Ebpf, prog_name: &str, func_name: &str) -> Result<()> {
    let btf = Btf::from_sys_fs().context("kernel BTF not available")?;
    let program: &mut FEntry = bpf
        .program_mut(prog_name)
        .with_context(|| format!("Failed to find program {}", prog_name))?
        .try_into()?;
    program.load(func_name, &btf)?;
    program
        .attach()
        .with_context(|| format!("Failed to attach {} to {}", prog_name, func_name))?;
    Ok(())
}

/// Detach a uprobe link returned by [`attach_uprobe`].
pub fn detach_uprobe(bpf: &mut Ebpf, prog_name: &str, link_id: UProbeLinkId) -> Result<()> {
    let program: &mut UProbe = bpf
        .program_mut(prog_name)
        .with_context(|| format!("Failed to find program {}", prog_name))?
        .try_into()?;
    program
        .detach(link_id)
        .with_context(|| format!("Failed to detach {}", prog_name))
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

/// A `libssl.so*` open reported by the kernel.
pub struct LibOpen {
    pub pid: u32,
    /// As the process passed it to open(), relative to its own root.
    pub path: String,
    /// Written back to `LIB_OPEN_SEEN` once handled, so the kernel stops reporting it.
    pub seen_key: [u64; 2],
    pub ts: u64,
}

pub type LibOpenQueue = Arc<Mutex<VecDeque<LibOpen>>>;
pub type LibOpenSeen = AyaHashMap<MapData, [u64; 2], u64>;

/// Set up the do_sys_openat2 hook (fentry, else kprobe) that reports `libssl.so*` opens before their first SSL call.
/// If it fails to attach, only this hook is lost and exec re-discovery still runs.
pub fn setup_lib_open_watch(bpf: &mut Ebpf) -> Result<(LibOpenQueue, ExecNotify, LibOpenSeen)> {
    // fentry runs on every open without the kprobe trap; the kprobe covers kernels without BTF.
    match attach_fentry(bpf, "probe_lib_open_fentry", "do_sys_openat2") {
        Ok(()) => info!("libssl open watch: fentry on do_sys_openat2"),
        Err(fentry_err) => match attach_kprobe(bpf, "probe_lib_open", "do_sys_openat2") {
            Ok(()) => info!("libssl open watch: kprobe (fentry: {:#})", fentry_err),
            Err(e) => warn!(
                "libssl open watch not attached: {:#} (fentry: {:#})",
                e, fentry_err
            ),
        },
    }
    let seen = AyaHashMap::try_from(
        bpf.take_map("LIB_OPEN_SEEN")
            .context("Failed to get LIB_OPEN_SEEN map")?,
    )?;

    let queue: LibOpenQueue = Arc::new(Mutex::new(VecDeque::new()));
    let notify: ExecNotify = Arc::new(Notify::new());
    let handler_queue = queue.clone();
    let handler_notify = notify.clone();
    let own_pid = std::process::id();

    spawn_ringbuf_handler(bpf, "LIB_OPEN_EVENTS", move |event: LibOpenEvent| {
        // aya opens the library file itself while attaching.
        if event.pid == own_pid {
            return;
        }
        let len = (event.len as usize).min(event.path.len());
        let open = LibOpen {
            pid: event.pid,
            path: String::from_utf8_lossy(&event.path[..len]).into_owned(),
            seen_key: event.seen_key,
            ts: event.ts,
        };
        let mut q = handler_queue.lock().unwrap_or_else(|e| e.into_inner());
        if q.len() < MAX_EXEC_QUEUE_SIZE {
            q.push_back(open);
        } else {
            warn!(
                "libssl open queue full, dropping {} (PID: {})",
                open.path, open.pid
            );
        }
        handler_notify.notify_one();
    })?;

    Ok((queue, notify, seen))
}

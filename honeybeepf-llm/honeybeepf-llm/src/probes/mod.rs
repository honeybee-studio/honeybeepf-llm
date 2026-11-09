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
use honeybeepf_llm_common::{ExecEvent, LibOpenEvent, LlmEventHeader};
use std::os::unix::io::AsRawFd;

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
    /// Host path of the main executable; only exec events carry it.
    pub exe: Option<String>,
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
/// How long (ms) a ring-buffer reader blocks in poll() before re-checking the shutdown flag.
const POLL_WAIT_MS: libc::c_int = 1000;
const MAX_EXEC_QUEUE_SIZE: usize = 1024;

/// Shared queue of PIDs from exec events.
pub type ExecPidQueue = Arc<Mutex<VecDeque<u32>>>;

/// Notifier to wake up the main loop immediately when new exec events arrive.
pub type ExecNotify = Arc<Notify>;

pub(crate) fn tracepoint_exists(category: &str, name: &str) -> bool {
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
    T: aya::Pod,
    F: FnMut(&T) + Send + 'static,
{
    let ring_buf = RingBuf::try_from(bpf.take_map(map_name).context("Failed to get map")?)?;
    spawn_ring_loop(ring_buf, move |rb| drain_ring(rb, &mut handler));
    Ok(())
}

/// Like `spawn_ringbuf_handler`, for the variable-length SSL ring: each record is a fixed
/// `LlmEventHeader` then the payload bytes it holds, handed to `handler` borrowed in place.
pub fn spawn_ringbuf_handler_var<F>(bpf: &mut Ebpf, map_name: &str, mut handler: F) -> Result<()>
where
    F: FnMut(&LlmEventHeader, &[u8]) + Send + 'static,
{
    let ring_buf = RingBuf::try_from(bpf.take_map(map_name).context("Failed to get map")?)?;
    spawn_ring_loop(ring_buf, move |rb| drain_var(rb, &mut handler));
    Ok(())
}

/// Drive a ring reader on its own blocking thread, not a tokio worker. Under load: drain, then sleep
/// 10 ms so it wakes per batch, not per event. Idle: block on the ring fd with poll(), so no CPU.
fn spawn_ring_loop(
    mut ring_buf: RingBuf<MapData>,
    mut drain: impl FnMut(&mut RingBuf<MapData>) -> usize + Send + 'static,
) {
    let shutdown = shutdown_flag();
    let fd = ring_buf.as_raw_fd();
    tokio::task::spawn_blocking(move || {
        while !shutdown.load(Ordering::Relaxed) {
            if drain(&mut ring_buf) > 0 {
                std::thread::sleep(Duration::from_millis(POLL_INTERVAL_MS));
            } else {
                let mut pfd = libc::pollfd {
                    fd,
                    events: libc::POLLIN,
                    revents: 0,
                };
                // SAFETY: one valid pollfd for the duration of the call; poll reads fd/events, writes revents.
                // The timeout bounds how long an idle reader waits before re-checking the shutdown flag.
                unsafe {
                    libc::poll(&mut pfd, 1, POLL_WAIT_MS);
                }
            }
        }
    });
}

/// Read every fixed-size record waiting in the ring in place, handing each to `handler` without
/// copying it out; returns how many were drained (0 means the ring was empty).
fn drain_ring<T: aya::Pod>(ring_buf: &mut RingBuf<MapData>, handler: &mut impl FnMut(&T)) -> usize {
    let mut drained = 0;
    while let Some(item) = ring_buf.next() {
        drained += 1;
        if item.len() >= std::mem::size_of::<T>() {
            // SAFETY: BPF ring-buffer records are 8-byte aligned and `T: aya::Pod` (repr(C), every byte
            // pattern valid); the length is checked, so the record is reborrowed in place.
            let event: &T = unsafe { &*(item.as_ptr() as *const T) };
            handler(event);
        }
    }
    drained
}

/// Drain SSL records, handing each as (header, payload) borrowed in place; returns how many were
/// drained. A record too short for its header, or shorter than the payload the header claims, is dropped.
fn drain_var(
    ring_buf: &mut RingBuf<MapData>,
    handler: &mut impl FnMut(&LlmEventHeader, &[u8]),
) -> usize {
    let mut drained = 0;
    while let Some(item) = ring_buf.next() {
        drained += 1;
        if let Some((header, payload)) = parse_llm_record(&item) {
            handler(header, payload);
        }
    }
    drained
}

/// Split one SSL ring record into its header and payload slice, checking the record is long enough
/// for both; `None` drops a malformed record.
fn parse_llm_record(item: &[u8]) -> Option<(&LlmEventHeader, &[u8])> {
    const H: usize = std::mem::size_of::<LlmEventHeader>();
    if item.len() < H {
        return None;
    }
    // SAFETY: BPF ring-buffer records are 8-byte aligned, which meets `align_of::<LlmEventHeader>()`
    // (its widest field is u64); `LlmEventHeader: aya::Pod` (repr(C), every byte pattern valid) and the
    // length is checked, so the header is reborrowed in place.
    #[allow(clippy::cast_ptr_alignment)] // record alignment (8) covers the header; verified above
    let header: &LlmEventHeader = unsafe { &*(item.as_ptr() as *const LlmEventHeader) };
    let end = H + header.payload_len();
    item.get(H..end).map(|payload| (header, payload))
}

/// Attach a uprobe at a file offset in a library or executable.
/// Loads the program if not already loaded.
pub fn attach_uprobe(
    bpf: &mut Ebpf,
    prog_name: &str,
    func_name: &str,
    path: &str,
    offset: u64,
) -> Result<UProbeLinkId> {
    let program: &mut UProbe = bpf
        .program_mut(prog_name)
        .with_context(|| format!("Failed to find program {}", prog_name))?
        .try_into()?;

    // Check if loaded using fd()
    if program.fd().is_err() {
        program.load()?;
    }

    // With a name, aya would read the whole file again for every program.
    program
        .attach(None, offset, path, None)
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

    spawn_ringbuf_handler(bpf, "EXEC_EVENTS", move |event: &ExecEvent| {
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

    spawn_ringbuf_handler(bpf, "LIB_OPEN_EVENTS", move |event: &LibOpenEvent| {
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

#[cfg(test)]
mod tests {
    use super::*;
    use honeybeepf_llm_common::{LlmEvent, MAX_SSL_BUF_SIZE};

    const H: usize = std::mem::size_of::<LlmEventHeader>();

    fn event(data: &[u8], offset: u32, len: u32) -> LlmEvent {
        let mut ev = LlmEvent::default();
        ev.header.len = len;
        ev.header.offset = offset;
        ev.header.buf_filled = u32::from(!data.is_empty());
        ev.buf[..data.len()].copy_from_slice(data);
        ev
    }

    /// One ring record is the event's bytes truncated to its real size; the value is 8-byte aligned.
    fn record(ev: &LlmEvent, bytes: usize) -> &[u8] {
        // SAFETY: LlmEvent is repr(C) + aya::Pod (every byte valid); bytes <= its size.
        unsafe { std::slice::from_raw_parts(ev as *const LlmEvent as *const u8, bytes) }
    }

    #[test]
    fn a_short_record_yields_only_its_payload() {
        let data = b"POST /v1/messages";
        let ev = event(data, 0, data.len() as u32);
        let (header, payload) =
            parse_llm_record(record(&ev, H + data.len())).expect("record parses");
        assert_eq!(payload, data);
        assert_eq!(header.len, data.len() as u32);
    }

    #[test]
    fn a_full_fragment_yields_the_whole_buffer() {
        let data = [0xabu8; MAX_SSL_BUF_SIZE];
        let ev = event(&data, 0, MAX_SSL_BUF_SIZE as u32);
        let (_, payload) =
            parse_llm_record(record(&ev, H + MAX_SSL_BUF_SIZE)).expect("record parses");
        assert_eq!(payload.len(), MAX_SSL_BUF_SIZE);
    }

    /// A call over the capture limit still fills one fragment: payload is clamped to the buffer.
    #[test]
    fn a_truncated_call_clamps_the_payload_to_the_buffer() {
        let data = [7u8; MAX_SSL_BUF_SIZE];
        let ev = event(&data, 0, 10 * MAX_SSL_BUF_SIZE as u32);
        let (_, payload) =
            parse_llm_record(record(&ev, H + MAX_SSL_BUF_SIZE)).expect("record parses");
        assert_eq!(payload.len(), MAX_SSL_BUF_SIZE);
    }

    #[test]
    fn a_payloadless_record_is_just_the_header() {
        let ev = event(b"", 0, 0);
        let (_, payload) = parse_llm_record(record(&ev, H)).expect("record parses");
        assert!(payload.is_empty());
    }

    #[test]
    fn a_record_shorter_than_its_header_is_dropped() {
        let ev = event(b"x", 0, 1);
        assert!(parse_llm_record(record(&ev, H - 1)).is_none());
    }

    #[test]
    fn a_record_shorter_than_its_claimed_payload_is_dropped() {
        let data = b"twenty-byte-payload!";
        let ev = event(data, 0, data.len() as u32);
        assert!(parse_llm_record(record(&ev, H + data.len() - 1)).is_none());
    }
}

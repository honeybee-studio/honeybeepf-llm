use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result};
use aya::{Ebpf, maps::RingBuf, programs::TracePoint};
use log::{info, warn};

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

pub struct TracepointConfig<'a> {
    pub program_name: &'a str,
    pub category: &'a str,
    pub name: &'a str,
}

pub const POLL_INTERVAL_MS: u64 = 10;

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

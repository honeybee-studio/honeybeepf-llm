use anyhow::{Context, Result};
use aya::Ebpf;
use aya::maps::{HashMap, RingBuf};
use aya::programs::TracePoint;
use honeybeepf_common::FileAccessEvent;
use log::info;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::probes::{IdentityResolver, Probe};
use crate::telemetry;

pub struct FileAccessProbe {
    pub watched_paths: Vec<String>,
    running: Arc<AtomicBool>,
}

impl FileAccessProbe {
    pub fn new(watched_paths: Vec<String>) -> Self {
        Self {
            watched_paths,
            running: Arc::new(AtomicBool::new(true)),
        }
    }
}

impl Default for FileAccessProbe {
    fn default() -> Self {
        Self {
            watched_paths: Vec::new(),
            running: Arc::new(AtomicBool::new(true)), // must be true for event loop
        }
    }
}

impl Probe for FileAccessProbe {
    fn attach(&self, bpf: &mut Ebpf, _resolver: IdentityResolver) -> Result<()> {
        // Populate watched paths map BEFORE loading the program
        self.populate_watched_paths(bpf)?;

        let program: &mut TracePoint = bpf
            .program_mut("sys_enter_openat")
            .context("Failed to find sys_enter_openat program")?
            .try_into()
            .context("Program is not a TracePoint")?;

        program.load()?;
        program
            .attach("syscalls", "sys_enter_openat")
            .context("Failed to attach sys_enter_openat tracepoint")?;

        info!("Attached tracepoint: syscalls/sys_enter_openat");
        info!("Watching {} sensitive paths", self.watched_paths.len());
        for path in &self.watched_paths {
            info!("  - {}", path);
        }

        self.spawn_event_handler(bpf, _resolver)?;

        telemetry::record_active_probe("file_access", 1);
        info!("FileAccessProbe attached successfully");

        Ok(())
    }
}

impl FileAccessProbe {
    /// Populate the WATCHED_PATHS eBPF map with exact path hashes.
    fn populate_watched_paths(&self, bpf: &mut Ebpf) -> Result<()> {
        let mut watched_map: HashMap<_, u64, u8> = bpf
            .map_mut("WATCHED_PATHS")
            .context("Failed to find WATCHED_PATHS map")?
            .try_into()
            .context("WATCHED_PATHS is not a HashMap")?;

        for path in &self.watched_paths {
            let hash = simple_hash(path.as_bytes());
            watched_map.insert(hash, 1, 0)?;
            info!("Added watched path: {} (hash: {:#x})", path, hash);
        }

        Ok(())
    }

    fn spawn_event_handler(&self, bpf: &mut Ebpf, _resolver: IdentityResolver) -> Result<()> {
        let ring_buf = RingBuf::try_from(
            bpf.take_map("FILE_ACCESS_EVENTS")
                .context("Failed to find FILE_ACCESS_EVENTS map")?,
        )?;

        let running = self.running.clone();

        std::thread::spawn(move || {
            let mut ring_buf = ring_buf;

            while running.load(Ordering::Relaxed) {
                if let Some(item) = ring_buf.next() {
                    if item.len() >= std::mem::size_of::<FileAccessEvent>() {
                        let event: FileAccessEvent = unsafe {
                            std::ptr::read_unaligned(item.as_ptr() as *const FileAccessEvent)
                        };

                        #[cfg(feature = "k8s")]
                        let pod_info =
                            _resolver.resolve_pod(event.metadata.pid, event.metadata.cgroup_id);

                        let comm = std::str::from_utf8(&event.comm)
                            .unwrap_or("<invalid>")
                            .trim_matches(char::from(0));

                        let filename = std::str::from_utf8(&event.filename)
                            .unwrap_or("<invalid>")
                            .trim_matches(char::from(0));

                        let flags_str = format_open_flags(event.flags);

                        #[cfg(feature = "k8s")]
                        if let Some(pod) = pod_info.as_ref() {
                            info!(
                                "FILE_ACCESS pid={} comm={} file={} flags={} cgroup={} namespace={} pod={}",
                                event.metadata.pid,
                                comm,
                                filename,
                                flags_str,
                                event.metadata.cgroup_id,
                                pod.namespace,
                                pod.pod_name,
                            );
                        } else {
                            info!(
                                "FILE_ACCESS pid={} comm={} file={} flags={} cgroup={}",
                                event.metadata.pid,
                                comm,
                                filename,
                                flags_str,
                                event.metadata.cgroup_id,
                            );
                        }

                        #[cfg(not(feature = "k8s"))]
                        info!(
                            "FILE_ACCESS pid={} comm={} file={} flags={} cgroup={}",
                            event.metadata.pid, comm, filename, flags_str, event.metadata.cgroup_id,
                        );

                        telemetry::record_file_access_event(
                            filename,
                            &flags_str,
                            comm,
                            event.metadata.cgroup_id,
                            #[cfg(feature = "k8s")]
                            pod_info.as_ref(),
                        );
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        });

        Ok(())
    }
}

/// FNV-1a hash - must match the eBPF implementation exactly
fn simple_hash(data: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325; // FNV offset basis
    for &b in data {
        if b == 0 {
            break;
        }
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3); // FNV prime
    }
    hash
}

fn format_open_flags(flags: u32) -> String {
    let mut parts = Vec::new();

    // Access mode
    match flags & 0b11 {
        0 => parts.push("O_RDONLY"),
        1 => parts.push("O_WRONLY"),
        2 => parts.push("O_RDWR"),
        _ => {}
    }

    // Common flags
    if flags & 0o100 != 0 {
        parts.push("O_CREAT");
    }
    if flags & 0o1000 != 0 {
        parts.push("O_TRUNC");
    }
    if flags & 0o2000 != 0 {
        parts.push("O_APPEND");
    }

    parts.join("|")
}

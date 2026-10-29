pub mod settings;
pub mod telemetry;

#[cfg(feature = "k8s")]
pub mod k8s;

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::atomic::Ordering,
    time::Duration,
};

use anyhow::Result;
use aya::Ebpf;
use aya_log::EbpfLogger;
use log::{debug, info, warn};
use tokio::{signal, time::Instant};

use crate::settings::Settings;

pub mod probes;
use crate::probes::{
    DynamicProbe, IdentityResolver, LibOpen, LibOpenSeen, Probe, ProcessInfo,
    builtin::{
        FileAccessProbe,
        llm::{
            ExecNotify, ExecPidQueue, LibOpenQueue, LlmProbe, setup_exec_watch,
            setup_lib_open_watch,
        },
        process_lifecycle::{self, ProcessLifecycleProbe},
    },
    discovery::resolve_in_root,
    request_shutdown, shutdown_flag,
};

pub struct HoneyBeeEngine {
    pub settings: Settings,
    bpf: Ebpf,
    dynamic_probes: Vec<Box<dyn DynamicProbe>>,
    resolver: IdentityResolver,
}

impl HoneyBeeEngine {
    pub fn new(settings: Settings, bytecode: &[u8]) -> Result<Self> {
        bump_memlock_rlimit()?;
        let mut bpf = Ebpf::load(bytecode)?;

        // Initialize the eBPF Logger only in debug mode
        if settings.debug.unwrap_or(false) {
            if let Err(e) = EbpfLogger::init(&mut bpf) {
                warn!("Failed to initialize eBPF logger: {}", e);
            } else {
                debug!("eBPF logger enabled (debug mode)");
            }
        } else {
            debug!("eBPF logger disabled (production mode)");
        }
        let mut dynamic_probes: Vec<Box<dyn DynamicProbe>> = Vec::new();
        if settings.builtin_probes.llm.unwrap_or(false) {
            let executables = settings.builtin_probes.llm_executables.unwrap_or(true);
            dynamic_probes.push(Box::new(LlmProbe::new(executables)));
        }

        let resolver = IdentityResolver::none();

        Ok(Self {
            settings,
            bpf,
            dynamic_probes,
            resolver,
        })
    }

    pub async fn run(mut self) -> Result<()> {
        let _shutdown = ShutdownOnDrop;
        if let Err(e) = telemetry::init_metrics() {
            warn!(
                "Failed to initialize OpenTelemetry metrics: {}. Metrics will not be exported.",
                e
            );
        }

        // Start K8s pod watcher if available
        #[cfg(feature = "k8s")]
        {
            use std::sync::Arc;

            // In restrictive enterprise environments, the Downward API might be blocked or missing.
            // Fall back to HOSTNAME or reading the system hostname directly (since agent runs with hostNetwork: true).
            let resolved_node_name = std::env::var("K8S_NODE_NAME")
                .or_else(|_| std::env::var("HOSTNAME"))
                .or_else(|_| std::fs::read_to_string("/etc/hostname").map(|s| s.trim().to_string()))
                .or_else(|_| {
                    std::fs::read_to_string("/proc/sys/kernel/hostname")
                        .map(|s| s.trim().to_string())
                });

            if let Ok(node_name) = resolved_node_name {
                info!("Starting K8s pod watcher for node: {}", node_name);
                let pod_resolver = Arc::new(k8s::PodResolver::new());
                match pod_resolver.start_k8s_watcher(node_name).await {
                    Ok(_handle) => {
                        info!("K8s pod watcher started");
                        self.resolver = IdentityResolver::with_pod_resolver(pod_resolver);
                    }
                    Err(e) => {
                        warn!(
                            "K8s pod watcher failed to start: {}. Pod resolution disabled.",
                            e
                        );
                    }
                }
            } else {
                info!(
                    "K8S_NODE_NAME and hostname not found. Pod resolution disabled (running outside K8s?)."
                );
            }
        }

        self.attach_probes()?;

        // Start dynamic probe discovery if any dynamic probes are enabled
        if !self.dynamic_probes.is_empty() {
            let exec_watch = setup_exec_watch(&mut self.bpf)?;
            let lib_watch = setup_lib_open_watch(&mut self.bpf)?;
            self.run_discovery(exec_watch, lib_watch).await?;
        } else {
            info!("Monitoring active. Press Ctrl-C to exit.");
            signal::ctrl_c().await?;
        }

        info!("Exiting...");
        Ok(())
    }

    /// Run the dynamic discovery loop that monitors for new processes and notifies probes.
    async fn run_discovery(
        &mut self,
        (queue, notify): (ExecPidQueue, ExecNotify),
        (lib_queue, lib_notify, mut lib_seen): (LibOpenQueue, ExecNotify, LibOpenSeen),
    ) -> Result<()> {
        const BATCH_WAIT_MS: u64 = 50;

        let shutdown = shutdown_flag();
        // A deadline branch rather than a sleep inside a branch, so opens are handled while it waits.
        let mut exec_due: Option<Instant> = None;

        info!("Dynamic discovery active. Press Ctrl-C to exit.");

        loop {
            tokio::select! {
                _ = signal::ctrl_c() => break,
                _ = lib_notify.notified() => {
                    let opens: Vec<LibOpen> = {
                        let mut q = lib_queue.lock().unwrap_or_else(|e| e.into_inner());
                        q.drain(..).collect()
                    };
                    for open in opens {
                        debug!("libssl open: {} (PID: {})", open.path, open.pid);
                        let root = PathBuf::from(format!("/proc/{}/root", open.pid));
                        // Process gone: not marked seen, so the kernel reports the next open again.
                        let Some(host_path) = resolve_in_root(&root, Path::new(&open.path)) else {
                            continue;
                        };
                        let libs = HashSet::from([host_path.to_string_lossy().into_owned()]);
                        self.notify_probes(&ProcessInfo { pid: open.pid, libs, exe: None });
                        let _ = lib_seen.insert(open.seen_key, open.ts, 0);
                    }
                }
                _ = notify.notified(), if exec_due.is_none() => {
                    // Brief delay to batch rapid exec events
                    exec_due = Some(Instant::now() + Duration::from_millis(BATCH_WAIT_MS));
                }
                _ = tokio::time::sleep_until(exec_due.unwrap_or_else(Instant::now)), if exec_due.is_some() => {
                    exec_due = None;
                    let pids: Vec<u32> = {
                        let mut q = queue.lock().unwrap_or_else(|e| e.into_inner());
                        q.drain(..).collect()
                    };

                    if !pids.is_empty() {
                        for pid in pids {
                            // Pre-fetch libraries once per process to avoid redundant I/O
                            let libs = match crate::probes::discovery::get_process_libraries(pid) {
                                Ok(l) => l,
                                Err(e) => {
                                    // Debug level because this can happen for short-lived processes
                                    log::debug!("Failed to get libraries for PID {}: {}", pid, e);
                                    continue;
                                }
                            };

                            // Check if useful
                            if libs.is_empty() {
                                continue;
                            }

                            let exe = crate::probes::discovery::get_process_exe(pid);
                            self.notify_probes(&ProcessInfo { pid, libs, exe });
                        }
                    }
                }
            }

            if shutdown.load(Ordering::Relaxed) {
                break;
            }
        }

        crate::probes::builtin::llm::log_event_stats();
        telemetry::shutdown_metrics();

        Ok(())
    }

    fn notify_probes(&mut self, process_info: &ProcessInfo) {
        for probe in &self.dynamic_probes {
            if let Err(e) = probe.on_exec(&mut self.bpf, process_info) {
                warn!("Probe on_exec error: {}", e);
            }
        }
    }

    fn attach_probes(&mut self) -> Result<()> {
        // File Access Probe
        if self
            .settings
            .builtin_probes
            .filesystem
            .file_access
            .unwrap_or(false)
        {
            let watched_paths = self
                .settings
                .builtin_probes
                .filesystem
                .watched_paths
                .clone()
                .unwrap_or_default();

            if watched_paths.is_empty() {
                info!("FileAccessProbe is ENABLED but no watched_paths configured, skipping...");
            } else {
                info!("FileAccessProbe is ENABLED, attaching...");
                let probe = FileAccessProbe::new(watched_paths);
                probe.attach(&mut self.bpf, self.resolver.clone())?;
            }
        }

        for probe in &self.dynamic_probes {
            probe.attach(&mut self.bpf, self.resolver.clone())?;
            telemetry::record_active_probe("llm", 1);
        }

        if self
            .settings
            .builtin_probes
            .process_lifecycle
            .unwrap_or(false)
        {
            if let Some(hook) = process_lifecycle::missing_kernel_hook() {
                warn!("Process lifecycle probe skipped: kernel has no {hook}");
            } else {
                ProcessLifecycleProbe.attach(&mut self.bpf, self.resolver.clone())?;
                telemetry::record_active_probe("process_lifecycle", 1);
                info!("Process lifecycle probe attached");
            }
        }
        Ok(())
    }
}

/// Stops the ringbuf readers on every return from `run`: the runtime's drop waits for blocking tasks forever.
struct ShutdownOnDrop;

impl Drop for ShutdownOnDrop {
    fn drop(&mut self) {
        request_shutdown();
    }
}

fn bump_memlock_rlimit() -> Result<()> {
    let rlim = libc::rlimit {
        rlim_cur: libc::RLIM_INFINITY,
        rlim_max: libc::RLIM_INFINITY,
    };
    let ret = unsafe { libc::setrlimit(libc::RLIMIT_MEMLOCK, &rlim) };
    if ret != 0 {
        warn!("Failed to increase rlimit");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;
    use crate::{
        probes::POLL_INTERVAL_MS,
        settings::{BuiltinProbes, FilesystemProbes},
    };

    /// A 64-bit little-endian BPF ELF header with no sections, which aya loads without the kernel.
    fn empty_bpf_object() -> Vec<u8> {
        let mut elf = vec![0x7f, b'E', b'L', b'F', 2, 1, 1];
        elf.resize(16, 0);
        // e_type ET_REL, e_machine EM_BPF, e_version 1; entry, offsets and flags stay 0.
        elf.extend_from_slice(&[1, 0, 247, 0, 1, 0, 0, 0]);
        elf.resize(52, 0);
        // e_ehsize 64, no program headers, e_shentsize 64, no sections.
        elf.extend_from_slice(&[64, 0, 0, 0, 0, 0, 64, 0]);
        elf.resize(64, 0);
        elf
    }

    // Leaves the process-wide shutdown flag set; no other test in this binary reads it.
    #[test]
    fn test_startup_error_stops_blocking_readers() {
        let settings = Settings {
            builtin_probes: BuiltinProbes {
                // The object has no file access program, so attaching it fails inside run().
                filesystem: FilesystemProbes {
                    file_access: Some(true),
                    watched_paths: Some(vec!["/etc".into()]),
                },
                ..Default::default()
            },
            ..Default::default()
        };
        let engine =
            HoneyBeeEngine::new(settings, &empty_bpf_object()).expect("empty object loads");
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");

        let result = rt.block_on(async {
            let shutdown = shutdown_flag();
            // The loop shape of spawn_ringbuf_handler, which needs a kernel ringbuf map.
            tokio::task::spawn_blocking(move || {
                while !shutdown.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(POLL_INTERVAL_MS));
                }
            });
            engine.run().await
        });
        let err = result.expect_err("attaching the file access probe fails");
        assert!(format!("{err:#}").contains("WATCHED_PATHS"), "{err:#}");

        let (done_tx, done_rx) = mpsc::channel();
        std::thread::spawn(move || {
            drop(rt);
            let _ = done_tx.send(());
        });
        // Generous: with a kubeconfig, the pod watcher's DNS lookup also runs on a blocking thread.
        assert!(
            done_rx.recv_timeout(Duration::from_secs(30)).is_ok(),
            "runtime drop is still waiting on the reader loop"
        );
    }
}

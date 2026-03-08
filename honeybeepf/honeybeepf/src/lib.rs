pub mod settings;
pub mod telemetry;

#[cfg(feature = "k8s")]
pub mod k8s;

use std::{sync::atomic::Ordering, time::Duration};

use anyhow::Result;
use aya::Ebpf;
use aya_log::EbpfLogger;
use log::{debug, info, warn};
use tokio::signal;

use crate::settings::Settings;

pub mod probes;
use crate::probes::{
    DynamicProbe, IdentityResolver, Probe, ProcessInfo,
    builtin::{
        block_io::BlockIoProbe,
        gpu_usage::GpuUsageProbe,
        llm::{ExecNotify, ExecPidQueue, LlmProbe, setup_exec_watch},
        network::NetworkLatencyProbe,
    },
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
            dynamic_probes.push(Box::new(LlmProbe::default()));
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
            let (queue, notify) = setup_exec_watch(&mut self.bpf)?;
            self.run_discovery(queue, notify).await?;
        } else {
            info!("Monitoring active. Press Ctrl-C to exit.");
            signal::ctrl_c().await?;
        }

        request_shutdown();
        info!("Exiting...");
        Ok(())
    }

    /// Run the dynamic discovery loop that monitors for new processes and notifies probes.
    async fn run_discovery(&mut self, queue: ExecPidQueue, notify: ExecNotify) -> Result<()> {
        const BATCH_WAIT_MS: u64 = 50;

        let shutdown = shutdown_flag();

        info!("Dynamic discovery active. Press Ctrl-C to exit.");

        loop {
            tokio::select! {
                _ = signal::ctrl_c() => break,
                _ = notify.notified() => {
                    // Brief delay to batch rapid exec events
                    tokio::time::sleep(Duration::from_millis(BATCH_WAIT_MS)).await;

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

                            let process_info = ProcessInfo { pid, libs };

                            for probe in &self.dynamic_probes {
                                if let Err(e) = probe.on_exec(&mut self.bpf, &process_info) {
                                    warn!("Probe on_exec error: {}", e);
                                }
                            }
                        }
                    }
                }
            }

            if shutdown.load(Ordering::Relaxed) {
                break;
            }
        }

        telemetry::shutdown_metrics();

        Ok(())
    }

    fn attach_probes(&mut self) -> Result<()> {
        if self
            .settings
            .builtin_probes
            .network_latency
            .unwrap_or(false)
        {
            NetworkLatencyProbe.attach(&mut self.bpf, self.resolver.clone())?;
            // Note: network_latency probe currently logs connection events only,
            // latency measurement not yet implemented
        }

        if self.settings.builtin_probes.block_io.unwrap_or(false) {
            BlockIoProbe.attach(&mut self.bpf, self.resolver.clone())?;
            telemetry::record_active_probe("block_io", 1);
        }

        if self.settings.builtin_probes.gpu_usage.unwrap_or(false) {
            GpuUsageProbe.attach(&mut self.bpf, self.resolver.clone())?;
            telemetry::record_active_probe("gpu_usage", 1);
        }

        for probe in &self.dynamic_probes {
            probe.attach(&mut self.bpf, self.resolver.clone())?;
            telemetry::record_active_probe("llm", 1);
        }

        Ok(())
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

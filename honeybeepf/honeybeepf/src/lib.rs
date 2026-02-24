pub mod settings;
pub mod telemetry;

use std::{collections::HashSet, sync::atomic::Ordering, time::Duration};

use anyhow::Result;
use aya::Ebpf;
use aya_log::EbpfLogger;
use log::{debug, info, warn};
use tokio::signal;

use crate::settings::Settings;

pub mod probes;
use crate::probes::{
    Probe,
    builtin::{
        block_io::BlockIoProbe,
        gpu_usage::GpuUsageProbe,
        llm::{
            ExecNotify, ExecPidQueue, LlmProbe, attach_new_targets_for_pids, discovery,
            setup_exec_watch,
        },
        network::NetworkLatencyProbe,
    },
    request_shutdown, shutdown_flag,
};

pub struct HoneyBeeEngine {
    pub settings: Settings,
    bpf: Ebpf,
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

        Ok(Self { settings, bpf })
    }

    pub async fn run(mut self) -> Result<()> {
        if let Err(e) = telemetry::init_metrics() {
            warn!(
                "Failed to initialize OpenTelemetry metrics: {}. Metrics will not be exported.",
                e
            );
        }

        self.attach_probes()?;

        // Start LLM dynamic discovery if enabled
        if self.settings.builtin_probes.llm.unwrap_or(false) {
            let (queue, notify) = setup_exec_watch(&mut self.bpf)?;
            self.run_llm_discovery(queue, notify).await?;
        } else {
            info!("Monitoring active. Press Ctrl-C to exit.");
            signal::ctrl_c().await?;
        }

        request_shutdown();
        info!("Exiting...");
        Ok(())
    }

    /// Run the LLM discovery loop that monitors for new processes and attaches SSL probes.
    async fn run_llm_discovery(&mut self, queue: ExecPidQueue, notify: ExecNotify) -> Result<()> {
        const BATCH_WAIT_MS: u64 = 50;

        // Seed with initial targets to avoid duplicate attachments
        let mut known_targets: HashSet<String> = discovery::find_all_targets().unwrap_or_default();
        let shutdown = shutdown_flag();

        info!("LLM discovery active. Press Ctrl-C to exit.");

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

                    if !pids.is_empty()
                        && let Err(e) = attach_new_targets_for_pids(&mut self.bpf, &mut known_targets, &pids) {
                            warn!("LLM re-discovery error: {}", e);
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
            NetworkLatencyProbe.attach(&mut self.bpf)?;
            // Note: network_latency probe currently logs connection events only,
            // latency measurement not yet implemented
        }

        if self.settings.builtin_probes.block_io.unwrap_or(false) {
            BlockIoProbe.attach(&mut self.bpf)?;
            telemetry::record_active_probe("block_io", 1);
        }

        if self.settings.builtin_probes.gpu_usage.unwrap_or(false) {
            GpuUsageProbe.attach(&mut self.bpf)?;
            telemetry::record_active_probe("gpu_usage", 1);
        }

        if self.settings.builtin_probes.llm.unwrap_or(false) {
            LlmProbe.attach(&mut self.bpf)?;
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

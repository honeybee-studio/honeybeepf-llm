use anyhow::{Context, Result};
use aya::Ebpf;
use aya::programs::KProbe;
use honeybeepf_llm_common::ProcessExitEvent;
use log::{info, warn};

use crate::probes::{IdentityResolver, Probe, spawn_ringbuf_handler};
use crate::telemetry;

pub struct ProcessLifecycleProbe;

impl Probe for ProcessLifecycleProbe {
    fn attach(&self, bpf: &mut Ebpf, _resolver: IdentityResolver) -> Result<()> {
        // 1. Attach to do_group_exit to capture the exit code reliably
        let exit_prog: &mut KProbe = bpf
            .program_mut("honeybeepf_llm_process_exit")
            .context("BPF program honeybeepf-llm_process_exit not found")?
            .try_into()?;
        exit_prog.load()?;
        exit_prog.attach("do_group_exit", 0)?;

        // 2. Attach to oom_kill_process to flag kernel-level OOM events
        let oom_prog: &mut KProbe = bpf
            .program_mut("honeybeepf_llm_oom_kill_process")
            .context("BPF program honeybeepf-llm_oom_kill_process not found")?
            .try_into()?;
        oom_prog.load()?;
        oom_prog.attach("oom_kill_process", 0)?;

        spawn_ringbuf_handler(
            bpf,
            "PROCESS_EXIT_EVENTS",
            move |event: ProcessExitEvent| {
                let comm = std::str::from_utf8(&event.comm)
                    .unwrap_or("<invalid>")
                    .trim_matches(char::from(0));

                let lifetime_ms = if event.start_time > 0 {
                    (event.exit_time - event.start_time) / 1_000_000
                } else {
                    0
                };

                let signal = event.exit_code & 0x7f;
                let status = (event.exit_code >> 8) & 0xff;

                // Distinguish OOM_TERMINATED vs MANUAL_SIGKILL using the kernel-side marker (ppid 0xDEAD)
                let cause = if signal != 0 {
                    match signal {
                        9 => {
                            if event.ppid == 0xDEAD {
                                "OOM_TERMINATED"
                            } else {
                                "MANUAL_SIGKILL"
                            }
                        }
                        11 => "SEGFAULT",
                        15 => "SIGTERM",
                        _ => "SIGNAL_TERMINATED",
                    }
                } else {
                    "NORMAL_EXIT"
                };

                if signal != 0 {
                    warn!(
                        "PROCESS_TERMINATED cause={} pid={} comm={} signal={} duration={}ms cgroup={}",
                        cause, event.pid, comm, signal, lifetime_ms, event.cgroup_id
                    );
                } else {
                    info!(
                        "PROCESS_EXIT cause=NORMAL pid={} comm={} status={} duration={}ms cgroup={}",
                        event.pid, comm, status, lifetime_ms, event.cgroup_id
                    );
                }

                telemetry::record_process_exit(cause, status as u32, lifetime_ms, event.cgroup_id);
            },
        )?;

        Ok(())
    }
}

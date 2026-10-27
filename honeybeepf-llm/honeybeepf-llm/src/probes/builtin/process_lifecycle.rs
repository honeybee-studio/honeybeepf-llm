use std::fs::File;
use std::io::{BufRead, BufReader};

use anyhow::{Context, Result};
use aya::Ebpf;
use aya::programs::{KProbe, TracePoint};
use honeybeepf_llm_common::ProcessExitEvent;
use log::{info, warn};

use crate::probes::{IdentityResolver, Probe, spawn_ringbuf_handler};
use crate::telemetry;

pub struct ProcessLifecycleProbe;

impl Probe for ProcessLifecycleProbe {
    fn attach(&self, bpf: &mut Ebpf, _resolver: IdentityResolver) -> Result<()> {
        // 1. do_group_exit marks user-space groups; acct_collect reports each one at its last thread
        let group_prog: &mut KProbe = bpf
            .program_mut("honeybeepf_llm_group_exit")
            .context("BPF program honeybeepf-llm_group_exit not found")?
            .try_into()?;
        group_prog.load()?;
        group_prog.attach("do_group_exit", 0)?;

        let exit_prog: &mut KProbe = bpf
            .program_mut("honeybeepf_llm_process_exit")
            .context("BPF program honeybeepf-llm_process_exit not found")?
            .try_into()?;
        exit_prog.load()?;
        exit_prog
            .attach("acct_collect", 0)
            .context("attach to acct_collect (needs CONFIG_BSD_PROCESS_ACCT)")?;

        // 2. oom:mark_victim flags OOM victims; its pid field needs no kernel struct layout
        let oom_prog: &mut TracePoint = bpf
            .program_mut("honeybeepf_llm_oom_mark_victim")
            .context("BPF program honeybeepf-llm_oom_mark_victim not found")?
            .try_into()?;
        oom_prog.load()?;
        oom_prog.attach("oom", "mark_victim")?;

        spawn_ringbuf_handler(
            bpf,
            "PROCESS_EXIT_EVENTS",
            move |event: ProcessExitEvent| {
                let comm = std::str::from_utf8(&event.comm)
                    .unwrap_or("<invalid>")
                    .trim_matches(char::from(0));

                let lifetime = lifetime_secs(event.start_time, event.exit_time);

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
                        "PROCESS_TERMINATED cause={} pid={} comm={} signal={} duration={:.3}s cgroup={}",
                        cause, event.pid, comm, signal, lifetime, event.cgroup_id
                    );
                } else {
                    info!(
                        "PROCESS_EXIT cause=NORMAL pid={} comm={} status={} duration={:.3}s cgroup={}",
                        event.pid, comm, status, lifetime, event.cgroup_id
                    );
                }

                telemetry::record_process_exit(cause, status as u32, lifetime, event.cgroup_id);
            },
        )?;

        Ok(())
    }
}

/// The kernel hook this probe needs that the running kernel lacks, if any.
pub fn missing_kernel_hook() -> Option<&'static str> {
    // An unreadable kallsyms proves nothing; attach then reports the real error.
    let kallsyms = File::open("/proc/kallsyms").ok()?;
    missing_hook(BufReader::new(kallsyms))
}

fn missing_hook(kallsyms: impl BufRead) -> Option<&'static str> {
    let has_acct_collect = kallsyms
        .lines()
        .map_while(std::io::Result::ok)
        .any(|line| line.split_whitespace().nth(2) == Some("acct_collect"));
    (!has_acct_collect).then_some("acct_collect (CONFIG_BSD_PROCESS_ACCT)")
}

/// Lifetime in seconds from the exec and exit timestamps (ns); 0 when the exec was not seen.
fn lifetime_secs(start_ns: u64, exit_ns: u64) -> f64 {
    if start_ns == 0 {
        return 0.0;
    }
    exit_ns.saturating_sub(start_ns) as f64 / 1e9
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifetime_is_reported_in_seconds() {
        let start_ns = 5_000_000_000;
        assert_eq!(lifetime_secs(start_ns, start_ns + 1_500_000_000), 1.5);
        assert_eq!(lifetime_secs(start_ns, start_ns + 250_000_000), 0.25);
    }

    #[test]
    fn lifetime_is_zero_without_a_start_time_or_with_a_backwards_clock() {
        assert_eq!(lifetime_secs(0, 7_000_000_000), 0.0);
        assert_eq!(lifetime_secs(9_000_000_000, 7_000_000_000), 0.0);
    }

    #[test]
    fn a_kernel_without_acct_collect_is_reported_missing() {
        let kallsyms = "0000000000000000 T do_group_exit\n\
                        0000000000000000 t acct_collect.cold\n\
                        0000000000000000 t acct_collect_x [some_mod]\n";
        assert_eq!(
            missing_hook(kallsyms.as_bytes()),
            Some("acct_collect (CONFIG_BSD_PROCESS_ACCT)")
        );
    }

    #[test]
    fn a_kernel_with_acct_collect_has_nothing_missing() {
        let kallsyms = "0000000000000000 T do_group_exit\n\
                        0000000000000000 T acct_collect\n";
        assert_eq!(missing_hook(kallsyms.as_bytes()), None);
    }

    /// OOM victims must come from a tracepoint field, not a kprobe reading hand-written struct offsets.
    #[test]
    fn oom_victims_are_marked_by_the_mark_victim_tracepoint() -> Result<()> {
        use object::{Object, ObjectSection, ObjectSymbol, SymbolKind};

        let data = aya::include_bytes_aligned!(concat!(env!("OUT_DIR"), "/honeybeepf-llm"));
        let obj = object::File::parse(data)?;
        let mut oom_programs = Vec::new();
        for sym in obj.symbols().filter(|s| s.kind() == SymbolKind::Text) {
            let name = sym.name()?;
            let Some(index) = sym.section_index() else {
                continue;
            };
            if name.contains("oom") {
                oom_programs.push((obj.section_by_index(index)?.name()?, name));
            }
        }
        assert_eq!(
            oom_programs,
            [("tracepoint", "honeybeepf_llm_oom_mark_victim")]
        );
        Ok(())
    }
}

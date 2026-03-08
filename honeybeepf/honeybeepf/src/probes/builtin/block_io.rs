use anyhow::Result;
use aya::Ebpf;
use honeybeepf_common::{BlockIoEvent, BlockIoEventType};
use log::info;

use crate::probes::{
    IdentityResolver, Probe, TracepointConfig, attach_tracepoint, spawn_ringbuf_handler,
};
use crate::telemetry;

pub struct BlockIoProbe;

impl Probe for BlockIoProbe {
    fn attach(&self, bpf: &mut Ebpf, resolver: IdentityResolver) -> Result<()> {
        info!("Attaching block IO probes...");

        attach_tracepoint(
            bpf,
            TracepointConfig {
                program_name: "honeybeepf_block_io_start",
                category: "block",
                name: "block_io_start",
            },
        )?;
        attach_tracepoint(
            bpf,
            TracepointConfig {
                program_name: "honeybeepf_block_io_done",
                category: "block",
                name: "block_io_done",
            },
        )?;

        spawn_ringbuf_handler(bpf, "BLOCK_IO_EVENTS", move |event: BlockIoEvent| {
            let rwbs = std::str::from_utf8(&event.rwbs)
                .unwrap_or("<invalid>")
                .trim_matches(char::from(0));
            let comm = std::str::from_utf8(&event.comm)
                .unwrap_or("<invalid>")
                .trim_matches(char::from(0));

            let type_str = match BlockIoEventType::from(event.event_type) {
                BlockIoEventType::Start => "START",
                BlockIoEventType::Done => "DONE",
                BlockIoEventType::Unknown => "UNKNOWN",
            };

            // Create device name (major:minor)
            let device = format!("{}:{}", event.dev >> 20, event.dev & 0xFFFFF);

            let _pod_info = resolver.resolve_pod(event.metadata.pid, event.metadata.cgroup_id);

            info!(
                "BlockIO {} pid={} dev={} sector={} nr_sector={} bytes={} rwbs={} comm={}",
                type_str,
                event.metadata.pid,
                device,
                event.sector,
                event.nr_sector,
                event.bytes,
                rwbs,
                comm
            );

            telemetry::record_block_io_event(
                type_str,
                event.bytes as u64,
                None, // Latency requires separate calculation
                &device,
                comm,
                #[cfg(feature = "k8s")]
                _pod_info.as_deref(),
            );
        })?;
        Ok(())
    }
}

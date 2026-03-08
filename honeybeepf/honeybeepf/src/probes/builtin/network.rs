use std::net::Ipv4Addr;

use anyhow::Result;
use aya::Ebpf;
use honeybeepf_common::ConnectionEvent;
use log::info;

use crate::probes::{
    IdentityResolver, Probe, TracepointConfig, attach_tracepoint, spawn_ringbuf_handler,
};

pub struct NetworkLatencyProbe;

impl Probe for NetworkLatencyProbe {
    fn attach(&self, bpf: &mut Ebpf, resolver: IdentityResolver) -> Result<()> {
        info!("Attaching network latency probes...");
        attach_tracepoint(
            bpf,
            TracepointConfig {
                program_name: "honeybeepf",
                category: "syscalls",
                name: "sys_enter_connect",
            },
        )?;

        spawn_ringbuf_handler(bpf, "NETWORK_EVENTS", move |event: ConnectionEvent| {
            let dest_ip = Ipv4Addr::from(u32::from_be(event.dest_addr));
            let dest_port = u16::from_be(event.dest_port);

            let _pod_info = resolver.resolve_pod(event.metadata.pid, event.metadata.cgroup_id);
            #[cfg(feature = "k8s")]
            let pod_str = _pod_info
                .as_ref()
                .map(|p| format!(" | Pod: {}/{}", p.namespace, p.pod_name))
                .unwrap_or_default();
            #[cfg(not(feature = "k8s"))]
            let pod_str = "";

            // Read process name from /proc since ConnectionEvent doesn't have comm field
            let comm = std::fs::read_to_string(format!("/proc/{}/comm", event.metadata.pid))
                .unwrap_or_default();
            let comm = comm.trim();

            info!(
                "PID {} ({}) connecting to {}:{} (cgroup_id={}, ts={}){}",
                event.metadata.pid,
                comm,
                dest_ip,
                dest_port,
                event.metadata.cgroup_id,
                event.metadata.timestamp,
                pod_str
            );

            crate::telemetry::record_network_connection(
                "tcp", // Assuming TCP for sys_enter_connect
                &format!("{}:{}", dest_ip, dest_port),
                comm,
                #[cfg(feature = "k8s")]
                _pod_info.as_deref(),
            );
        })?;

        Ok(())
    }
}

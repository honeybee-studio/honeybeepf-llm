//! Exec watch tracepoint for LLM probe discovery.
//!
//! Notifies userspace when new processes exec, allowing dynamic
//! attachment of SSL probes to newly started processes.

use aya_ebpf::{
    helpers::{bpf_get_current_comm, bpf_get_current_pid_tgid, bpf_ktime_get_ns},
    macros::{map, tracepoint},
    maps::{HashMap, RingBuf},
    programs::TracePointContext,
};
use honeybeepf_llm_common::ExecEvent;

use super::llm::maps::EXEC_RINGBUF_SIZE;

#[map]
pub static EXEC_EVENTS: RingBuf = RingBuf::with_byte_size(EXEC_RINGBUF_SIZE, 0);

#[map]
pub static PROCESS_START_TIMES: HashMap<u32, u64> = HashMap::with_max_entries(10240, 0);

/// Tracepoint for sched_process_exec - fires when a process calls exec().
#[map]
pub static PROCESS_NAMES: HashMap<u32, [u8; 16]> = HashMap::with_max_entries(10240, 0);

#[tracepoint]
pub fn probe_exec(_ctx: TracePointContext) -> u32 {
    let pid = (bpf_get_current_pid_tgid() >> 32) as u32;

    // Cache the accurate process name during exec phase
    if let Ok(comm) = bpf_get_current_comm() {
        let _ = PROCESS_NAMES.insert(&pid, &comm, 0);
    }

    // Cache the start timestamp
    let ts = unsafe { bpf_ktime_get_ns() };
    let _ = PROCESS_START_TIMES.insert(&pid, &ts, 0);

    // Notify userspace for LLM probe attachment
    if let Some(mut slot) = EXEC_EVENTS.reserve::<ExecEvent>(0) {
        let event = unsafe { &mut *slot.as_mut_ptr() };
        event.pid = pid;
        event._pad = 0;
        slot.submit(0);
    }
    0
}

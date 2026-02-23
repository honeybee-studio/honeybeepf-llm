//! Exec watch tracepoint for LLM probe discovery.
//!
//! Notifies userspace when new processes exec, allowing dynamic
//! attachment of SSL probes to newly started processes.

use aya_ebpf::{
    helpers::bpf_get_current_pid_tgid,
    macros::{map, tracepoint},
    maps::RingBuf,
    programs::TracePointContext,
};
use honeybeepf_llm_common::ExecEvent;

use super::llm::maps::EXEC_RINGBUF_SIZE;

#[map]
pub static EXEC_EVENTS: RingBuf = RingBuf::with_byte_size(EXEC_RINGBUF_SIZE, 0);

/// Tracepoint for sched_process_exec - fires when a process calls exec().
#[tracepoint]
pub fn probe_exec(_ctx: TracePointContext) -> u32 {
    if let Some(mut slot) = EXEC_EVENTS.reserve::<ExecEvent>(0) {
        let event = unsafe { &mut *slot.as_mut_ptr() };
        event.pid = (bpf_get_current_pid_tgid() >> 32) as u32;
        event._pad = 0;
        slot.submit(0);
    }
    0
}

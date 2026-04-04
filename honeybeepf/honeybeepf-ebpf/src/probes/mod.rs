use aya_ebpf::{
    helpers::{bpf_get_current_cgroup_id, bpf_get_current_pid_tgid, bpf_ktime_get_ns},
    maps::RingBuf,
};

pub mod builtin;
pub mod custom;

use honeybeepf_common::EventMetadata;

/// Trait defining the lifecycle of an eBPF event
pub trait HoneyBeeEvent<C> {
    /// Each event must define how to fill its specific fields
    fn fill(&mut self, ctx: &C) -> Result<(), u32>;

    // Accessor for common metadata
    fn metadata(&mut self) -> &mut EventMetadata;

    /// Common logic to populate base metadata
    fn init_base(&mut self) {
        unsafe {
            let m = self.metadata();
            m.pid = (bpf_get_current_pid_tgid() >> 32) as u32;
            m.cgroup_id = bpf_get_current_cgroup_id();
            m.timestamp = bpf_ktime_get_ns();
        }
    }
}

#[repr(u32)]
pub enum EmitStatus {
    Success = 0,
    Failure = 1,
}

/// A generic reporter function to reduce boilerplate
pub fn emit_event<C, T: HoneyBeeEvent<C> + 'static>(ringbuf: &RingBuf, ctx: &C) -> u32 {
    if let Some(mut slot) = ringbuf.reserve::<T>(0) {
        let event = unsafe { &mut *slot.as_mut_ptr() };

        // Populate event data
        match event.fill(ctx) {
            Ok(_) => {
                slot.submit(0);
                EmitStatus::Success as u32
            }
            Err(e) => {
                slot.discard(0);
                e
            }
        }
    } else {
        EmitStatus::Failure as u32
    }
}

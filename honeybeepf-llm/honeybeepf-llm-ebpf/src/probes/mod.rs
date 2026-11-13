use aya_ebpf::{
    bindings::bpf_pidns_info,
    helpers::{
        bpf_get_current_cgroup_id, bpf_get_current_pid_tgid, bpf_get_ns_current_pid_tgid,
        bpf_ktime_get_ns,
    },
    maps::RingBuf,
};

pub mod builtin;
pub mod custom;

use honeybeepf_llm_common::EventMetadata;

/// The agent's PID namespace (st_dev, st_ino of its /proc/self/ns/pid), set by the loader only when the agent
/// runs in a nested one (kind, k3d, minikube): the kernel's IDs then differ from the ones its /proc shows.
#[unsafe(no_mangle)]
static PIDNS_DEV: u64 = 0;
#[unsafe(no_mangle)]
static PIDNS_INO: u64 = 0;

/// `bpf_get_current_pid_tgid` in the agent's PID namespace when it runs in a nested one. A task outside that
/// namespace keeps its root-namespace IDs. On a node's root namespace the branch is never taken.
#[inline(always)]
pub fn current_pid_tgid() -> u64 {
    // Read first so every path has a value: with an early return per branch, LLVM left a register unset on
    // the translated path and the verifier rejected the program (R9 !read_ok).
    let raw = bpf_get_current_pid_tgid();
    // SAFETY: u64 statics; the volatile reads keep the loader's values from being folded to 0.
    let ino = unsafe { core::ptr::read_volatile(&PIDNS_INO) };
    if ino == 0 {
        return raw;
    }
    // SAFETY: as above, for the namespace's st_dev.
    let dev = unsafe { core::ptr::read_volatile(&PIDNS_DEV) };
    let mut ns = bpf_pidns_info { pid: 0, tgid: 0 };
    let size = core::mem::size_of::<bpf_pidns_info>() as u32;
    // SAFETY: `ns` is a stack bpf_pidns_info of `size` bytes the helper writes.
    match unsafe { bpf_get_ns_current_pid_tgid(dev, ino, &mut ns, size) } {
        0 => (u64::from(ns.tgid) << 32) | u64::from(ns.pid),
        _ => raw,
    }
}

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
            m.pid = (current_pid_tgid() >> 32) as u32;
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

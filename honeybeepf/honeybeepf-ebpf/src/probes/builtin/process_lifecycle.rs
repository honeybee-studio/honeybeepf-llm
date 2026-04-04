use crate::vmlinux::{oom_control, task_struct};
use aya_ebpf::{
    helpers::{
        bpf_get_current_cgroup_id, bpf_get_current_comm, bpf_get_current_pid_tgid,
        bpf_ktime_get_ns, bpf_probe_read_kernel,
    },
    macros::{kprobe, map},
    maps::{HashMap, RingBuf},
    programs::ProbeContext,
};
use honeybeepf_common::ProcessExitEvent;

// Pull shared maps from exec_watch
use crate::probes::builtin::exec_watch::{PROCESS_NAMES, PROCESS_START_TIMES};

#[map]
pub static OOM_VICTIMS: HashMap<u32, u8> = HashMap::with_max_entries(1024, 0);

#[map]
pub static PROCESS_EXIT_EVENTS: RingBuf = RingBuf::with_byte_size(1024 * 1024, 0);

#[kprobe]
pub fn honeybeepf_oom_kill_process(ctx: ProbeContext) -> u32 {
    // oom_kill_process(struct oom_control *oc, ...) — arg(0) is a pointer, not a PID.
    // Follow: oom_control.chosen -> task_struct.tgid to get the victim PID.
    let oc_ptr = ctx
        .arg::<*const oom_control>(0)
        .unwrap_or(core::ptr::null());
    if oc_ptr.is_null() {
        return 0;
    }
    let chosen_ptr = unsafe {
        let field_ptr = core::ptr::addr_of!((*oc_ptr).chosen);
        match bpf_probe_read_kernel(field_ptr) {
            Ok(chosen) => chosen as *const task_struct,
            Err(_) => return 0,
        }
    };
    if chosen_ptr.is_null() {
        return 0;
    }
    let victim_pid = unsafe {
        let field_ptr = core::ptr::addr_of!((*chosen_ptr).tgid);
        match bpf_probe_read_kernel(field_ptr) {
            Ok(tgid) => tgid as u32,
            Err(_) => return 0,
        }
    };
    if victim_pid != 0 {
        let val: u8 = 1;
        let _ = OOM_VICTIMS.insert(&victim_pid, &val, 0);
    }
    0
}

#[kprobe]
pub fn honeybeepf_process_exit(ctx: ProbeContext) -> u32 {
    if let Some(mut slot) = PROCESS_EXIT_EVENTS.reserve::<ProcessExitEvent>(0) {
        let event = unsafe {
            let ptr = slot.as_mut_ptr();
            // Zero-initialize the entire event to avoid leaking uninitialized kernel memory
            core::ptr::write_bytes(ptr as *mut u8, 0, core::mem::size_of::<ProcessExitEvent>());
            &mut *ptr
        };
        let pid = (bpf_get_current_pid_tgid() >> 32) as u32;

        event.pid = pid;
        event.exit_code = ctx.arg::<i32>(0).unwrap_or(0);
        let signal = event.exit_code & 0x7f;

        // Check for OOM flag set by oom_kill_process probe
        if signal == 9 && unsafe { OOM_VICTIMS.get(&pid).is_some() } {
            // Mark as OOM victim so userspace can distinguish from MANUAL_SIGKILL
            event.ppid = 0xDEAD;
            let _ = OOM_VICTIMS.remove(&pid);
        } else {
            event.ppid = 0;
        }

        // Try to fetch the cached name from the exec phase first
        event.comm = unsafe {
            match PROCESS_NAMES.get(&pid) {
                Some(cached_name) => {
                    let name = *cached_name;
                    let _ = PROCESS_NAMES.remove(&pid);
                    name
                }
                None => bpf_get_current_comm().unwrap_or([0u8; 16]),
            }
        };

        // Calculate lifetime: fall back to 0 if process started before agent
        event.start_time = unsafe {
            let start = PROCESS_START_TIMES.get(&pid).copied().unwrap_or(0);
            let _ = PROCESS_START_TIMES.remove(&pid);
            start
        };

        event.exit_time = unsafe { bpf_ktime_get_ns() };
        event.cgroup_id = unsafe { bpf_get_current_cgroup_id() };

        slot.submit(0);
    }
    0
}

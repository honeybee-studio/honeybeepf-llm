use aya_ebpf::{
    helpers::{
        bpf_get_current_cgroup_id, bpf_get_current_comm, bpf_get_current_pid_tgid, bpf_ktime_get_ns,
    },
    macros::{kprobe, map, tracepoint},
    maps::{LruHashMap, RingBuf},
    programs::{ProbeContext, TracePointContext},
};
use honeybeepf_llm_common::ProcessExitEvent;

// Pull shared maps from exec_watch
use crate::probes::builtin::exec_watch::{PROCESS_NAMES, PROCESS_START_TIMES};

/// Pids from oom:mark_victim; LRU because a mark that lands after its task exited is never consumed.
#[map]
pub static OOM_VICTIMS: LruHashMap<u32, u8> = LruHashMap::with_max_entries(1024, 0);

#[map]
pub static PROCESS_EXIT_EVENTS: RingBuf = RingBuf::with_byte_size(1024 * 1024, 0);

/// Tgids that entered do_group_exit; kernel threads never do, so they are not reported.
/// LRU because an exec from a multi-threaded process marks a group that does not exit yet.
#[map]
pub static GROUP_EXITING: LruHashMap<u32, u8> = LruHashMap::with_max_entries(10240, 0);

/// `pid` follows the 8-byte common header in oom:mark_victim on 4.18 through 7.0.
const MARK_VICTIM_PID_OFFSET: usize = 8;

#[tracepoint]
pub fn honeybeepf_llm_oom_mark_victim(ctx: TracePointContext) -> u32 {
    // SAFETY: the tracepoint record always holds an int at MARK_VICTIM_PID_OFFSET.
    let Ok(victim_pid) = (unsafe { ctx.read_at::<i32>(MARK_VICTIM_PID_OFFSET) }) else {
        return 0;
    };
    if victim_pid > 0 {
        let val: u8 = 1;
        let _ = OOM_VICTIMS.insert(&(victim_pid as u32), &val, 0);
    }
    0
}

#[kprobe]
pub fn honeybeepf_llm_group_exit(_ctx: ProbeContext) -> u32 {
    let tgid = (bpf_get_current_pid_tgid() >> 32) as u32;
    let _ = GROUP_EXITING.insert(&tgid, &1, 0);
    0
}

#[kprobe]
pub fn honeybeepf_llm_process_exit(ctx: ProbeContext) -> u32 {
    let pid_tgid = bpf_get_current_pid_tgid();
    let pid = (pid_tgid >> 32) as u32;
    let tid = pid_tgid as u32;
    // mark_victim may name a non-leader thread; carry its mark over to the group.
    if tid != pid && OOM_VICTIMS.get_ptr(&tid).is_some() {
        let _ = OOM_VICTIMS.remove(&tid);
        let _ = OOM_VICTIMS.insert(&pid, &1, 0);
    }
    // acct_collect(long exitcode, int group_dead) runs for every thread; only the
    // group's last thread has group_dead set, and its exitcode is the group's code.
    if ctx.arg::<i32>(1).unwrap_or(0) == 0 {
        return 0;
    }
    // Consumed on every group exit, so a mark never outlives its process.
    let oom_marked = OOM_VICTIMS.get_ptr(&pid).is_some();
    let _ = OOM_VICTIMS.remove(&pid);
    if GROUP_EXITING.get_ptr(&pid).is_none() {
        return 0;
    }
    let _ = GROUP_EXITING.remove(&pid);

    if let Some(mut slot) = PROCESS_EXIT_EVENTS.reserve::<ProcessExitEvent>(0) {
        let event = unsafe {
            let ptr = slot.as_mut_ptr();
            // Zero-initialize the entire event to avoid leaking uninitialized kernel memory
            core::ptr::write_bytes(ptr as *mut u8, 0, core::mem::size_of::<ProcessExitEvent>());
            &mut *ptr
        };

        event.pid = pid;
        event.exit_code = ctx.arg::<i64>(0).unwrap_or(0) as i32;
        let signal = event.exit_code & 0x7f;

        // Check for OOM flag set by the oom:mark_victim tracepoint
        if signal == 9 && oom_marked {
            // Mark as OOM victim so userspace can distinguish from MANUAL_SIGKILL
            event.ppid = 0xDEAD;
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

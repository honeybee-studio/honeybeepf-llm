//! Exec watch tracepoint for LLM probe discovery.
//!
//! Notifies userspace when new processes exec, allowing dynamic
//! attachment of SSL probes to newly started processes.

use aya_ebpf::{
    helpers::{
        bpf_get_current_cgroup_id, bpf_get_current_comm, bpf_get_current_pid_tgid,
        bpf_ktime_get_ns, bpf_probe_read_kernel, bpf_probe_read_user_str_bytes,
    },
    macros::{kprobe, map, tracepoint},
    maps::{HashMap, LruHashMap, RingBuf},
    programs::{ProbeContext, TracePointContext},
};
use honeybeepf_llm_common::{ExecEvent, LibOpenEvent, MAX_FILENAME_LEN};

use super::llm::maps::EXEC_RINGBUF_SIZE;

const LIBSSL_PREFIX: &[u8] = b"libssl.so";
/// A handled (scope, path) is reported again after this, so a replaced file at the same path is seen.
const LIB_OPEN_TTL_NS: u64 = 60_000_000_000;
const O_ACCMODE: u64 = 3;

#[map]
pub static EXEC_EVENTS: RingBuf = RingBuf::with_byte_size(EXEC_RINGBUF_SIZE, 0);

#[map]
pub static LIB_OPEN_EVENTS: RingBuf = RingBuf::with_byte_size(EXEC_RINGBUF_SIZE, 0);

/// [scope, path hash] -> when userspace handled it (only userspace writes, so a failed attempt is
/// retried on the next open). Scope is the cgroup, or the process on cgroup v1.
#[map]
pub static LIB_OPEN_SEEN: LruHashMap<[u64; 2], u64> = LruHashMap::with_max_entries(4096, 0);

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

/// kprobe on do_sys_openat2(dfd, filename, how): reports absolute read-only `.../libssl.so*` opens.
#[kprobe]
pub fn probe_lib_open(ctx: ProbeContext) -> u32 {
    let ptr: *const u8 = match ctx.arg(1) {
        Some(p) => p,
        None => return 0,
    };
    if ptr.is_null() {
        return 0;
    }
    let mut buf = [0u8; MAX_FILENAME_LEN];
    // SAFETY: the helper copies at most buf.len() bytes and fails cleanly on a bad user pointer.
    let len = match unsafe { bpf_probe_read_user_str_bytes(ptr, &mut buf) } {
        Ok(path) => path.len(),
        Err(_) => return 0,
    };
    if buf[0] != b'/' {
        return 0;
    }

    // Scan back to the last '/' and stop there: tracking it in a forward loop forks a verifier
    // state per slash. Indexes are masked because the verifier loses LLVM's len-based bounds.
    const MASK: usize = MAX_FILENAME_LEN - 1;
    let mut start = 0usize;
    let mut n = 0usize;
    while n < MAX_FILENAME_LEN && n < len {
        let idx = (len - 1 - n) & MASK;
        if buf[idx] == b'/' {
            start = idx + 1;
            break;
        }
        n += 1;
    }
    let mut j = 0usize;
    while j < LIBSSL_PREFIX.len() {
        let k = start + j;
        if k >= MAX_FILENAME_LEN || buf[k & MASK] != LIBSSL_PREFIX[j] {
            return 0;
        }
        j += 1;
    }

    // Writes (package upgrades: libssl.so.3.dpkg-new) would be attached half-written and never retried.
    let how: *const u64 = match ctx.arg(2) {
        Some(p) => p,
        None => return 0,
    };
    // SAFETY: `how` is the kernel's struct open_how; flags is its first u64 (UAPI layout).
    match unsafe { bpf_probe_read_kernel(how) } {
        Ok(flags) if flags & O_ACCMODE == 0 => {}
        _ => return 0,
    }

    let mut hash: u64 = 0xcbf29ce484222325;
    let mut i = 0usize;
    while i < MAX_FILENAME_LEN && i < len {
        hash ^= buf[i] as u64;
        hash = hash.wrapping_mul(0x100000001b3);
        i += 1;
    }

    let pid = (bpf_get_current_pid_tgid() >> 32) as u32;
    // cgroup v1 reports one id for every task, so key by process there.
    // SAFETY: argument-free helper, valid in kprobe context.
    let cgroup = unsafe { bpf_get_current_cgroup_id() };
    let scope = if cgroup > 1 {
        cgroup
    } else {
        (1 << 63) | pid as u64
    };
    let key = [scope, hash];
    // SAFETY: argument-free helper, valid in kprobe context.
    let now = unsafe { bpf_ktime_get_ns() };
    // SAFETY: the value is a plain u64 that is only read here; a concurrent update is harmless.
    if let Some(last) = unsafe { LIB_OPEN_SEEN.get(&key) } {
        if now.wrapping_sub(*last) < LIB_OPEN_TTL_NS {
            return 0;
        }
    }

    if let Some(mut slot) = LIB_OPEN_EVENTS.reserve::<LibOpenEvent>(0) {
        // SAFETY: the slot is sized for LibOpenEvent and every field is written before submit.
        let event = unsafe { &mut *slot.as_mut_ptr() };
        event.pid = pid;
        event.len = len as u32;
        event.seen_key = key;
        event.ts = now;
        event.path = buf;
        slot.submit(0);
    }
    0
}

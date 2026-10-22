//! Exec watch tracepoint for LLM probe discovery.
//!
//! Notifies userspace when new processes exec, allowing dynamic
//! attachment of SSL probes to newly started processes.

use aya_ebpf::{
    helpers::{
        bpf_get_current_cgroup_id, bpf_get_current_comm, bpf_get_current_pid_tgid,
        bpf_ktime_get_ns, bpf_probe_read_kernel, bpf_probe_read_kernel_buf,
        bpf_probe_read_user_str_bytes,
    },
    macros::{fentry, kprobe, map, tracepoint},
    maps::{Array, HashMap, LruHashMap, PerCpuArray, RingBuf},
    programs::{FEntryContext, ProbeContext, TracePointContext},
};
use honeybeepf_llm_common::{ExecEvent, LibOpenEvent, MAX_FILENAME_LEN};

use super::llm::maps::EXEC_RINGBUF_SIZE;

/// Path bytes searched for the basename, counted from the end; libssl basenames are far shorter.
const TAIL_LEN: usize = 32;
/// "/libssl.so" as the last 10 bytes read: first two in `hi`, last eight in `lo`.
const LIBSSL_WIN_HI: u64 = u16::from_be_bytes(*b"/l") as u64;
const LIBSSL_WIN_LO: u64 = u64::from_be_bytes(*b"ibssl.so");
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

#[repr(C)]
struct LibOpenScratch {
    path: [u8; MAX_FILENAME_LEN],
    /// Copied from the end of `path` by a helper: the verifier rejects the variable offset as an index.
    tail: [u8; TAIL_LEN],
}

/// Off the stack: a zeroed stack buffer is a memset loop for the verifier.
#[map]
static LIB_OPEN_PATH: PerCpuArray<LibOpenScratch> = PerCpuArray::with_max_entries(1, 0);

/// Never written: clears LIB_OPEN_PATH with one helper copy.
#[map]
static LIB_OPEN_ZERO: Array<[u8; MAX_FILENAME_LEN]> = Array::with_max_entries(1, 0);

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

/// kprobe on do_sys_openat2(dfd, filename, how), used where the fentry below can't attach.
#[kprobe]
pub fn probe_lib_open(ctx: ProbeContext) -> u32 {
    match (ctx.arg(1), ctx.arg(2)) {
        (Some(ptr), Some(how)) => lib_open(ptr, how),
        _ => 0,
    }
}

/// fentry on do_sys_openat2: no kprobe trap, and the args come straight from the context.
#[fentry(function = "do_sys_openat2")]
pub fn probe_lib_open_fentry(ctx: FEntryContext) -> u32 {
    // SAFETY: do_sys_openat2(int dfd, const char __user *filename, struct open_how *how).
    let (ptr, how) = unsafe { (ctx.arg(1), ctx.arg(2)) };
    lib_open(ptr, how)
}

/// Reports absolute read-only `.../libssl.so*` opens.
#[inline(always)]
fn lib_open(ptr: *const u8, how: *const u64) -> u32 {
    if ptr.is_null() {
        return 0;
    }

    // Writes (package upgrades: libssl.so.3.dpkg-new) would be attached half-written and never retried.
    // SAFETY: `how` is the kernel's struct open_how; flags is its first u64 (UAPI layout).
    match unsafe { bpf_probe_read_kernel(how) } {
        Ok(flags) if flags & O_ACCMODE == 0 => {}
        _ => return 0,
    }

    // Loops below run a fixed count over constant indexes. Loops that stop at `len` or index by a
    // found position made the verifier explore one state per value and hit its 1M-insn limit.
    let LibOpenScratch { path, tail } = match LIB_OPEN_PATH.get_ptr_mut(0) {
        // SAFETY: this CPU's slot, used only by this program while it runs (it doesn't nest).
        Some(p) => unsafe { &mut *p },
        None => return 0,
    };
    // SAFETY: the helper copies at most path.len() bytes and fails cleanly on a bad user pointer.
    let len = match unsafe { bpf_probe_read_user_str_bytes(ptr, path) } {
        Ok(s) => s.len(),
        Err(_) => return 0,
    };
    if path[0] != b'/' {
        return 0;
    }

    let start = len.saturating_sub(TAIL_LEN);
    if start > MAX_FILENAME_LEN - TAIL_LEN {
        return 0;
    }
    // SAFETY: start + TAIL_LEN <= MAX_FILENAME_LEN, so the source stays inside `path`.
    if unsafe { bpf_probe_read_kernel_buf(path.as_ptr().add(start), tail) }.is_err() {
        return 0;
    }
    // "/libssl.so" in a rolling 10-byte window; a later '/' means it was a directory name. Bytes
    // after the NUL are left from an earlier, longer path and must not count.
    let mut hi = 0u64;
    let mut lo = 0u64;
    let mut libssl = false;
    let mut ended = false;
    let mut i = 0usize;
    while i < TAIL_LEN {
        let c = tail[i];
        ended = ended || c == 0;
        hi = ((hi << 8) | (lo >> 56)) & 0xffff;
        lo = (lo << 8) | c as u64;
        if !ended {
            libssl = (hi == LIBSSL_WIN_HI && lo == LIBSSL_WIN_LO) || (libssl && c != b'/');
        }
        i += 1;
    }
    if !libssl {
        return 0;
    }

    // Stale bytes after the NUL must not reach the hash either. Clearing costs a full-buffer copy,
    // so it is done only here, for the rare libssl open, and the path is read again.
    let zero = match LIB_OPEN_ZERO.get(0) {
        Some(z) => z,
        None => return 0,
    };
    // SAFETY: both are MAX_FILENAME_LEN-byte map values.
    if unsafe { bpf_probe_read_kernel_buf(zero.as_ptr(), path) }.is_err() {
        return 0;
    }
    // SAFETY: as the first read.
    let len = match unsafe { bpf_probe_read_user_str_bytes(ptr, path) } {
        Ok(s) => s.len(),
        Err(_) => return 0,
    };

    let mut hash: u64 = 0xcbf29ce484222325;
    let mut i = 0usize;
    while i < MAX_FILENAME_LEN {
        hash = (hash ^ path[i] as u64).wrapping_mul(0x100000001b3);
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
        // A helper copy: a plain assignment compiles to a byte loop the verifier walks per state.
        // SAFETY: both buffers are MAX_FILENAME_LEN bytes of kernel memory owned by this program.
        if unsafe { bpf_probe_read_kernel_buf(path.as_ptr(), &mut event.path) }.is_err() {
            slot.discard(0);
            return 0;
        }
        slot.submit(0);
    }
    0
}

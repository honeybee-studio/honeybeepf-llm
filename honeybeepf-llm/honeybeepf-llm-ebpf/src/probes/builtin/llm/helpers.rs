use aya_ebpf::{
    helpers::{
        bpf_get_current_comm, bpf_get_current_pid_tgid, bpf_ktime_get_ns, bpf_probe_read_user,
        bpf_probe_read_user_buf,
    },
    programs::RetProbeContext,
};
use honeybeepf_llm_common::{
    EventMetadata, LlmDirection, LlmEvent, MAX_SSL_BUF_SIZE, SSL_CLASS_MASK, SSL_CLASS_READ_SEEN,
    SSL_CLASS_SEND, capture_limit, ssl_class_after_write, ssl_class_drops,
};

use crate::probes::builtin::llm::maps::{BUFS, LLM_STATS, READBYTES_PTRS, SSL_CLASS, START_NS};

#[inline(always)]
pub fn get_current_tid() -> u32 {
    bpf_get_current_pid_tgid() as u32
}

/// Increment one `LLM_STAT_*` counter for the current CPU (approximate: not atomic).
#[inline(never)]
pub fn count(stat: u32) {
    if let Some(p) = LLM_STATS.get_ptr_mut(stat) {
        // SAFETY: `p` is this CPU's slot of the per-CPU value, valid while the program runs.
        unsafe { *p += 1 };
    }
}

pub struct Session;

impl Session {
    #[inline(always)]
    pub fn start(tid: u32, ssl: u64, buf_addr: u64, len_ptr: Option<u64>) {
        let ts = unsafe { bpf_ktime_get_ns() };
        let _ = START_NS.insert(&tid, &ts, 0);
        let _ = BUFS.insert(&tid, &[buf_addr, ssl], 0);
        if let Some(lp) = len_ptr {
            let _ = READBYTES_PTRS.insert(&tid, &lp, 0);
        }
    }

    /// (start ns, buf, SSL*, readbytes ptr) saved by the entry probe.
    #[inline(always)]
    pub fn get_info(tid: u32) -> (u64, u64, u64, Option<u64>) {
        unsafe {
            let ts = START_NS.get(&tid).copied().unwrap_or(0);
            let [buf, ssl] = BUFS.get(&tid).copied().unwrap_or([0, 0]);
            let len_ptr = READBYTES_PTRS.get(&tid).copied();
            (ts, buf, ssl, len_ptr)
        }
    }

    #[inline(always)]
    pub fn clear(tid: u32) {
        let _ = START_NS.remove(&tid);
        let _ = BUFS.remove(&tid);
        let _ = READBYTES_PTRS.remove(&tid);
    }
}

/// Drop the connection's class, since its SSL* address can be reused by the next connection.
#[inline(always)]
pub fn forget_class(ssl: u64) {
    let tgid = bpf_get_current_pid_tgid() >> 32;
    let _ = SSL_CLASS.remove(&[tgid, ssl]);
}

/// Metadata for an event of the current task (LlmEvent doesn't use the HoneyBeeEvent trait).
#[inline(always)]
pub fn current_metadata() -> EventMetadata {
    let pid_tgid = bpf_get_current_pid_tgid();
    EventMetadata {
        pid: (pid_tgid >> 32) as u32,
        // EventMetadata is shared across all event types, so the tid goes in the padding field.
        _pad: pid_tgid as u32,
        cgroup_id: unsafe { aya_ebpf::helpers::bpf_get_current_cgroup_id() },
        timestamp: unsafe { bpf_ktime_get_ns() },
    }
}

/// One finished SSL call, read once and stamped on each fragment it is sent in.
pub struct SslCall {
    metadata: EventMetadata,
    latency_ns: u64,
    comm: [u8; 16],
    buf_addr: u64,
    ssl: u64,
    pub len: u32,
    closed: u8,
}

impl SslCall {
    /// SSL_free of `ssl`: no data, it only ends the connection's stream in userspace.
    #[inline(always)]
    pub fn free(ssl: u64) -> Self {
        Self {
            metadata: current_metadata(),
            latency_ns: 0,
            comm: [0; 16],
            buf_addr: 0,
            ssl,
            len: 0,
            closed: 1,
        }
    }

    #[inline(always)]
    pub fn capture(ctx: &RetProbeContext, is_handshake: bool) -> Result<Self, u32> {
        let tid = get_current_tid();
        let (start_ts, buf_addr, ssl, len_ptr) = Session::get_info(tid);

        let metadata = current_metadata();
        let timestamp = metadata.timestamp;
        let latency_ns = if start_ts > 0 {
            timestamp - start_ts
        } else {
            0
        };

        // C `int` return: RAX's upper half isn't sign-extended, so as i64 a -1 reads as 4294967295.
        let ret: i32 = ctx.ret().ok_or(1u32)?;
        // Filter out errors (ret < 0) and zero-byte results (ret == 0).
        // For SSL_read, ret == 0 indicates clean shutdown - no data to capture.
        // For LLM monitoring, we only care about actual data transfers.
        if ret <= 0 {
            return Err(1);
        }
        // No session: the entry probe missed this call or another return probe consumed it.
        if buf_addr == 0 && !is_handshake {
            return Err(1);
        }

        let len = if let Some(lp) = len_ptr {
            let actual_len: usize =
                unsafe { bpf_probe_read_user(lp as *const usize) }.map_err(|_| 1u32)?;
            actual_len as u32
        } else {
            ret as u32
        };

        // Session cleanup is handled by the caller (emit_llm_event) for consistency
        Ok(Self {
            metadata,
            latency_ns,
            comm: bpf_get_current_comm().unwrap_or([0; 16]),
            buf_addr,
            ssl,
            len,
            closed: 0,
        })
    }

    /// Whether the connection's class drops this call; a Write first reclasses it by its first 8 bytes.
    #[inline(always)]
    pub fn filtered(&self, rw: u8) -> bool {
        let key = [self.metadata.pid as u64, self.ssl];
        // SAFETY: the value is copied out at once, before an update or eviction can free it.
        let entry = unsafe { SSL_CLASS.get(&key) }.copied();
        let class = entry.map_or(0, |v| (v & SSL_CLASS_MASK) as u8);
        let born = entry.unwrap_or(self.metadata.timestamp) & !SSL_CLASS_MASK;
        let next = if rw == LlmDirection::Write as u8 {
            let head = if self.len >= 8 {
                // SAFETY: the helper checks the user address; a fault returns Err, read as no head.
                unsafe { bpf_probe_read_user(self.buf_addr as *const [u8; 8]) }.unwrap_or([0; 8])
            } else {
                [0; 8]
            };
            ssl_class_after_write(class, head, self.len)
        } else if class == SSL_CLASS_SEND {
            SSL_CLASS_SEND | SSL_CLASS_READ_SEEN
        } else {
            class
        };
        if next != class {
            let _ = SSL_CLASS.insert(&key, &(born | next as u64), 0);
        }
        ssl_class_drops(next)
    }
}

pub trait LlmEventExt {
    fn fill(&mut self, call: &SslCall, rw: u8, is_handshake: bool, offset: u32) -> Result<(), u32>;
}

impl LlmEventExt for LlmEvent {
    #[inline(always)]
    fn fill(&mut self, call: &SslCall, rw: u8, is_handshake: bool, offset: u32) -> Result<(), u32> {
        self.metadata = call.metadata;
        self.rw = rw;
        self.is_handshake = if is_handshake { 1 } else { 0 };
        self.truncated = (call.len as usize > capture_limit(LlmDirection::from(rw))) as u8;
        self.closed = call.closed;
        self.len = call.len;
        self.offset = offset;
        self.ssl_ptr = call.ssl;
        self.latency_ns = call.latency_ns;
        self.comm = call.comm;

        if call.buf_addr > 0 {
            let to_read =
                core::cmp::min(call.len.saturating_sub(offset) as usize, MAX_SSL_BUF_SIZE);
            let src = (call.buf_addr + offset as u64) as *const u8;
            // Read directly into self.buf to avoid 4KB stack allocation
            if unsafe { bpf_probe_read_user_buf(src, &mut self.buf[..to_read]) }.is_err() {
                self.buf_filled = 0;
                return Err(1);
            }
            self.buf_filled = 1;
        } else {
            self.buf_filled = 0;
        }
        Ok(())
    }
}

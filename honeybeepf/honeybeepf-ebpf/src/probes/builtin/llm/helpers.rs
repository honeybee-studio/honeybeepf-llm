use aya_ebpf::{
    helpers::{
        bpf_get_current_comm, bpf_get_current_pid_tgid, bpf_ktime_get_ns, bpf_probe_read_user,
        bpf_probe_read_user_buf,
    },
    programs::RetProbeContext,
};
use honeybeepf_common::{LlmEvent, MAX_SSL_BUF_SIZE};

use crate::probes::builtin::llm::maps::{BUFS, READBYTES_PTRS, START_NS};

#[inline(always)]
pub fn get_current_tid() -> u32 {
    bpf_get_current_pid_tgid() as u32
}

pub struct Session;

impl Session {
    #[inline(always)]
    pub fn start(tid: u32, buf_addr: u64, len_ptr: Option<u64>) {
        let ts = unsafe { bpf_ktime_get_ns() };
        let _ = START_NS.insert(&tid, &ts, 0);
        let _ = BUFS.insert(&tid, &buf_addr, 0);
        if let Some(lp) = len_ptr {
            let _ = READBYTES_PTRS.insert(&tid, &lp, 0);
        }
    }

    #[inline(always)]
    pub fn get_info(tid: u32) -> (u64, u64, Option<u64>) {
        unsafe {
            let ts = START_NS.get(&tid).copied().unwrap_or(0);
            let buf = BUFS.get(&tid).copied().unwrap_or(0);
            let len_ptr = READBYTES_PTRS.get(&tid).copied();
            (ts, buf, len_ptr)
        }
    }

    #[inline(always)]
    pub fn clear(tid: u32) {
        let _ = START_NS.remove(&tid);
        let _ = BUFS.remove(&tid);
        let _ = READBYTES_PTRS.remove(&tid);
    }
}

pub trait LlmEventExt {
    fn capture_data(
        &mut self,
        ctx: &RetProbeContext,
        rw: u8,
        is_handshake: bool,
    ) -> Result<(), u32>;
}

impl LlmEventExt for LlmEvent {
    #[inline(always)]
    fn capture_data(
        &mut self,
        ctx: &RetProbeContext,
        rw: u8,
        is_handshake: bool,
    ) -> Result<(), u32> {
        let tid = get_current_tid();
        let (start_ts, buf_addr, len_ptr) = Session::get_info(tid);

        // Initialize metadata inline. LlmEvent uses a custom capture pattern with RetProbeContext
        // rather than the HoneyBeeEvent trait (which uses TracePointContext/ProbeContext).
        let pid_tgid = bpf_get_current_pid_tgid();
        self.metadata.pid = (pid_tgid >> 32) as u32;
        // Store tid in _pad for userspace per-thread stream correlation. EventMetadata is shared
        // across all event types, so we reuse the padding field rather than adding a new field.
        self.metadata._pad = pid_tgid as u32;
        self.metadata.timestamp = unsafe { bpf_ktime_get_ns() };
        self.metadata.cgroup_id = unsafe { aya_ebpf::helpers::bpf_get_current_cgroup_id() };

        self.rw = rw;
        self.is_handshake = if is_handshake { 1 } else { 0 };
        self.latency_ns = if start_ts > 0 {
            self.metadata.timestamp - start_ts
        } else {
            0
        };

        let ret: i64 = ctx.ret().ok_or(1u32)?;
        // Filter out errors (ret < 0) and zero-byte results (ret == 0).
        // For SSL_read, ret == 0 indicates clean shutdown - no data to capture.
        // For LLM monitoring, we only care about actual data transfers.
        if ret <= 0 {
            return Err(1);
        }

        self.len = if let Some(lp) = len_ptr {
            let actual_len: usize =
                unsafe { bpf_probe_read_user(lp as *const usize) }.map_err(|_| 1u32)?;
            actual_len as u32
        } else {
            ret as u32
        };

        if buf_addr > 0 {
            let to_read = core::cmp::min(self.len as usize, MAX_SSL_BUF_SIZE);
            // Read directly into self.buf to avoid 4KB stack allocation
            let result =
                unsafe { bpf_probe_read_user_buf(buf_addr as *const u8, &mut self.buf[..to_read]) };
            if result.is_ok() {
                self.buf_filled = 1;
            } else {
                self.buf_filled = 0;
                return Err(1);
            }
        } else {
            self.buf_filled = 0;
        }

        self.comm = bpf_get_current_comm().unwrap_or([0; 16]);

        // Session cleanup is handled by the caller (emit_llm_event) for consistency
        Ok(())
    }
}

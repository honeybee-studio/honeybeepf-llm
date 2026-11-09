//! eBPF probes for SSL/TLS interception to capture LLM API traffic.
//!
//! Attaches uprobes to OpenSSL's SSL_read/SSL_write functions to capture
//! encrypted traffic after decryption, enabling LLM request/response monitoring.
//!
//! # Probe Mapping
//! - `probe_ssl_rw_enter` → Entry for `SSL_read(ssl, buf, num)` and `SSL_write(ssl, buf, num)`
//! - `probe_ssl_read_exit` → Return from `SSL_read`
//! - `probe_ssl_write_exit` → Return from `SSL_write`
//! - `probe_ssl_rw_ex_enter` → Entry for `SSL_read_ex(ssl, buf, num, *readbytes)` and `SSL_write_ex`
//! - `probe_ssl_read_ex_exit` → Return from `SSL_read_ex`
//! - `probe_ssl_write_ex_exit` → Return from `SSL_write_ex`
//! - `probe_ssl_do_handshake_enter/exit` → `SSL_do_handshake` for latency measurement
//! - `probe_ssl_free` → Entry for `SSL_free(ssl)`, ends the connection's stream
//! - `probe_ssl_task_exit` → `sched:sched_process_exit`, clears the exiting thread's call state

use aya_ebpf::{
    EbpfContext,
    helpers::{bpf_probe_read_user_buf, r#gen},
    macros::{tracepoint, uprobe, uretprobe},
    programs::{ProbeContext, RetProbeContext, TracePointContext},
};
use honeybeepf_llm_common::{
    LLM_STAT_DISCARD, LLM_STAT_EMPTY, LLM_STAT_ENTER, LLM_STAT_FILTERED, LLM_STAT_FRAGMENT,
    LLM_STAT_HANDSHAKE, LLM_STAT_RESERVE_FAIL, LLM_STAT_SUBMIT, LLM_STAT_TRUNCATED, LlmDirection,
    LlmEventHeader, MAX_SSL_BUF_SIZE, MAX_SSL_FRAGMENTS, capture_limit,
};

mod filter;
pub(crate) mod helpers;
pub mod maps;

use helpers::{Session, SslCall, count, fill_header, get_current_tid};
use maps::*;

/// Entry probe for SSL_read/SSL_write. Captures buffer pointer from arg1.
/// Session::start overwrites any existing entry, so no clear() needed.
#[uprobe]
pub fn probe_ssl_rw_enter(ctx: ProbeContext) -> u32 {
    count(LLM_STAT_ENTER);
    let tid = get_current_tid();
    Session::start(tid, ctx.arg(0).unwrap_or(0), ctx.arg(1).unwrap_or(0), None);
    keep_task_stack(&ctx);
    0
}

/// Return probe for SSL_read - captures decrypted data read from the connection.
#[uretprobe]
pub fn probe_ssl_read_exit(ctx: RetProbeContext) -> u32 {
    emit_llm_event(&ctx, LlmDirection::Read as u8, false)
}

/// Return probe for SSL_write - captures data written to the connection.
#[uretprobe]
pub fn probe_ssl_write_exit(ctx: RetProbeContext) -> u32 {
    emit_llm_event(&ctx, LlmDirection::Write as u8, false)
}

/// Entry probe for SSL_read_ex/SSL_write_ex. These variants have a 4th arg
/// that points to the actual bytes read/written (instead of return value).
#[uprobe]
pub fn probe_ssl_rw_ex_enter(ctx: ProbeContext) -> u32 {
    count(LLM_STAT_ENTER);
    let tid = get_current_tid();
    Session::start(
        tid,
        ctx.arg(0).unwrap_or(0),
        ctx.arg(1).unwrap_or(0),
        Some(ctx.arg(3).unwrap_or(0)),
    );
    keep_task_stack(&ctx);
    0
}

/// Return probe for SSL_read_ex.
#[uretprobe]
pub fn probe_ssl_read_ex_exit(ctx: RetProbeContext) -> u32 {
    emit_llm_event(&ctx, LlmDirection::Read as u8, false)
}

/// Return probe for SSL_write_ex.
#[uretprobe]
pub fn probe_ssl_write_ex_exit(ctx: RetProbeContext) -> u32 {
    emit_llm_event(&ctx, LlmDirection::Write as u8, false)
}

/// Entry probe for SSL_do_handshake - captures start time for latency.
#[uprobe]
pub fn probe_ssl_do_handshake_enter(ctx: ProbeContext) -> u32 {
    count(LLM_STAT_ENTER);
    let tid = get_current_tid();
    let ssl = ctx.arg(0).unwrap_or(0);
    filter::forget(ssl);
    Session::start(tid, ssl, 0, None);
    keep_task_stack(&ctx);
    0
}

/// Return probe for SSL_do_handshake - emits handshake completion event.
#[uretprobe]
pub fn probe_ssl_do_handshake_exit(ctx: RetProbeContext) -> u32 {
    emit_llm_event(&ctx, LlmDirection::Handshake as u8, true)
}

/// Entry probe for SSL_free(ssl): tells userspace the connection's stream is over, since the
/// address can be reused by the next connection.
#[uprobe]
pub fn probe_ssl_free(ctx: ProbeContext) -> u32 {
    count(LLM_STAT_ENTER);
    match ctx.arg::<u64>(0) {
        Some(ssl) if ssl != 0 => {
            filter::forget(ssl);
            emit_fragments(&SslCall::free(ssl), LlmDirection::Unknown as u8, false)
        }
        _ => count(LLM_STAT_DISCARD),
    }
    keep_task_stack(&ctx);
    0
}

/// Runs for every exiting thread: one killed inside an SSL call never reaches the return probe,
/// and its entries would fill the session maps for good.
#[tracepoint]
pub fn probe_ssl_task_exit(_ctx: TracePointContext) -> u32 {
    Session::clear(get_current_tid());
    0
}

/// Emit an LLM event to the ring buffer. Always clears session state after processing.
#[inline(always)]
fn emit_llm_event(ctx: &RetProbeContext, rw: u8, is_handshake: bool) -> u32 {
    let tid = get_current_tid();
    match SslCall::capture(ctx, is_handshake) {
        Ok(call) if !is_handshake && filter::drops(&call, rw) => count(LLM_STAT_FILTERED),
        Ok(call) => emit_fragments(&call, rw, is_handshake),
        Err(_) => count(LLM_STAT_DISCARD),
    }
    // Always clear session state - handles success, failure, and reserve failure cases
    Session::clear(tid);
    keep_task_stack(ctx);
    0
}

/// Set by the loader where a tail call may share a program with bpf-to-bpf calls (x86-64 5.10+, 6.0+).
#[unsafe(no_mangle)]
static TAIL_CALL_OK: u8 = 0;

/// Tail call into the empty NO_PRIV_STACK so this preemptible program keeps its frame on the task stack.
#[inline(always)]
fn keep_task_stack<C: EbpfContext>(ctx: &C) {
    // The verifier skips the call when TAIL_CALL_OK is 0 but still sees it, which disables the private stack.
    // SAFETY: TAIL_CALL_OK is a u8 static; the volatile read keeps the loader's value from being folded to 0.
    let ok = unsafe { core::ptr::read_volatile(&TAIL_CALL_OK) };
    if ok == 0 {
        return;
    }
    // SAFETY: NO_PRIV_STACK holds no program, so the tail call fails and execution continues here.
    unsafe {
        r#gen::bpf_tail_call(
            ctx.as_ptr(),
            (&raw const NO_PRIV_STACK).cast_mut().cast(),
            0,
        );
    }
}

/// Payload buckets: each event reserves the smallest that holds its bytes. Largest is
/// `MAX_SSL_BUF_SIZE`; writes are capped at 4 KB and SSE read fragments are usually small, so a
/// short read or an empty event takes only a small ring slot.
const BUCKET_SMALL: usize = 512;
const BUCKET_MED: usize = 2048;

/// One ring record sized to a bucket: `header` sits at offset 0, `buf` holds `N` payload bytes.
#[repr(C)]
struct VarEvent<const N: usize> {
    header: LlmEventHeader,
    buf: [u8; N],
}

enum Emitted {
    Payload,
    Empty,
    NoRoom,
    ReadFail,
}

/// Reserve an `N`-byte-payload record, fill it, and submit; the size is constant for the verifier.
#[inline(always)]
fn emit_bucket<const N: usize>(
    call: &SslCall,
    rw: u8,
    is_handshake: bool,
    truncated: u8,
    offset: u32,
    to_read: usize,
) -> Emitted {
    let Some(mut slot) = SSL_EVENTS.reserve::<VarEvent<N>>(0) else {
        return Emitted::NoRoom;
    };
    // SAFETY: the slot is reserved for a VarEvent<N>; every header field is set before submit.
    let ev = unsafe { &mut *slot.as_mut_ptr() };
    fill_header(&mut ev.header, call, rw, is_handshake, truncated, offset);
    if N == 0 || call.buf_addr == 0 {
        ev.header.buf_filled = 0;
        slot.submit(0);
        return Emitted::Empty;
    }
    // The dispatch already keeps to_read <= N, but the compiler spills it and the verifier loses
    // that bound; a volatile reload makes the min(.., N) survive so the copy length stays <= N.
    // SAFETY: &to_read is a valid initialized local.
    let n = core::cmp::min(unsafe { core::ptr::read_volatile(&to_read) }, N);
    let src = (call.buf_addr + offset as u64) as *const u8;
    // SAFETY: src is the user buffer at this fragment's offset; n <= N keeps the ring write in bounds.
    if unsafe { bpf_probe_read_user_buf(src, &mut ev.buf[..n]) }.is_err() {
        slot.discard(0);
        return Emitted::ReadFail;
    }
    ev.header.buf_filled = 1;
    slot.submit(0);
    Emitted::Payload
}

/// Emit one fragment at `offset`, choosing the smallest bucket that fits its payload.
#[inline(never)]
fn emit_one(call: &SslCall, rw: u8, is_handshake: bool, truncated: u8, offset: u32) -> Emitted {
    let to_read = if call.buf_addr > 0 {
        core::cmp::min(call.len.saturating_sub(offset) as usize, MAX_SSL_BUF_SIZE)
    } else {
        0
    };
    if to_read == 0 {
        emit_bucket::<0>(call, rw, is_handshake, truncated, offset, 0)
    } else if to_read <= BUCKET_SMALL {
        emit_bucket::<BUCKET_SMALL>(call, rw, is_handshake, truncated, offset, to_read)
    } else if to_read <= BUCKET_MED {
        emit_bucket::<BUCKET_MED>(call, rw, is_handshake, truncated, offset, to_read)
    } else {
        emit_bucket::<MAX_SSL_BUF_SIZE>(call, rw, is_handshake, truncated, offset, to_read)
    }
}

/// Send the call's data as up to MAX_SSL_FRAGMENTS events, each sized to its payload.
#[inline(always)]
fn emit_fragments(call: &SslCall, rw: u8, is_handshake: bool) {
    let limit = capture_limit(LlmDirection::from(rw));
    let truncated = (call.len as usize > limit) as u8;
    // Calls over the limit were counted as truncated at fragment 0.
    let tail_lost = |i: usize| {
        if i > 0 && call.len as usize <= limit {
            count(LLM_STAT_TRUNCATED);
        }
    };
    for i in 0..MAX_SSL_FRAGMENTS {
        let offset = i * MAX_SSL_BUF_SIZE;
        if i > 0 && (offset >= call.len as usize || offset >= limit) {
            return;
        }
        let offset = offset as u32;
        match emit_one(call, rw, is_handshake, truncated, offset) {
            Emitted::NoRoom => {
                if i == 0 {
                    count(LLM_STAT_RESERVE_FAIL);
                }
                tail_lost(i);
                return;
            }
            Emitted::ReadFail => {
                if i == 0 {
                    count(LLM_STAT_DISCARD);
                }
                tail_lost(i);
                return;
            }
            outcome => {
                if i > 0 {
                    count(LLM_STAT_FRAGMENT);
                } else if is_handshake {
                    count(LLM_STAT_HANDSHAKE);
                } else if matches!(outcome, Emitted::Empty) {
                    count(LLM_STAT_EMPTY);
                } else if truncated != 0 {
                    count(LLM_STAT_TRUNCATED);
                }
                count(LLM_STAT_SUBMIT);
            }
        }
    }
}

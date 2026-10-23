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

use aya_ebpf::{
    macros::{uprobe, uretprobe},
    programs::{ProbeContext, RetProbeContext},
};
use honeybeepf_llm_common::{
    LLM_STAT_DISCARD, LLM_STAT_EMPTY, LLM_STAT_ENTER, LLM_STAT_FRAGMENT, LLM_STAT_HANDSHAKE,
    LLM_STAT_RESERVE_FAIL, LLM_STAT_SUBMIT, LLM_STAT_TRUNCATED, LlmDirection, LlmEvent,
    MAX_SSL_BUF_SIZE, MAX_SSL_FRAGMENTS, capture_limit,
};

mod helpers;
pub mod maps;

use helpers::{LlmEventExt, Session, SslCall, count, get_current_tid};
use maps::*;

/// Entry probe for SSL_read/SSL_write. Captures buffer pointer from arg1.
/// Session::start overwrites any existing entry, so no clear() needed.
#[uprobe]
pub fn probe_ssl_rw_enter(ctx: ProbeContext) -> u32 {
    count(LLM_STAT_ENTER);
    let tid = get_current_tid();
    Session::start(tid, ctx.arg(0).unwrap_or(0), ctx.arg(1).unwrap_or(0), None);
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
    Session::start(tid, ctx.arg(0).unwrap_or(0), 0, None);
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
            emit_fragments(&SslCall::free(ssl), LlmDirection::Unknown as u8, false)
        }
        _ => count(LLM_STAT_DISCARD),
    }
    0
}

/// Emit an LLM event to the ring buffer. Always clears session state after processing.
#[inline(always)]
fn emit_llm_event(ctx: &RetProbeContext, rw: u8, is_handshake: bool) -> u32 {
    let tid = get_current_tid();
    match SslCall::capture(ctx, is_handshake) {
        Ok(call) => emit_fragments(&call, rw, is_handshake),
        Err(_) => count(LLM_STAT_DISCARD),
    }
    // Always clear session state - handles success, failure, and reserve failure cases
    Session::clear(tid);
    0
}

/// Send the call's data as up to MAX_SSL_FRAGMENTS events of MAX_SSL_BUF_SIZE bytes each.
#[inline(always)]
fn emit_fragments(call: &SslCall, rw: u8, is_handshake: bool) {
    let limit = capture_limit(LlmDirection::from(rw));
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
        let Some(mut slot) = SSL_EVENTS.reserve::<LlmEvent>(0) else {
            if i == 0 {
                count(LLM_STAT_RESERVE_FAIL);
            }
            tail_lost(i);
            return;
        };
        let event = unsafe { &mut *slot.as_mut_ptr() };
        if event.fill(call, rw, is_handshake, offset).is_err() {
            if i == 0 {
                count(LLM_STAT_DISCARD);
            }
            tail_lost(i);
            slot.discard(0);
            return;
        }
        if i > 0 {
            count(LLM_STAT_FRAGMENT);
        } else if is_handshake {
            count(LLM_STAT_HANDSHAKE);
        } else if event.buf_filled == 0 {
            count(LLM_STAT_EMPTY);
        } else if event.truncated != 0 {
            count(LLM_STAT_TRUNCATED);
        }
        count(LLM_STAT_SUBMIT);
        slot.submit(0);
    }
}

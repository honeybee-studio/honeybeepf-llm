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

use aya_ebpf::{
    macros::{uprobe, uretprobe},
    programs::{ProbeContext, RetProbeContext},
};
use honeybeepf_common::{LlmDirection, LlmEvent};

mod helpers;
pub mod maps;

use helpers::{LlmEventExt, Session, get_current_tid};
use maps::*;

/// Entry probe for SSL_read/SSL_write. Captures buffer pointer from arg1.
/// Session::start overwrites any existing entry, so no clear() needed.
#[uprobe]
pub fn probe_ssl_rw_enter(ctx: ProbeContext) -> u32 {
    let tid = get_current_tid();
    Session::start(tid, ctx.arg(1).unwrap_or(0), None);
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
    let tid = get_current_tid();
    Session::start(tid, ctx.arg(1).unwrap_or(0), Some(ctx.arg(3).unwrap_or(0)));
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
pub fn probe_ssl_do_handshake_enter(_ctx: ProbeContext) -> u32 {
    let tid = get_current_tid();
    Session::start(tid, 0, None);
    0
}

/// Return probe for SSL_do_handshake - emits handshake completion event.
#[uretprobe]
pub fn probe_ssl_do_handshake_exit(ctx: RetProbeContext) -> u32 {
    emit_llm_event(&ctx, LlmDirection::Handshake as u8, true)
}

/// Emit an LLM event to the ring buffer. Always clears session state after processing.
#[inline(always)]
fn emit_llm_event(ctx: &RetProbeContext, rw: u8, is_handshake: bool) -> u32 {
    let tid = get_current_tid();
    if let Some(mut slot) = SSL_EVENTS.reserve::<LlmEvent>(0) {
        let event = unsafe { &mut *slot.as_mut_ptr() };
        if event.capture_data(ctx, rw, is_handshake).is_ok() {
            slot.submit(0);
        } else {
            slot.discard(0);
        }
    }
    // Always clear session state - handles success, failure, and reserve failure cases
    Session::clear(tid);
    0
}

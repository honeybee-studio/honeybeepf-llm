//! uprobes on Go's `crypto/tls`: `(*Conn).Write` and `Read` entries, each `RET` in `Read`, and `Close`.
//! Arguments follow Go's register ABI, and the current goroutine is in R14 (amd64) or R28 (arm64).

use aya_ebpf::{
    helpers::{bpf_get_current_pid_tgid, bpf_ktime_get_ns},
    macros::uprobe,
    programs::ProbeContext,
};
use honeybeepf_llm_common::{LlmDirection, LlmStat};

use super::{
    emit_fragments, filter,
    helpers::{SslCall, count},
    keep_task_stack,
    maps::GO_READS,
};

/// (first, second and third integer registers of the Go ABI, current goroutine) at this probe.
/// For a `*Conn` method taking `b []byte`: receiver, `b`'s pointer and length; at a return, the first is `n`.
#[inline(always)]
fn go_regs(ctx: &ProbeContext) -> (u64, u64, u64, u64) {
    #[cfg(bpf_target_arch = "x86_64")]
    {
        // SAFETY: a uprobe's context is the task's saved user registers.
        let r = unsafe { &*ctx.regs.cast::<aya_ebpf::bindings::pt_regs>() };
        (r.rax, r.rbx, r.rcx, r.r14)
    }
    #[cfg(bpf_target_arch = "aarch64")]
    {
        // SAFETY: a uprobe's context is the task's saved user registers.
        let r = unsafe { &*ctx.regs.cast::<aya_ebpf::bindings::user_pt_regs>() };
        (r.regs[0], r.regs[1], r.regs[2], r.regs[28])
    }
}

#[inline(always)]
fn goroutine_key(g: u64) -> [u64; 2] {
    [bpf_get_current_pid_tgid() >> 32, g]
}

#[inline(always)]
fn emit(call: &SslCall, rw: u8) {
    if filter::drops(call, rw) {
        count(LlmStat::Filtered);
    } else {
        emit_fragments(call, rw, false);
    }
}

/// `(*Conn).Write(b)`: the plaintext is in `b` at entry, so it is sent here; a failed write is rare.
#[uprobe]
pub fn probe_go_tls_write(ctx: ProbeContext) -> u32 {
    count(LlmStat::Enter);
    let (conn, buf, len, _) = go_regs(&ctx);
    if conn != 0 && buf != 0 && len != 0 {
        emit(&SslCall::go(conn, buf, len, 0), LlmDirection::Write as u8);
    }
    keep_task_stack(&ctx);
    0
}

/// `(*Conn).Read(b)` entry: saves `b` for the return probes.
#[uprobe]
pub fn probe_go_tls_read_enter(ctx: ProbeContext) -> u32 {
    count(LlmStat::Enter);
    let (conn, buf, _, g) = go_regs(&ctx);
    if conn != 0 && buf != 0 {
        // SAFETY: argument-free helper.
        let now = unsafe { bpf_ktime_get_ns() };
        let _ = GO_READS.insert(&goroutine_key(g), &[now, buf, conn], 0);
    }
    keep_task_stack(&ctx);
    0
}

/// At each `RET` of `(*Conn).Read`: `n` bytes of plaintext are in the saved `b`.
#[uprobe]
pub fn probe_go_tls_read_return(ctx: ProbeContext) -> u32 {
    let (n, _, _, g) = go_regs(&ctx);
    let key = goroutine_key(g);
    // SAFETY: the value is copied out at once, before the remove below.
    match unsafe { GO_READS.get(&key) }.copied() {
        // `n` is a Go int, so an error with no data is 0, never negative.
        Some([start, buf, conn]) if (n as i64) > 0 => {
            emit(&SslCall::go(conn, buf, n, start), LlmDirection::Read as u8);
        }
        Some(_) => count(LlmStat::Discard),
        None => {}
    }
    let _ = GO_READS.remove(&key);
    keep_task_stack(&ctx);
    0
}

/// `(*Conn).Close` entry: ends the connection's stream, as `SSL_free` does.
#[uprobe]
pub fn probe_go_tls_close(ctx: ProbeContext) -> u32 {
    count(LlmStat::Enter);
    let (conn, _, _, _) = go_regs(&ctx);
    if conn != 0 {
        filter::forget(conn);
        emit_fragments(&SslCall::free(conn), LlmDirection::Unknown as u8, false);
    }
    keep_task_stack(&ctx);
    0
}

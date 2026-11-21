//! Probes on the AEAD cipher of a TLS library built into an executable (BoringSSL, aws-lc): `seal` encrypts
//! one TLS 1.3 record and `open` decrypts one. A cipher context serves one direction of one connection, so it
//! is bound to a TCP socket: the thread's last receive when it is keyed, else its next send after a seal.

use aya_ebpf::{
    helpers::{bpf_get_current_pid_tgid, bpf_probe_read_user},
    macros::{kprobe, uprobe, uretprobe},
    programs::{ProbeContext, RetProbeContext},
};
use honeybeepf_llm_common::{LlmDirection, LlmStat};

use super::{
    emit_fragments, filter,
    gotls::emit,
    helpers::{SslCall, count, get_current_tid},
    keep_task_stack,
    maps::{AEAD_CTX, AEAD_OPENS, AEAD_RECV_SOCK, AEAD_SEALS, AEAD_TGIDS, SEAL_QUEUE, SealQueue},
};

/// TLS record content types; in TLS 1.3 the type is encrypted with the record.
const ALERT: u8 = 21;
const HANDSHAKE: u8 = 22;
const APPLICATION_DATA: u8 = 23;
/// Handshake messages sent inside a running session.
const NEW_SESSION_TICKET: u8 = 4;
const KEY_UPDATE: u8 = 24;
/// Longest TLS 1.3 record content with its type byte and padding; a longer length is not a record.
const MAX_RECORD: u64 = (1 << 14) + 256;
/// The vector form `(ctx, iovec, n, ..)` has an entry count below this as third argument; the classic form
/// `(ctx, out, pointer, ..)` has a pointer there. An iovec entry is {out, in, len}.
const VECTOR_MAX: u64 = 16;

/// Remember the process, so its TCP receives are tracked from now on.
#[inline(always)]
fn note_process() {
    let tgid = (bpf_get_current_pid_tgid() >> 32) as u32;
    // SAFETY: only presence is read.
    if unsafe { AEAD_TGIDS.get(&tgid) }.is_none() {
        let _ = AEAD_TGIDS.insert(&tgid, &1, 0);
    }
}

#[inline(always)]
fn context_key(aead: u64) -> [u64; 2] {
    [bpf_get_current_pid_tgid() >> 32, aead]
}

#[inline(always)]
fn read_byte(at: u64) -> Option<u8> {
    // SAFETY: the helper checks the user address; a fault returns Err.
    unsafe { bpf_probe_read_user(at as *const u8) }.ok()
}

/// [in, in_len, extra_in, extra_in_len] of the classic seal: its arguments 8 to 11, past the registers on
/// x86-64 and from the 9th on arm64.
#[inline(always)]
fn classic_seal_input(ctx: &ProbeContext) -> Option<[u64; 4]> {
    #[cfg(bpf_target_arch = "x86_64")]
    {
        // SAFETY: a uprobe's context is the task's saved user registers.
        let sp = unsafe { &*ctx.regs.cast::<aya_ebpf::bindings::pt_regs>() }.rsp;
        // At entry the stack holds the return address, then the arguments from the 7th.
        // SAFETY: the helper checks the user address; a fault returns Err.
        let [_, input, len, extra, extra_len] =
            unsafe { bpf_probe_read_user((sp + 8) as *const [u64; 5]) }.ok()?;
        Some([input, len, extra, extra_len])
    }
    #[cfg(bpf_target_arch = "aarch64")]
    {
        // SAFETY: a uprobe's context is the task's saved user registers.
        let r = unsafe { &*ctx.regs.cast::<aya_ebpf::bindings::user_pt_regs>() };
        // SAFETY: the helper checks the user address; a fault returns Err.
        let [len, extra, extra_len] =
            unsafe { bpf_probe_read_user(r.sp as *const [u64; 3]) }.ok()?;
        Some([r.regs[7], len, extra, extra_len])
    }
}

/// (plaintext, length, sealed in place) of the application data a seal call encrypts; a null plaintext is
/// a handshake or alert record that starts or ends the TLS session.
#[inline(always)]
fn sealed_record(ctx: &ProbeContext) -> Option<(u64, u64, bool)> {
    let (first, n): (u64, u64) = (ctx.arg(1)?, ctx.arg(2)?);
    let (out, input, len, typ_at) = if n < VECTOR_MAX {
        if n != 2 {
            return None;
        }
        // A TLS 1.3 record is sealed from two entries: its content, then its one-byte content type.
        // SAFETY: the helper checks the user address; a fault returns Err.
        let [out, input, len, _, typ, typ_len] =
            unsafe { bpf_probe_read_user(first as *const [u64; 6]) }.ok()?;
        if typ_len != 1 {
            return None;
        }
        (out, input, len, typ)
    } else {
        let [input, len, extra, extra_len] = classic_seal_input(ctx)?;
        match extra_len {
            1 => (first, input, len, extra),
            // Without extra input the caller put the type after the content.
            0 if len > 1 => (first, input, len - 1, input + len - 1),
            _ => return None,
        }
    };
    if input == 0 || len > MAX_RECORD {
        return None;
    }
    match read_byte(typ_at)? {
        APPLICATION_DATA if len > 0 => Some((input, len, input == out)),
        HANDSHAKE if matches!(read_byte(input), Some(NEW_SESSION_TICKET | KEY_UPDATE)) => None,
        HANDSHAKE | ALERT => Some((0, 0, false)),
        _ => None,
    }
}

/// Add a record sealed with an unbound context to the thread's queue; a record past `SEAL_QUEUE` is lost.
#[inline(always)]
fn queue_record(tid: u32, record: [u64; 3]) {
    let Some(queue) = AEAD_SEALS.get_ptr_mut(&tid) else {
        let mut queue = SealQueue {
            len: 1,
            records: [[0; 3]; SEAL_QUEUE],
        };
        queue.records[0] = record;
        let _ = AEAD_SEALS.insert(&tid, &queue, 0);
        return;
    };
    // SAFETY: the pointer is valid for this run, and only this thread writes its own entry.
    let queue = unsafe { &mut *queue };
    match queue.records.get_mut(queue.len as usize) {
        Some(slot) => {
            *slot = record;
            queue.len += 1;
        }
        None => count(LlmStat::Discard),
    }
}

/// One sealed record that goes out on `sk`.
#[inline(never)]
fn send_record(sk: u64, buf: u64, len: u64) {
    if buf == 0 {
        // A handshake or alert record starts or ends the TLS session on this socket.
        filter::forget(sk);
        emit_fragments(&SslCall::free(sk), LlmDirection::Unknown as u8, false);
    } else {
        emit(&SslCall::go(sk, buf, len, 0), LlmDirection::Write as u8);
    }
}

/// The records the thread queued go out on `sk`, in order, and their contexts are bound to it.
#[inline(always)]
fn flush_queue(tid: u32, sk: u64) {
    // SAFETY: the value is copied out at once, before the remove below.
    let Some(queue) = (unsafe { AEAD_SEALS.get(&tid) }).copied() else {
        return;
    };
    let _ = AEAD_SEALS.remove(&tid);
    for (i, &[buf, len, aead]) in queue.records.iter().enumerate() {
        if (i as u64) < queue.len {
            let _ = AEAD_CTX.insert(&context_key(aead), &sk, 0);
            // A null buffer with a length only binds its context.
            if buf != 0 || len == 0 {
                send_record(sk, buf, len);
            }
        }
    }
}

/// `init(ctx, key, ..)`: a TLS library keys a context while it handles the handshake bytes it just read,
/// so the context belongs to the socket of the thread's last receive.
#[uprobe]
pub fn probe_aead_init(ctx: ProbeContext) -> u32 {
    note_process();
    if let Some(aead) = ctx.arg::<u64>(0) {
        let key = context_key(aead);
        // SAFETY: the value is copied out at once.
        match unsafe { AEAD_RECV_SOCK.get(&get_current_tid()) }.copied() {
            Some(sk) => {
                let _ = AEAD_CTX.insert(&key, &sk, 0);
            }
            // The address may be a freed context's, bound to an older socket.
            None => {
                let _ = AEAD_CTX.remove(&key);
            }
        }
    }
    keep_task_stack(&ctx);
    0
}

/// `cleanup(ctx)`: the context's connection is over.
#[uprobe]
pub fn probe_aead_cleanup(ctx: ProbeContext) -> u32 {
    if let Some(aead) = ctx.arg::<u64>(0) {
        let _ = AEAD_CTX.remove(&context_key(aead));
    }
    keep_task_stack(&ctx);
    0
}

/// Seal entry: the plaintext is in the caller's buffer. A bound context sends it now; an unbound one queues
/// it for the thread's next TCP send, where the socket is learned.
#[uprobe]
pub fn probe_aead_seal(ctx: ProbeContext) -> u32 {
    count(LlmStat::Enter);
    if let (Some(aead), Some((buf, len, in_place))) = (ctx.arg::<u64>(0), sealed_record(&ctx)) {
        let tid = get_current_tid();
        // SAFETY: the value is copied out at once.
        match unsafe { AEAD_CTX.get(&context_key(aead)) }.copied() {
            Some(sk) => {
                flush_queue(tid, sk);
                send_record(sk, buf, len);
            }
            // Sealed in place, the buffer holds ciphertext by the next send: only the socket is learned.
            None if in_place => {
                count(LlmStat::Discard);
                queue_record(tid, [0, 1, aead]);
            }
            None => queue_record(tid, [buf, len, aead]),
        }
    }
    keep_task_stack(&ctx);
    0
}

/// `tcp_sendmsg(sk, ..)`: the records this thread queued since its last send go out on `sk`.
#[kprobe]
pub fn probe_aead_tcp_sendmsg(ctx: ProbeContext) -> u32 {
    if let Some(sk) = ctx.arg::<u64>(0) {
        flush_queue(get_current_tid(), sk);
    }
    0
}

/// `tcp_recvmsg(sk, ..)`: a context this thread keys next, or a record it opens with an unbound one,
/// belongs to `sk`.
#[kprobe]
pub fn probe_aead_tcp_recvmsg(ctx: ProbeContext) -> u32 {
    let pid_tgid = bpf_get_current_pid_tgid();
    // SAFETY: only presence is read.
    if unsafe { AEAD_TGIDS.get(&((pid_tgid >> 32) as u32)) }.is_none() {
        return 0;
    }
    if let Some(sk) = ctx.arg::<u64>(0) {
        let _ = AEAD_RECV_SOCK.insert(&(pid_tgid as u32), &sk, 0);
    }
    0
}

/// [out, len, context] of the record an open call decrypts: on success `out` holds `len` bytes, the
/// record's content and then its content type.
#[inline(always)]
fn opened_record(ctx: &ProbeContext) -> Option<[u64; 3]> {
    let (aead, first, n): (u64, u64, u64) = (ctx.arg(0)?, ctx.arg(1)?, ctx.arg(2)?);
    let (out, len) = if n < VECTOR_MAX {
        if n != 1 {
            return None;
        }
        // The single entry is decrypted in place.
        // SAFETY: the helper checks the user address; a fault returns Err.
        let [out, _, len] = unsafe { bpf_probe_read_user(first as *const [u64; 3]) }.ok()?;
        (out, len)
    } else {
        // Classic form: `(ctx, out, nonce, nonce_len, in, in_len, ..)`.
        (first, ctx.arg(5)?)
    };
    (out != 0 && len > 1 && len <= MAX_RECORD).then_some([out, len, aead])
}

/// Open entry: saves where the plaintext will be.
#[uprobe]
pub fn probe_aead_open_enter(ctx: ProbeContext) -> u32 {
    count(LlmStat::Enter);
    note_process();
    let tid = get_current_tid();
    match opened_record(&ctx) {
        Some(record) => {
            let _ = AEAD_OPENS.insert(&tid, &record, 0);
        }
        None => {
            let _ = AEAD_OPENS.remove(&tid);
        }
    }
    keep_task_stack(&ctx);
    0
}

/// Socket of the context that opened a record: its binding, else the thread's last receive, bound now.
#[inline(always)]
fn opening_socket(tid: u32, aead: u64) -> Option<u64> {
    let key = context_key(aead);
    // SAFETY: the value is copied out at once.
    if let Some(sk) = unsafe { AEAD_CTX.get(&key) }.copied() {
        return Some(sk);
    }
    // SAFETY: as above.
    let sk = unsafe { AEAD_RECV_SOCK.get(&tid) }.copied()?;
    let _ = AEAD_CTX.insert(&key, &sk, 0);
    Some(sk)
}

/// Open return: on success the buffer holds the record's content followed by its content type.
#[uretprobe]
pub fn probe_aead_open_exit(ctx: RetProbeContext) -> u32 {
    let tid = get_current_tid();
    // SAFETY: the value is copied out at once, before the remove below.
    if let Some([buf, len, aead]) = (unsafe { AEAD_OPENS.get(&tid) }).copied() {
        let _ = AEAD_OPENS.remove(&tid);
        let typ = if ctx.ret::<i32>() == Some(1) {
            read_byte(buf + len - 1)
        } else {
            None
        };
        match (typ, opening_socket(tid, aead)) {
            (Some(APPLICATION_DATA), Some(sk)) => {
                emit(&SslCall::go(sk, buf, len - 1, 0), LlmDirection::Read as u8);
            }
            (Some(APPLICATION_DATA) | None, _) => count(LlmStat::Discard),
            _ => {}
        }
    }
    keep_task_stack(&ctx);
    0
}

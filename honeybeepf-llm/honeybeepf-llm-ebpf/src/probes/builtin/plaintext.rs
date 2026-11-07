//! Socket-level plaintext (non-TLS) capture. The sockops program records each watched socket in a
//! sockhash and its pid/cgroup in PLAIN_CONNS, and clears that entry on TCP close; the sk_msg and
//! sk_skb programs copy each watched send and receive into PLAIN_EVENTS, attributed from that entry.
use aya_ebpf::{
    EbpfContext,
    bindings::{
        BPF_NOEXIST, BPF_SOCK_OPS_ACTIVE_ESTABLISHED_CB, BPF_SOCK_OPS_PASSIVE_ESTABLISHED_CB,
        BPF_SOCK_OPS_STATE_CB, BPF_SOCK_OPS_STATE_CB_FLAG, BPF_TCP_CLOSE, sk_action::SK_PASS,
    },
    helpers::{
        bpf_get_current_cgroup_id, bpf_get_current_pid_tgid, bpf_get_socket_cookie,
        bpf_msg_pull_data, bpf_probe_read_kernel_buf, bpf_skb_load_bytes,
    },
    macros::{map, sk_msg, sock_ops, stream_verdict},
    maps::{HashMap, RingBuf, SockHash},
    programs::{SkBuffContext, SkMsgContext, SockOpsContext},
};
use honeybeepf_llm_common::{
    EventMetadata, LlmDirection, LlmEventHeader, MAX_SSL_BUF_SIZE, PLAIN_CONNS_ENTRIES,
    PLAIN_ENDPOINTS_ENTRIES, PLAIN_SOCKS_ENTRIES, PlainFlowKey, plain_endpoint_key, plain_flow_key,
    sock_ops_remote_port,
};

/// Minimum when plaintext is off; the loader bumps it to the full size when enabled.
pub const PLAIN_RINGBUF_MIN: u32 = 4096;

/// Payload buckets: a send reserves the smallest that holds its first bytes, so a short request
/// takes a small ring slot. Largest is `MAX_SSL_BUF_SIZE`; a write is captured only up to it.
const PLAIN_BUCKET_SMALL: usize = 512;
const PLAIN_BUCKET_MED: usize = 2048;

#[map]
pub static PLAIN_ENDPOINTS: HashMap<u64, u8> =
    HashMap::with_max_entries(PLAIN_ENDPOINTS_ENTRIES, 0);

#[map]
pub static PLAIN_SOCKS: SockHash<u64> = SockHash::with_max_entries(PLAIN_SOCKS_ENTRIES, 0);

#[map]
pub static PLAIN_CONNS: HashMap<PlainFlowKey, EventMetadata> =
    HashMap::with_max_entries(PLAIN_CONNS_ENTRIES, 0);

#[map]
pub static PLAIN_EVENTS: RingBuf = RingBuf::with_byte_size(PLAIN_RINGBUF_MIN, 0);

/// Add a connecting socket to the sockhash only if its destination is watched, record the task's
/// pid/cgroup for later attribution, and request a state callback so the entry is freed on close.
#[sock_ops]
pub fn plaintext_sockops(ctx: SockOpsContext) -> u32 {
    let op = ctx.op();
    if op == BPF_SOCK_OPS_STATE_CB {
        // Fires only on sockets we flagged (watched ones); free their attribution when they close.
        if ctx.arg(1) == BPF_TCP_CLOSE {
            let flow = plain_flow_key(
                ctx.local_ip4(),
                ctx.remote_ip4(),
                ctx.local_port(),
                ctx.remote_port(),
            );
            let _ = PLAIN_CONNS.remove(&flow);
        }
        return 0;
    }
    if op != BPF_SOCK_OPS_ACTIVE_ESTABLISHED_CB && op != BPF_SOCK_OPS_PASSIVE_ESTABLISHED_CB {
        return 0;
    }
    let key = plain_endpoint_key(ctx.remote_ip4(), sock_ops_remote_port(ctx.remote_port()));
    // SAFETY: map lookup with a stack key; the returned reference is only tested for presence.
    if unsafe { PLAIN_ENDPOINTS.get(&key) }.is_none() {
        return 0;
    }
    let pid_tgid = bpf_get_current_pid_tgid();
    let meta = EventMetadata {
        pid: (pid_tgid >> 32) as u32,
        _pad: pid_tgid as u32,
        // SAFETY: the helper reads the current task's cgroup; 1 on cgroup v1, which userspace gates out.
        cgroup_id: unsafe { bpf_get_current_cgroup_id() },
        timestamp: 0,
    };
    // Key by the 4-tuple so the sk_msg side (which may not call the cookie helper) rebuilds the same key.
    let flow = plain_flow_key(
        ctx.local_ip4(),
        ctx.remote_ip4(),
        ctx.local_port(),
        ctx.remote_port(),
    );
    let _ = PLAIN_CONNS.insert(&flow, &meta, 0);
    // SAFETY: `ctx.as_ptr()` is this callback's live context, valid for the helper call.
    let mut cookie = unsafe { bpf_get_socket_cookie(ctx.as_ptr()) };
    // SAFETY: `ctx.ops` is the live bpf_sock_ops this callback was given, borrowed only for the update.
    let ops = unsafe { &mut *ctx.ops };
    let _ = PLAIN_SOCKS.update(&mut cookie, ops, BPF_NOEXIST as u64);
    // Ask for the state callback so the close path above frees PLAIN_CONNS for this socket.
    let _ = ctx.set_cb_flags((ctx.cb_flags() | BPF_SOCK_OPS_STATE_CB_FLAG) as i32);
    0
}

/// Fill a reserved record's header; the caller sets `buf_filled` after the copy. `ssl_ptr` stays 0
/// until a later PR joins the parser and supplies a stream key.
#[inline(always)]
fn init_header(
    h: &mut LlmEventHeader,
    meta: &EventMetadata,
    len: u32,
    rw: LlmDirection,
    truncated: u8,
) {
    h.metadata = *meta;
    h.ssl_ptr = 0;
    h.latency_ns = 0;
    h.len = len;
    h.offset = 0;
    h.is_handshake = 0;
    h.rw = rw as u8;
    h.truncated = truncated;
    h.closed = 0;
    h.comm = [0u8; MAX_COMM];
}

/// Copy a watched send into PLAIN_EVENTS as an `N`-byte-payload record; `N` is constant so the
/// verifier can bound the copy. The record is `LlmEventHeader` + payload, the SSL ring's shape.
#[inline(always)]
fn emit_send<const N: usize>(ctx: &SkMsgContext, meta: &EventMetadata, size: u32) {
    let Some(mut slot) = PLAIN_EVENTS.reserve::<PlainEvent<N>>(0) else {
        return;
    };
    // SAFETY: the slot is reserved for a PlainEvent<N>; every header field is set before submit.
    let ev = unsafe { &mut *slot.as_mut_ptr() };
    init_header(
        &mut ev.header,
        meta,
        size,
        LlmDirection::Write,
        (size as usize > N) as u8,
    );
    // The dispatch keeps `size` within the chosen bucket, but the compiler spills it and the verifier
    // loses that bound; a volatile reload makes the min(.., N) survive so the copy stays <= N.
    // SAFETY: &size is a valid initialized local.
    let n = core::cmp::min(unsafe { core::ptr::read_volatile(&size) } as usize, N);
    let data = ctx.data() as *const u8;
    // SAFETY: `data` is the message's linear head after pull_data; n <= N keeps the ring write in bounds.
    if unsafe { bpf_probe_read_kernel_buf(data, &mut ev.buf[..n]) }.is_err() {
        ev.header.buf_filled = 0;
        slot.discard(0);
        return;
    }
    ev.header.buf_filled = 1;
    slot.submit(0);
}

/// Copy a watched receive into PLAIN_EVENTS; mirrors `emit_send` but reads from the (possibly
/// non-linear) skb with the bounded `bpf_skb_load_bytes` helper, so no pull is needed.
#[inline(always)]
fn emit_recv<const N: usize>(ctx: &SkBuffContext, meta: &EventMetadata, size: u32) {
    let Some(mut slot) = PLAIN_EVENTS.reserve::<PlainEvent<N>>(0) else {
        return;
    };
    // SAFETY: the slot is reserved for a PlainEvent<N>; every header field is set before submit.
    let ev = unsafe { &mut *slot.as_mut_ptr() };
    init_header(
        &mut ev.header,
        meta,
        size,
        LlmDirection::Read,
        (size as usize > N) as u8,
    );
    // SAFETY: &size is a valid initialized local; the volatile reload keeps the min(.., N) bound.
    let n = core::cmp::min(unsafe { core::ptr::read_volatile(&size) } as usize, N);
    // bpf_skb_load_bytes rejects a zero-length read, so bail on an empty record.
    if n == 0 {
        ev.header.buf_filled = 0;
        slot.discard(0);
        return;
    }
    // Call the helper directly, not aya's `load_bytes`: that re-mins the length with `skb->len`,
    // whose lower bound of 0 the verifier keeps, so it rejects the read. The volatile reload plus
    // clamp pins the length passed here to [1, N].
    // SAFETY: &n is a valid initialized local.
    let n = unsafe { core::ptr::read_volatile(&n) }.clamp(1, N);
    let dst = &mut ev.buf[..n];
    // SAFETY: `ctx.as_ptr()` is the live skb; `dst` is exactly `n` (1..=N) bytes of the reserved
    // slot and `n` is the length passed, so the kernel write stays in bounds.
    let loaded = unsafe {
        bpf_skb_load_bytes(
            ctx.as_ptr(),
            0,
            dst.as_mut_ptr() as *mut core::ffi::c_void,
            n as u32,
        )
    };
    if loaded != 0 {
        ev.header.buf_filled = 0;
        slot.discard(0);
        return;
    }
    ev.header.buf_filled = 1;
    slot.submit(0);
}

/// Linearize the send's first bytes and emit them in the smallest bucket that fits.
#[inline(never)]
fn capture_send(ctx: &SkMsgContext, meta: &EventMetadata) {
    let size = ctx.size();
    if size == 0 {
        return;
    }
    let to_read = core::cmp::min(size as usize, MAX_SSL_BUF_SIZE);
    // SAFETY: `ctx.msg` is this callback's live message; [0, to_read) is within its size, and the
    // pull makes that span contiguous so the kernel read below sees one linear region.
    if unsafe { bpf_msg_pull_data(ctx.msg, 0, to_read as u32, 0) } != 0 {
        return;
    }
    if to_read <= PLAIN_BUCKET_SMALL {
        emit_send::<PLAIN_BUCKET_SMALL>(ctx, meta, size);
    } else if to_read <= PLAIN_BUCKET_MED {
        emit_send::<PLAIN_BUCKET_MED>(ctx, meta, size);
    } else {
        emit_send::<MAX_SSL_BUF_SIZE>(ctx, meta, size);
    }
}

/// Copy each send on a watched socket into PLAIN_EVENTS, attributed from the connect-time entry
/// found by the connection's 4-tuple. Always SK_PASS: the bytes are copied, never redirected.
#[sk_msg]
pub fn plaintext_skmsg(ctx: SkMsgContext) -> u32 {
    // SAFETY: `ctx.msg` is this callback's live sk_msg_md; only the 4-tuple fields are read here.
    let msg = unsafe { &*ctx.msg };
    let flow = plain_flow_key(
        msg.local_ip4,
        msg.remote_ip4,
        msg.local_port,
        msg.remote_port,
    );
    // SAFETY: map lookup with a stack key; the value is copied out before any other map or helper call.
    let Some(meta) = (unsafe { PLAIN_CONNS.get(&flow) }) else {
        return SK_PASS;
    };
    let meta = *meta;
    capture_send(&ctx, &meta);
    SK_PASS
}

/// Emit the receive's first bytes in the smallest bucket that fits; `bpf_skb_load_bytes` copies
/// directly from the skb, so no pull is needed.
#[inline(never)]
fn capture_recv(ctx: &SkBuffContext, meta: &EventMetadata) {
    let size = ctx.len();
    if size == 0 {
        return;
    }
    let to_read = core::cmp::min(size as usize, MAX_SSL_BUF_SIZE);
    if to_read <= PLAIN_BUCKET_SMALL {
        emit_recv::<PLAIN_BUCKET_SMALL>(ctx, meta, size);
    } else if to_read <= PLAIN_BUCKET_MED {
        emit_recv::<PLAIN_BUCKET_MED>(ctx, meta, size);
    } else {
        emit_recv::<MAX_SSL_BUF_SIZE>(ctx, meta, size);
    }
}

/// Copy each receive on a watched socket into PLAIN_EVENTS, attributed from the connect-time entry
/// found by the connection's 4-tuple. Always SK_PASS: the bytes are copied, never redirected.
#[stream_verdict]
pub fn plaintext_skskb(ctx: SkBuffContext) -> u32 {
    let flow = plain_flow_key(
        ctx.skb.local_ipv4(),
        ctx.skb.remote_ipv4(),
        ctx.skb.local_port(),
        ctx.skb.remote_port(),
    );
    // SAFETY: map lookup with a stack key; the value is copied out before any other map or helper call.
    let Some(meta) = (unsafe { PLAIN_CONNS.get(&flow) }) else {
        return SK_PASS;
    };
    let meta = *meta;
    capture_recv(&ctx, &meta);
    SK_PASS
}

const MAX_COMM: usize = 16;

/// One PLAIN_EVENTS record: the header at offset 0, then `N` payload bytes.
#[repr(C)]
struct PlainEvent<const N: usize> {
    header: LlmEventHeader,
    buf: [u8; N],
}

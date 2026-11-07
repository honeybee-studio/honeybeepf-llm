//! Socket-level plaintext (non-TLS) capture. On connect the sockops program records the socket in a
//! sockhash and its pid/cgroup in PLAIN_CONNS; the sk_msg program copies each watched send into
//! PLAIN_EVENTS, attributed from that entry. The per-recv program comes in a later PR.
use aya_ebpf::{
    EbpfContext,
    bindings::{
        BPF_NOEXIST, BPF_SOCK_OPS_ACTIVE_ESTABLISHED_CB, BPF_SOCK_OPS_PASSIVE_ESTABLISHED_CB,
        sk_action::SK_PASS,
    },
    helpers::{
        bpf_get_current_cgroup_id, bpf_get_current_pid_tgid, bpf_get_socket_cookie,
        bpf_msg_pull_data, bpf_probe_read_kernel_buf,
    },
    macros::{map, sk_msg, sock_ops},
    maps::{HashMap, RingBuf, SockHash},
    programs::{SkMsgContext, SockOpsContext},
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

/// Add a connecting socket to the sockhash only if its destination is watched, and record the
/// connecting task's pid/cgroup so a send seen later (possibly in softirq) stays attributable.
#[sock_ops]
pub fn plaintext_sockops(ctx: SockOpsContext) -> u32 {
    let op = ctx.op();
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
    0
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
    ev.header.metadata = *meta;
    ev.header.ssl_ptr = 0; // the plaintext stream key is set when a later PR joins the parser
    ev.header.latency_ns = 0;
    ev.header.len = size;
    ev.header.offset = 0;
    ev.header.is_handshake = 0;
    ev.header.rw = LlmDirection::Write as u8;
    ev.header.truncated = (size as usize > N) as u8;
    ev.header.closed = 0;
    ev.header.comm = [0u8; MAX_COMM];
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

const MAX_COMM: usize = 16;

/// One PLAIN_EVENTS record: the header at offset 0, then `N` payload bytes.
#[repr(C)]
struct PlainEvent<const N: usize> {
    header: LlmEventHeader,
    buf: [u8; N],
}

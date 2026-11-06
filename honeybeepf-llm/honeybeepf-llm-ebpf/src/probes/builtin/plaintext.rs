//! Socket-level plumbing for plaintext (non-TLS) capture. On connect the sockops program adds a
//! socket to the sockhash only when its destination is in the watched-endpoint map; the per-send
//! and per-recv programs that consume the sockhash and `PLAIN_EVENTS` come in later PRs.
use aya_ebpf::{
    EbpfContext,
    bindings::{
        BPF_NOEXIST, BPF_SOCK_OPS_ACTIVE_ESTABLISHED_CB, BPF_SOCK_OPS_PASSIVE_ESTABLISHED_CB,
    },
    helpers::bpf_get_socket_cookie,
    macros::{map, sock_ops},
    maps::{HashMap, RingBuf, SockHash},
    programs::SockOpsContext,
};
use honeybeepf_llm_common::{plain_endpoint_key, sock_ops_remote_port};

/// Watched destinations (IPv4:port), filled by userspace from the resolved endpoints.
pub const PLAIN_ENDPOINTS_ENTRIES: u32 = 8192;
/// Watched sockets, inserted on connect; a later PR's sk_msg/sk_skb run on these.
pub const PLAIN_SOCKS_ENTRIES: u32 = 65536;
/// Minimum when plaintext is off; the loader bumps it to the full size when enabled.
pub const PLAIN_RINGBUF_MIN: u32 = 4096;

#[map]
pub static PLAIN_ENDPOINTS: HashMap<u64, u8> =
    HashMap::with_max_entries(PLAIN_ENDPOINTS_ENTRIES, 0);

#[map]
pub static PLAIN_SOCKS: SockHash<u64> = SockHash::with_max_entries(PLAIN_SOCKS_ENTRIES, 0);

#[map]
pub static PLAIN_EVENTS: RingBuf = RingBuf::with_byte_size(PLAIN_RINGBUF_MIN, 0);

/// Add a connecting socket to the sockhash only if its destination is watched. Unrelated
/// connections pay one map lookup at connect and nothing per packet.
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
    // SAFETY: `ctx.as_ptr()` is this callback's live context, valid for the helper call.
    let mut cookie = unsafe { bpf_get_socket_cookie(ctx.as_ptr()) };
    // SAFETY: `ctx.ops` is the live bpf_sock_ops this callback was given, borrowed only for the update.
    let ops = unsafe { &mut *ctx.ops };
    let _ = PLAIN_SOCKS.update(&mut cookie, ops, BPF_NOEXIST as u64);
    0
}

# Security review

This document is for the security and platform teams who approve the agent before it runs. honeybeepf-llm is
a DaemonSet that runs on every node with eBPF capabilities, the host PID namespace and a read-only view of the
host filesystem. It attaches eBPF programs to OpenSSL and to a few kernel hooks, reads the TLS plaintext of LLM
API calls in memory on the node, and exports token and cost metrics over OTLP. It makes no change to the
observed applications and runs no proxy.

Every statement below is drawn from the chart, the code or the CI workflows, with the file named inline. Where a
behaviour is opt-in, that is stated; a reader can confirm each fact at the cited path. To report a suspected
vulnerability, follow [`SECURITY.md`](../SECURITY.md) (GitHub private vulnerability reporting).

## 1. Kernel hooks

The agent loads the programs below. The LLM programs are the default set; the process-lifecycle, file-access and
plaintext programs attach only when they are enabled or configured (see the notes under the table). Each program
is small and only reads: none changes a syscall result, drops a packet or redirects a connection.

| Program (eBPF) | Type | Attaches to | Purpose | What it reads |
|---|---|---|---|---|
| `probe_ssl_rw_enter` | uprobe | `SSL_read`, `SSL_write` entry | record the call's buffer pointer and length | arg0/arg1 (SSL\*, buffer pointer) |
| `probe_ssl_read_exit` / `probe_ssl_write_exit` | uretprobe | `SSL_read` / `SSL_write` return | copy the decrypted bytes of the call | up to 16 KB of a read, 4 KB of a write |
| `probe_ssl_rw_ex_enter` | uprobe | `SSL_read_ex`, `SSL_write_ex` entry | same, for the `_ex` variants (byte count via arg3) | arg0/arg1/arg3 pointers |
| `probe_ssl_read_ex_exit` / `probe_ssl_write_ex_exit` | uretprobe | `SSL_read_ex` / `SSL_write_ex` return | copy the decrypted bytes | up to 16 KB of a read, 4 KB of a write |
| `probe_ssl_do_handshake_enter` / `_exit` | uprobe / uretprobe | `SSL_do_handshake` | start the latency timer; mark a reused `SSL*` | the `SSL*` pointer only, no data |
| `probe_ssl_free` | uprobe | `SSL_free` | end the connection's stream (the address can be reused) | the `SSL*` pointer only |
| `probe_ssl_task_exit` | tracepoint | `sched:sched_process_exit` | clear session state for a thread killed mid-call | thread id only |
| `probe_exec` | tracepoint | `sched:sched_process_exec` | tell userspace to attach SSL probes to the new process | pid and the process `comm` |
| `probe_lib_open_fentry` / `probe_lib_open` | fentry, kprobe fallback | `do_sys_openat2` | notice a `libssl.so*` open before its first SSL call | the opened path |
| `honeybeepf_llm_group_exit` | kprobe | `do_group_exit` | mark a process group that is exiting | pid/tgid only |
| `honeybeepf_llm_process_exit` | kprobe | `acct_collect` | report a process exit at its last thread (needs `CONFIG_BSD_PROCESS_ACCT`) | pid/tgid only |
| `honeybeepf_llm_oom_mark_victim` | tracepoint | `oom:mark_victim` | flag a pid the OOM killer chose, so its exit is attributed | the victim pid |
| `sys_enter_openat` | tracepoint | the `openat` syscall | record opens of a short list of watched paths | the opened path (matched against the list) |
| `plaintext_connect4` | cgroup (connect4) | the node's cgroup v2 root | note the connecting task when the destination is a watched endpoint | destination IPv4/port, task pid/cgroup |
| `plaintext_sockops` | sock_ops | the node's cgroup v2 root | add a watched socket to the sockhash; free it and end its stream on close | the socket 4-tuple |
| `plaintext_skmsg` | sk_msg | the plaintext sockhash | copy each send on a watched socket (always `SK_PASS`, never redirected) | up to 4 KB of the sent bytes |
| `plaintext_skskb` | sk_skb (`stream_verdict`) | the plaintext sockhash | copy each receive on a watched socket (always `SK_PASS`) | up to 16 KB of the received bytes |

- The SSL uprobe targets and the program names are the table in
  `honeybeepf-llm/honeybeepf-llm/src/probes/builtin/llm/mod.rs`; the exec, libssl-open and process-lifecycle
  attach points are in `honeybeepf-llm/honeybeepf-llm/src/probes/mod.rs` and
  `.../probes/builtin/process_lifecycle.rs`; the eBPF programs are under
  `honeybeepf-llm/honeybeepf-llm-ebpf/src/probes/builtin/`.
- `probe_ssl_rw_enter` is attached to both `SSL_read` and `SSL_write`; only the return probes differ. The agent
  also attaches to a process's own executable when it exports `SSL_read`/`SSL_write` (official Node.js builds);
  this is the same uprobe set, switched by `builtinProbes.llm.executables` (default true).
- **Plaintext programs** (`plaintext_connect4`, `plaintext_sockops`, `plaintext_skmsg`, `plaintext_skskb`) load
  only when a provider sets `plaintext` and the node has a cgroup v2 hierarchy on 5.10+; otherwise they stay off
  and the agent logs why (`honeybeepf-llm/honeybeepf-llm/src/lib.rs`, `plaintext::start`). They capture non-TLS
  calls to configured in-cluster endpoints only; both data programs always return `SK_PASS`, so they copy bytes
  and never alter the stream (`.../probes/builtin/plaintext.rs`).
- **The process-lifecycle hooks are off by default.** `honeybeepf_llm_group_exit`, `honeybeepf_llm_process_exit`
  and `honeybeepf_llm_oom_mark_victim` attach only when `builtinProbes.process_lifecycle.enabled` is true
  (default false in `charts/honeybeepf-llm/values.yaml`; `honeybeepf-llm/honeybeepf-llm/src/lib.rs`). They read
  only pids, and a kernel missing a hook (for example no `CONFIG_BSD_PROCESS_ACCT`) is logged and skipped.
- **The `openat` file-access tracepoint is off by default** (`builtinProbes.filesystem.file_access.enabled:
  false` in `charts/honeybeepf-llm/values.yaml`). When enabled it records opens of the configured `watched_paths`
  only (default list: a few SSH-key paths), capped at 128 paths (`MAX_WATCHED_PATHS`,
  `.../probes/builtin/file_access.rs`). It reads the path, not file contents.

## 2. Privileges

The container's `securityContext` is in `charts/honeybeepf-llm/values.yaml`: `readOnlyRootFilesystem: true`,
`seccompProfile.type: RuntimeDefault`, `capabilities.drop: [ALL]`, then these are added back:

| Capability | Why it is needed (from the chart comment and the code) |
|---|---|
| `BPF` | load the eBPF programs and maps |
| `PERFMON` | attach the uprobes, tracepoints and fentry program and read user memory (`bpf_probe_read_user`) |
| `SYS_ADMIN` | the uprobe `perf_event_open`: Ubuntu 7.0 denies it with `PERFMON` alone (`EACCES` at `perf_event_paranoid` 2–4) |
| `SYS_PTRACE` | cross-pod `/proc` access for libssl discovery (blocked by container LSM profiles and yama without it) |

`SYS_ADMIN` is present for one reason — the `perf_event_open` denial noted in the chart (with a kernel bug link).
Whether it can be dropped depends on the node's distribution and `perf_event_paranoid`; the chart keeps it so the
default install attaches on the kernels CI covers. There is no other `SYS_ADMIN`-only operation in the code.

Host access, from `charts/honeybeepf-llm/templates/daemonset.yaml`:

- `hostPID: true` — to read `/proc/<pid>` of other pods for libssl discovery and pod attribution.
- Host mounts, all `readOnly: true`: `/` at `/host` (uprobe targets, the SSL libraries), `/sys/kernel/tracing`
  and `/sys/kernel/debug` (tracefs for the tracepoints). A prices ConfigMap is mounted read-only only when
  `builtinProbes.llm.prices` is set. No host path is mounted writable.
- No `privileged: true`, no `hostNetwork`, no `hostIPC`.

Kubernetes RBAC, from `charts/honeybeepf-llm/templates/clusterrole.yaml` (bound to the chart's ServiceAccount):

| API group | Resources | Verbs | Why | When granted |
|---|---|---|---|---|
| `""` (core) | `pods` | get, list, watch | map a pid/cgroup to a pod for metric labels | always |
| `batch` | `jobs` | get | a pod references its Job; the agent reads it to find the CronJob | always |
| `""` (core) | `services` | get, list, watch | resolve a configured plaintext provider's in-cluster hosts to ClusterIPs | only when a provider sets `plaintext` |
| `discovery.k8s.io` | `endpointslices` | get, list, watch | resolve those Services to ready pod IPs | only when a provider sets `plaintext` |

The role is read-only across the cluster (no create/update/delete, no secrets). The `services`/`endpointslices`
rules are emitted only when a provider sets `plaintext`; otherwise the ClusterRole has just `pods` and `jobs`.

## 3. Data handling

- **Plaintext stays in memory on the node.** The uprobes copy decrypted request/response bytes into a BPF ring
  buffer; userspace parses the HTTP to find token counts and drops the bytes. Captured bytes are bounded (16 KB
  per SSL read, 4 KB per write at the kernel; `honeybeepf-llm/honeybeepf-llm-ebpf/...`), and the request buffer
  is capped at 8 MB and the response at 16 MB in userspace.
- **Only counters and labels are exported, never message text or secrets.** The metrics are the enumerated set
  in `honeybeepf-llm/honeybeepf-llm/src/telemetry.rs` (request/token/latency/cost counters). Their labels are a
  fixed set — namespace, pod, workload, model name, reason, operation, modality, transport — not request or
  response bodies. A test feeds fake API keys in the three forms providers use (OpenAI `Authorization: Bearer`,
  Gemini `?key=`/`x-goog-api-key`, Bedrock SigV4 presigned query) and asserts the agent's logs never contain them
  (`test_credentials_stay_out_of_logs`, `.../probes/builtin/llm/processor/tests.rs`).
- **It writes nothing to disk and has no inbound listener.** The root filesystem is read-only and every host
  mount is read-only (section 2); metrics leave over OTLP to the configured collector (outbound only). The chart
  defines no Service and the container exposes no port, so nothing connects in.
- The agent reads the plaintext of LLM calls on its own node by design; keeping that plaintext inside the agent
  is the security boundary, as stated in `SECURITY.md`.

## 4. Resource budget per node

Measured overhead is in [docs/benchmark.md](benchmark.md); the numbers below are from its v0.2.0 run (kernel 7.0,
4 vCPU). Use that document as the source of truth.

| Measure | Value (from docs/benchmark.md) |
|---|---|
| Agent CPU | 0.067 CPU-seconds per 1,000 requests in the `llm` case; 0.008 at the `max` rate |
| Memory (RSS anon / file) | ~4.2 MB anonymous, ~26.8 MB file-backed; PSS ~20.6 MB |
| BPF map memory (`memlock`) | ~18.8 MB (the `bpftool map` figure) |
| Application throughput impact | −6.5% in the saturating `max` case; negligible at 200 req/s |

The chart requests 200m CPU / 512Mi and limits 1 CPU / 1Gi per node
(`charts/honeybeepf-llm/values.yaml`). The BPF maps are bounded by construction, so their memory does not grow
with traffic: the SSL ring buffer is 8 MB and the plaintext ring buffer 4 MB when it loads, the exec and
libssl-open ring buffers 64 KB each, the process-exit ring buffer 1 MB; the per-pid session hashes hold 10,240
entries, the connection-class map 32,768, and the plaintext socket/connection maps 65,536 each (sizes in
`honeybeepf-llm/honeybeepf-llm-ebpf/src/probes/builtin/llm/maps.rs`, `.../plaintext.rs` and
`honeybeepf-llm/honeybeepf-llm-common/src/lib.rs`).

## 5. Failure behaviour

The agent is an observer, not a proxy, so it is **not in the request path**. Every eBPF program only reads or
observes: the uprobes copy buffers, `plaintext_connect4` always allows the connect, and the `sk_msg`/`sk_skb`
programs always return `SK_PASS`. If the agent is slow, crashes, is OOM-killed or cannot attach, application
traffic is unaffected — at worst some calls go unrecorded.

- **Crash or OOM-kill:** the DaemonSet restarts the pod; metrics stop until it is back. The container requests
  512Mi and limits 1Gi (section 4), and BPF maps are bounded (section 4), so steady-state memory is predictable.
- **Cannot attach / missing hook:** attach failures are logged and the agent reports which kernel hook is missing
  (for example `acct_collect` without `CONFIG_BSD_PROCESS_ACCT`, or the `oom:mark_victim` tracepoint;
  `.../probes/builtin/process_lifecycle.rs`). Plaintext capture that cannot attach logs why and stays off
  (section 1); it does not fail the agent.
- **A startup error after programs attach** sets a global shutdown flag through a drop guard so the ring-buffer
  reader threads stop and the process exits with an error line rather than hanging
  (`ShutdownOnDrop` in `HoneyBeeEngine::run`, `honeybeepf-llm/honeybeepf-llm/src/lib.rs`).
- **Visibility:** the pod's liveness and readiness probes read `/proc/self/status`
  (`charts/honeybeepf-llm/templates/daemonset.yaml`); a dropped call is counted under
  `llm_responses_unparsed_total` with a reason, and kernel-side losses under `llm_events_total`
  (see [docs/metrics.md](metrics.md)), so a degraded agent is visible in its own metrics.

## 6. Coexistence with other eBPF tools

- **Other uprobes on libssl.** uprobes are not exclusive: several tools can attach to the same `SSL_read`/
  `SSL_write` without conflict. The agent attaches by `(device, inode)` of each libssl (or of an executable that
  exports the symbols) and attaches each file once; it does not patch libssl or change its behaviour, so another
  tracer on the same functions is unaffected and vice versa.
- **CNIs and other cgroup programs.** The plaintext `connect4` program attaches to the node's cgroup v2 root
  with `CgroupAttachMode::Single` (`honeybeepf-llm/honeybeepf-llm/src/plaintext.rs`) and only observes (it always
  allows the connect). Running it next to a CNI that has its own cgroup `connect4` program has not been tested; if
  the attach fails, plaintext capture stays off with a log line and TLS capture is unaffected. It loads only when
  plaintext capture is configured (section 1). The `sk_msg`/`sk_skb` programs act on the agent's own sockhash,
  which holds only the sockets it watches; they do not touch other sockets.
- **Other tracepoint and kprobe tools.** Tracepoints and kprobes are multiplexed by the kernel, so the
  agent's `sched_process_exec`, `oom:mark_victim`, `do_group_exit`, `acct_collect` and `openat` hooks run
  alongside another tool's hooks on the same points.
- **What is shared vs. not.** Shared: the kernel attach points above, which allow multiple consumers. Not shared:
  all of the agent's BPF maps and ring buffers are its own; it reads no other tool's maps and exposes none for
  others to write. It needs no pinned maps and no bpffs sharing.

## 7. Supply chain

- **Pinned actions.** Every GitHub Action in `.github/workflows/` is pinned to a full commit SHA with the version
  in a trailing comment (for example `actions/checkout@3d3c42e5… # v7.0.1`,
  `docker/build-push-action@10e90e36… # v6.19.2`). This holds across `build.yml`, `docker-publish.yml`,
  `kernel-matrix.yml`, `benchmark.yml` and `release.yml`.
- **Least-privilege workflow tokens.** Each workflow sets top-level `permissions: contents: read`; only the
  image-publish job raises it to `packages: write`, and the release job to `contents: write` for the GitHub
  release (`docker-publish.yml`, `release.yml`). Fork pull requests get a read-only token and cannot push images,
  and the build cache is written only from `main`, so a PR cannot poison the layers `main` builds from.
- **Image provenance and signing status.** Images are published to `ghcr.io/honeybee-studio/honeybeepf-llm`,
  tagged `latest` and `sha-<commit>` from `main` (`docker-publish.yml`) and with the release version from a
  `v*.*.*` tag (`release.yml`). Build provenance attestation is
  **explicitly disabled** (`provenance: false` on `docker/build-push-action` in both `docker-publish.yml` and
  `release.yml`), and **the images are not signed** — there is no cosign or sigstore step in any workflow. A
  deployer who needs a signed, attested image should build and sign it themselves from the pinned source.
- **Binary fixture provenance.** A CI step (`ci/check-binary-provenance.sh`, run from `docker-publish.yml`)
  checks that every binary test fixture in the repo carries a documented sha256 and regeneration command, so
  binary blobs cannot enter the source tree unexplained. This is a source-tree check, not image attestation.
- **Reporting a vulnerability.** Use GitHub private vulnerability reporting as described in
  [`SECURITY.md`](../SECURITY.md); the scope, what counts as a vulnerability, and the disclosure process are
  defined there.

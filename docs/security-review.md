# Security review

This document is for the security and platform teams who approve the agent before it runs. honeybeepf-llm is a DaemonSet
with eBPF capabilities, the host PID namespace and a read-only view of the host filesystem. It attaches eBPF programs to
OpenSSL and to a few kernel hooks, reads TLS plaintext in memory on the node to find LLM API calls, and exports token
and cost metrics over OTLP. It is not a proxy and does not change the data or the result of any call.

Every statement below is drawn from the chart, the code or the CI workflows, with the file named inline, and it
describes the chart's defaults unless an option is named. Where something has not been tested, it says so. Section 8
lists the known gaps in one place. To report a suspected vulnerability, follow [`SECURITY.md`](../SECURITY.md) (GitHub
private vulnerability reporting).

## At a glance

| Question | Answer |
|---|---|
| Privileged container? | Not `privileged: true`: `capabilities.drop: [ALL]`, then `BPF`, `PERFMON`, `SYS_ADMIN`, `SYS_PTRACE`, `NET_ADMIN` and `SYS_RESOURCE` are added back. With uid 0 and `hostPID`, a compromised agent has root-level access to the node (section 2). |
| Runs as root? | Yes, uid 0 in the container, with `hostPID: true`. |
| Host filesystem? | `/` is mounted read-only at `/host`, which exposes the whole host filesystem for reading; `/proc/<pid>/root` also reaches other processes' filesystems (section 2). |
| Reads plaintext? | Yes, in memory only: the decrypted bytes of TLS connections in processes that use the probed OpenSSL, until a connection is known not to be an LLM call (section 3). |
| What leaves the node? | OTLP metrics over plaintext gRPC, and the container's log lines; never request or response bodies (section 3). |
| Listening ports? | None. |
| In the request path? | No proxy, but the uprobes run in the application's own thread on every `SSL_read`/`SSL_write` (section 5). |
| Signed images? | No. No signing, no SBOM and no build provenance (section 7). |

## 1. Kernel hooks

With chart defaults no program attaches: the LLM probe is off until `builtinProbes.llm.enabled=true`, which the install
commands in the README and `docs/install.md` set. The other groups below are opt-in as noted. Every program only reads
or observes: none changes a syscall result, drops or redirects a packet, or modifies data.

| Program (eBPF) | Type | Attaches to | Purpose | What it reads |
|---|---|---|---|---|
| `probe_ssl_rw_enter` | uprobe | `SSL_read`, `SSL_write` entry | record the call's buffer pointer and length | arg0/arg1 (`SSL*`, buffer pointer) |
| `probe_ssl_read_exit` / `probe_ssl_write_exit` | uretprobe | `SSL_read` / `SSL_write` return | copy the decrypted bytes of the call | up to 16 KB of a read, 4 KB of a write |
| `probe_ssl_rw_ex_enter` | uprobe | `SSL_read_ex`, `SSL_write_ex` entry | same, for the `_ex` variants (byte count via arg3) | arg0/arg1/arg3 pointers |
| `probe_ssl_read_ex_exit` / `probe_ssl_write_ex_exit` | uretprobe | `SSL_read_ex` / `SSL_write_ex` return | copy the decrypted bytes | up to 16 KB of a read, 4 KB of a write |
| `probe_ssl_do_handshake_enter` / `_exit` | uprobe / uretprobe | `SSL_do_handshake` | start the latency timer; mark a reused `SSL*` | the `SSL*` pointer only |
| `probe_ssl_free` | uprobe | `SSL_free` | end the connection's stream (the address can be reused) | the `SSL*` pointer only |
| `probe_ssl_task_exit` | tracepoint | `sched:sched_process_exit` | clear the in-flight call state of an exiting thread | thread id only |
| `probe_exec` | tracepoint | `sched:sched_process_exec` | tell userspace to attach SSL probes to the new process | pid and the process `comm` |
| `probe_lib_open_fentry` / `probe_lib_open` | fentry, kprobe fallback | `do_sys_openat2` | notice a `libssl.so*` open before its first SSL call | the opened path |
| `honeybeepf_llm_group_exit` | kprobe | `do_group_exit` | mark a process group that is exiting | pid/tgid |
| `honeybeepf_llm_process_exit` | kprobe | `acct_collect` | report a process exit at its last thread (needs `CONFIG_BSD_PROCESS_ACCT`) | pid/tgid, exit code, `comm`, cgroup id, start and exit time |
| `honeybeepf_llm_oom_mark_victim` | tracepoint | `oom:mark_victim` | flag a pid the OOM killer chose, so its exit is attributed | the victim pid |
| `sys_enter_openat` | tracepoint | the `openat` syscall | record opens of a short list of watched paths | the opened path (matched against the list) |
| `plaintext_connect4` | cgroup (connect4) | the host's cgroup v2 root | note the connecting task when the destination is a watched endpoint | destination IPv4/port, task pid/cgroup |
| `plaintext_sockops` | sock_ops | the host's cgroup v2 root | add a watched socket to the sockhash; remove it and end its stream on close | the socket 4-tuple |
| `plaintext_skmsg` | sk_msg | the plaintext sockhash | copy each send on a watched socket (always `SK_PASS`) | up to 4 KB of the sent bytes |
| `plaintext_skskb` | sk_skb (`stream_verdict`) | the plaintext sockhash | copy each receive on a watched socket (always `SK_PASS`) | up to 16 KB of the received bytes |

- **LLM programs** (the SSL rows, `probe_ssl_task_exit`, `probe_exec` and the libssl-open watch) attach when
  `builtinProbes.llm.enabled` is true. The uprobe table is in
  `honeybeepf-llm/honeybeepf-llm/src/probes/builtin/llm/mod.rs`, the exec and libssl-open attach points in
  `honeybeepf-llm/honeybeepf-llm/src/probes/mod.rs`, and the eBPF programs under
  `honeybeepf-llm/honeybeepf-llm-ebpf/src/probes/builtin/`. The agent also attaches to a process's own executable when
  it exports `SSL_read`/`SSL_write` (official Node.js builds), switched by `builtinProbes.llm.executables` (default
  true). Without kernel BTF the libssl-open watch falls back from fentry to a kprobe.
- **Plaintext programs** load only when a provider sets `plaintext` and the node has cgroup v2 on kernel 5.10 or later;
  otherwise they stay off and the agent logs why (`plaintext::start`, called from
  `honeybeepf-llm/honeybeepf-llm/src/lib.rs`). They attach at the host's cgroup v2 root through the `/host` mount, so
  they see the connections of every process on the node (if `/host/sys/fs/cgroup` is not a cgroup2 mount they fall back
  to the agent's own `/sys/fs/cgroup` and see only the agent pod; the log names the path), and act only on sockets to
  the configured endpoints: in-cluster Services, or any other host the agent resolves by DNS
  (`honeybeepf-llm/honeybeepf-llm/src/plaintext.rs`). `plaintext_connect4` always allows the connect and both data
  programs always return `SK_PASS`. Watched sockets do pass through the kernel's sockmap layer, and `sk_msg` may
  linearize a message (`bpf_msg_pull_data`) before copying it; the bytes delivered are unchanged.
- **Process-lifecycle programs** (`honeybeepf_llm_group_exit`, `honeybeepf_llm_process_exit`,
  `honeybeepf_llm_oom_mark_victim`) attach only when `builtinProbes.process_lifecycle.enabled` is true (default false).
  When on, they export `process_exit{cause, exit_code, cgroup_id}` and `process_lifetime_seconds`, and log the process
  `comm`. A kernel without one of the hooks is logged and skipped; any other attach error stops the agent.
- **The `openat` file-access tracepoint** is off by default (`builtinProbes.filesystem.file_access.enabled: false`).
  When on, it records opens of the configured `watched_paths` only (the chart's default list is SSH-key paths), capped
  at 128 paths (`MAX_WATCHED_PATHS`, `.../probes/builtin/file_access.rs`). It reads the path, not file contents, and
  exports `file_access_events` with the file name, open flags, process name, cgroup id, namespace and pod, and logs each
  open at info level.
- **`customProbes`** (kprobes, uprobes, tracepoints) in `values.yaml` reaches the agent's ConfigMap, but no code reads
  it, so it attaches nothing.

## 2. Privileges

The container's `securityContext` is in `charts/honeybeepf-llm/values.yaml`: `readOnlyRootFilesystem: true`,
`seccompProfile.type: RuntimeDefault`, `capabilities.drop: [ALL]`, then these are added back:

| Capability | Why it is needed |
|---|---|
| `BPF` | create the eBPF maps and load the programs |
| `PERFMON` | attach the uprobes, tracepoints and fentry program, and read user memory (`bpf_probe_read_user`) |
| `SYS_ADMIN` | the uprobe `perf_event_open`: the Ubuntu 7.0 kernel refuses it with `PERFMON` alone (the chart comment links the kernel bug). On 6.9 and later it also satisfies the network checks below |
| `SYS_PTRACE` | read other processes' `/proc/<pid>/root` and `/proc/<pid>/maps` to find their libssl (`.../probes/discovery.rs`) |
| `NET_ADMIN` | create the plaintext sockhash map, which exists even when plaintext capture is off; kernels before 6.9 require it for that map type |
| `SYS_RESOURCE` | raise `RLIMIT_MEMLOCK` at startup; kernels before 5.11 charge BPF map memory to it rather than to the cgroup |

CI loads and attaches the agent with only these capabilities (read from `values.yaml` by `ci/smoke.sh`) on kernels 5.10
to 7.2, RHEL 8.10, 9.6 and 9.7, and bpf-next (`.github/workflows/kernel-matrix.yml`).

**If the agent is compromised**, these capabilities with uid 0 and `hostPID` amount to root on the node. For example,
`SYS_ADMIN` lets it load BPF programs that read or write any process's memory, and `SYS_PTRACE` with `hostPID` reaches
every process's `/proc/<pid>/root`, including filesystems that are not read-only. The agent's own programs only read,
but the boundary is the agent binary and image, not the kernel.

The agent runs as uid 0: the image sets no `USER` and the chart sets no `runAsUser` or `runAsNonRoot`. Kubernetes treats
`allowPrivilegeEscalation` as true when `SYS_ADMIN` is added. The service account token is mounted (no
`automountServiceAccountToken: false`). No AppArmor profile is set, so the runtime default applies. With `debug: true`
the chart also sets `seLinuxOptions.type: spc_t`, an unconfined SELinux type; it is off by default.

Host access, from `charts/honeybeepf-llm/templates/daemonset.yaml`:

- `hostPID: true`, to read `/proc/<pid>` of processes in other pods for libssl discovery and pod attribution.
- `/` mounted read-only at `/host`. The agent reads only the host's cgroup tree there (`/host/sys/fs/cgroup`, for pod
  attribution and the plaintext attach point; `honeybeepf-llm/honeybeepf-llm/src/k8s.rs`), but the mount exposes the
  whole host filesystem for reading, including files such as other pods' mounted Secrets under `/var/lib/kubelet/pods`.
- `/sys/kernel/tracing` and `/sys/kernel/debug`, read-only, for tracefs. A prices ConfigMap is mounted read-only only
  when `builtinProbes.llm.prices` is set. No host path is mounted writable.
- No `privileged: true`, no `hostNetwork`, no `hostIPC`.

Kubernetes RBAC, from `charts/honeybeepf-llm/templates/clusterrole.yaml`, created with the chart's ServiceAccount
(`serviceAccount.create`, default true):

| API group | Resources | Verbs | Why | When granted |
|---|---|---|---|---|
| `""` (core) | `pods` | get, list, watch | map a pid/cgroup to a pod for metric labels | always |
| `batch` | `jobs` | get | a pod references its Job; the agent reads it to find the CronJob | always |
| `""` (core) | `services` | get, list, watch | resolve a configured plaintext provider's in-cluster hosts to ClusterIPs | only when a provider sets `plaintext` |
| `discovery.k8s.io` | `endpointslices` | get, list, watch | resolve those Services to ready pod IPs | only when a provider sets `plaintext` |

The role has no write verbs and no access to Secrets. It is cluster-wide: the agent filters pods to its own node in its
watch (`k8s.rs`), but the token can read every pod spec in the cluster, including literal environment values. `watch` on
services and endpointslices is granted but the code only gets and lists them.

## 3. Data handling

- **Plaintext stays in memory on the node.** The uprobes (and, for plaintext endpoints, the socket programs) copy
  decrypted request and response bytes, which include headers such as `Authorization`, into a BPF ring buffer (8 MB for
  TLS, 4 MB for plaintext when it is on). Userspace parses the HTTP for token counts and drops the bytes. Captured
  bytes are bounded at the kernel (16 KB per read, 4 KB per write), and in userspace each connection's buffer is capped
  at 8 MB for a request and 16 MB for a response.
- **Which TLS traffic reaches the agent.** The uprobes fire for every TLS connection in a process that uses the probed
  OpenSSL, not only LLM calls. A connection's bytes are copied until the kernel knows its class (`ssl_class.rs`,
  `conn_filter.rs`): HTTP/1.1 connections to hosts no provider serves, and server-side connections, are dropped in the
  kernel after their first request; other HTTP/1.1 requests are dropped until the next POST; HTTP/2 connections and TLS
  that is not HTTP are not filtered in the kernel and keep being copied to the agent, which discards what it cannot
  match to a provider.
- **What is exported.** OTLP metrics only (`honeybeepf-llm/honeybeepf-llm/src/telemetry.rs`): request, token, latency
  and cost series with labels such as namespace, pod, workload, model, status, attempt, reason, operation, modality,
  transport, token type and cost type; an info series with the configured provider names and a config hash; and the
  price table version. `builtinProbes.llm.podLabels` (opt-in) adds chosen pod labels. The model label comes from the
  application's request and response JSON, so an application controls its value; the OpenTelemetry SDK caps each
  instrument at 2,000 series.
- **Logs.** The container logs to stdout, which the kubelet stores on the node. At the default `info` level a recorded
  call logs its model, request path (query string removed), pid, pod, latency and token counts. No log statement prints
  a request or response body or header; the model name and an OpenAI Responses background call's id are the only logged
  values taken from a body. A test feeds fake API keys in the three forms providers use (OpenAI `Authorization: Bearer`,
  Gemini `?key=`/`x-goog-api-key`, Bedrock SigV4 presigned query) and asserts the logs never contain them
  (`test_credentials_stay_out_of_logs`, `.../probes/builtin/llm/processor/tests.rs`); it covers the HTTP/1.1 path.
- **Network.** No listening port: the chart defines no Service and the container exposes no port; the liveness and
  readiness checks are exec probes. Outbound connections are the OTLP collector, the Kubernetes API, and DNS for
  plaintext hosts outside the cluster. OTLP is sent as plaintext gRPC: the agent is built without the exporter's TLS
  support (`honeybeepf-llm/honeybeepf-llm/Cargo.toml`), and the chart has no authentication settings for it. With the
  default empty `output.otlp.endpoint`, nothing is exported.
- **Disk.** The root filesystem and every host mount are read-only; the agent writes no files. (`/proc/<pid>/root` is a
  separate path; see "If the agent is compromised" in section 2.)
- The agent reads TLS plaintext on its own node by design; keeping that plaintext inside the agent is the security
  boundary, as stated in `SECURITY.md`.

## 4. Resource use per node

Measured overhead is in [docs/benchmark.md](benchmark.md). The numbers below are from its v0.2.0 run: kernel 7.0.0-38 on
a 4-vCPU VM, Python clients against a local mock server, plaintext capture off and no OTLP export. Use that document as
the source of truth.

| Measure | Value (from docs/benchmark.md) |
|---|---|
| Agent CPU | 0.067 CPU-seconds per 1,000 requests in the `llm` case; 0.008 at the `max` rate |
| Memory (RSS anon / file) | ~4.2 MB anonymous, ~26.8 MB file-backed; PSS ~20.6 MB |
| BPF map memory (`memlock`) | ~18.8 MB with plaintext capture off |
| Application impact | −6.5% throughput in the saturating `max` case; p99 latency +0.6 ms at 200 req/s, which the benchmark treats as noise |

The chart requests 200m CPU / 512Mi and limits 1 CPU / 1Gi per node (`charts/honeybeepf-llm/values.yaml`).

**Kernel memory is fixed by the map sizes.** The ring buffers are 8 MB for TLS, 4 MB for plaintext when it is on (4 KB
otherwise), 1 MB each for process exit and file access, and 64 KB each for exec and libssl opens. The per-thread maps of
in-flight SSL calls hold 10,240 entries, the connection-class map 32,768 (LRU), the process start-time and name maps
10,240 each, and the libssl-open map 4,096. With plaintext capture on, the socket, connection and connect-handoff maps
hold 65,536 entries each and the endpoint map 8,192; with it off they hold one entry each. The extra memory when it is
on has not been measured. Sizes are in `honeybeepf-llm/honeybeepf-llm-ebpf/src/probes/builtin/` and
`honeybeepf-llm/honeybeepf-llm-common/src/lib.rs`.

**Userspace memory is bounded per connection, not per node.** The agent keeps one stream per open connection, with no
limit on the number of connections, and frees a stream after 5 minutes idle. Each stream can hold up to 8 MB of request
and 16 MB of response (HTTP/2: up to 32 MB per connection); only speculative HTTP/2 buffering has a node-wide cap (64
MB). Many concurrent large calls can therefore reach the 1Gi limit and get the agent OOM-killed; applications are
unaffected (section 5).

## 5. Failure behaviour

The agent is not a proxy, so a slow, crashed or detached agent never blocks a call. It is not free for the application
either: the uprobes run synchronously in the calling thread on every `SSL_read`/`SSL_write`, so they add CPU and latency
whatever state the agent is in (section 4 has the measured cost). If the agent cannot keep up, the kernel drops events
at the ring buffer (`reserve_fail`) rather than slowing the application, and those calls go unrecorded. With plaintext
capture on, `connect4` also runs on every IPv4 connect on the node and `sock_ops` on every TCP connection setup,
returning at once for destinations that are not watched.

- **Crash or OOM-kill:** the kubelet restarts the container; metrics stop until it is back. Nothing is pinned, so the
  programs detach when the process exits.
- **Fatal at startup** (the agent exits and the pod goes into CrashLoopBackOff; applications are unaffected): loading
  the BPF object or creating a map (for example a missing capability, or a memlock limit too low on a kernel before
  5.11), attaching the exec tracepoint, file-access attach errors (including more than 128 watched paths), and
  process-lifecycle attach errors other than a missing hook.
- **Logged and skipped:** a uprobe attach to one library, no libssl on the node yet (the agent waits for one), the
  task-exit tracepoint, the libssl-open watch, plaintext capture, and a process-lifecycle hook the kernel lacks (for
  example `acct_collect` without `CONFIG_BSD_PROCESS_ACCT`).
- **A startup error after programs attach** sets a global shutdown flag through a drop guard, so the ring-buffer reader
  threads stop and the process exits with an error line rather than hanging (`ShutdownOnDrop` in `HoneyBeeEngine::run`,
  `honeybeepf-llm/honeybeepf-llm/src/lib.rs`).
- **Health checks do not detect a hung agent.** The liveness and readiness probes run `cat /proc/self/status`, which
  reads `cat`'s own status and always succeeds, so they only show that the container can run a command.
- **Visibility:** a call the agent saw but could not record is counted under `llm_responses_unparsed_total` with a
  reason, and kernel-side losses under `llm_events_total` (see [docs/metrics.md](metrics.md)). These reach a collector
  only when `output.otlp.endpoint` is set; otherwise only the logs show them.

## 6. Coexistence with other eBPF tools

- **Other uprobes on libssl.** Several tools can attach uprobes to the same `SSL_read`/`SSL_write`. The agent attaches
  each library file once, keyed by device and inode (on overlayfs, by inode, size and times). The file on disk is not
  changed, but the kernel writes a breakpoint into each process's in-memory copy of the probed functions, which a
  code-integrity checker or debugger looking at those addresses can see.
- **Other tracepoint and kprobe tools.** The kernel lets several programs share a tracepoint or kprobe, so the agent's
  hooks (`sched_process_exec`, `sched_process_exit`, `do_sys_openat2`, and the opt-in `do_group_exit`, `acct_collect`,
  `oom:mark_victim` and `openat`) run alongside another tool's on the same points.
- **CNIs and other cgroup programs.** The plaintext `connect4` and `sock_ops` programs attach at the host's cgroup v2
  root through a BPF link (`CgroupAttachMode::Single` in `honeybeepf-llm/honeybeepf-llm/src/plaintext.rs`; plaintext
  needs kernel 5.10 or later). Through a link the kernel attaches them as multi-program, so they run alongside other
  multi-program or link attachments; while they are attached, another tool's later attach of an exclusive (non-multi)
  program at the same cgroup fails. Running them next to a CNI that attaches its own cgroup programs has not been
  tested. If the attach fails, plaintext capture stays off with a log line and TLS capture is unaffected. They load only
  when plaintext capture is configured.
- **Sockmap.** The kernel attaches one set of sockmap programs per socket, so a socket the agent watches cannot also be
  accelerated by another tool's sockmap (some service meshes and CNIs use one), and a socket already in another tool's
  sockmap is not watched. This applies only to sockets to configured plaintext endpoints and has not been tested with
  such a tool.
- **Maps.** The agent pins no maps and uses no shared bpffs path, and it reads no other tool's maps. Like any BPF map,
  its maps, including the TLS ring buffer that carries plaintext, can be opened by ID by another process on the node
  that has `CAP_SYS_ADMIN`.

## 7. Supply chain

- **Pinned actions.** Every GitHub Action in `.github/workflows/` is pinned to a full commit SHA with the version in a
  trailing comment, and Dependabot (`.github/dependabot.yml`) proposes minor and patch updates to them weekly.
- **Unpinned build inputs.** The image's base images (`rustlang/rust:nightly-trixie` for the build, `debian:trixie-slim`
  at runtime, in `Dockerfile`) are referenced by tag, not digest; the nightly toolchain, the apt packages and
  `cargo-chef` are not pinned. `bpf-linker` is pinned in the image build.
- **No dependency scanning in CI.** Dependabot covers GitHub Actions only. No workflow runs `cargo audit`, `cargo deny`
  or an image scanner.
- **Workflow tokens.** Each workflow sets top-level `permissions: contents: read`. The image-publish job in
  `docker-publish.yml` and the image and chart jobs in `release.yml` raise it to `packages: write`, and the GitHub
  release job in `release.yml` to `contents: write`. `build.yml` also declares a `contents: write` release job, which
  never runs today: it needs the build job, which is skipped on pushes and pull requests. Fork pull requests get a
  read-only token and cannot push images; pull requests from branches in the repository push a `pr-<number>` image. In
  the workflow as written only `main` writes the build cache; since a pull request from a branch in the repository runs
  with `packages: write`, anyone who can push a branch could change that.
- **Images and tags.** Images are published to `ghcr.io/honeybee-studio/honeybeepf-llm`, tagged `latest` and `sha-<short
  commit>` from `main` (`docker-publish.yml`), and `X.Y.Z` and `X.Y` from a `vX.Y.Z` tag on `main` whose version matches
  the crate and chart (`release.yml`). The same public repository also holds the `pr-<number>` images and the
  `buildcache` tag, so deploy a release tag or a digest. The chart is published to
  `oci://ghcr.io/honeybee-studio/charts`.
- **No signing, SBOM or provenance.** Build provenance is disabled (`provenance: false` in both workflows), and neither
  the images nor the Helm chart are signed; no workflow produces an SBOM. A deployer who needs a signed, attested image
  should build and sign it from the source.
- **Binary fixture provenance.** `ci/check-binary-provenance.sh`, run from `docker-publish.yml`, checks that every
  binary test fixture in the repository carries a documented sha256 and regeneration command. This is a check on the
  source tree, not on the image.

## 8. Known gaps

These are open and listed here so a reviewer does not have to find them:

- A compromised agent has root-level access to the node through its capabilities, uid 0 and `hostPID` (section 2).
- The bytes of all TLS connections in probed processes reach the agent's memory until classified; HTTP/2 and non-HTTP
  TLS are not filtered in the kernel (section 3).
- The health probes do not detect a hung agent (section 5).
- The agent runs as root, and the `/host` mount exposes the whole host filesystem for reading; only the cgroup tree is
  used (section 2).
- The service account can read every pod spec in the cluster, not only those on its node (section 2).
- Userspace memory has no node-wide cap, so heavy concurrent traffic can OOM-kill the agent (section 4).
- OTLP is sent as plaintext gRPC, without TLS or authentication (section 3).
- `customProbes` in `values.yaml` is accepted but does nothing (section 1).
- Images and the chart are not signed, there is no SBOM, base images are not pinned by digest, and CI runs no dependency
  scan (section 7).

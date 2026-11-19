# Security review

For the security and platform teams who approve the agent before it runs. Every statement comes from the chart, the
code or the CI workflows and describes the chart's defaults unless an option is named; what has not been tested says so.
[Known gaps](#8-known-gaps) collects the open issues. To report a vulnerability, follow
[`SECURITY.md`](https://github.com/honeybee-studio/honeybeepf-llm/blob/main/SECURITY.md).

## At a glance

| Question | Answer |
|---|---|
| What is it? | A DaemonSet that reads TLS plaintext in memory on its node to find LLM API calls and exports token and cost metrics. Not a proxy; it never changes a call or its result. |
| Privileged? | No `privileged: true`. All capabilities dropped, six added back ([Privileges](#2-privileges)). AppArmor unconfined for the agent container. |
| Runs as root? | Yes, uid 0, with `hostPID`. A compromised agent has root-level access to the node. |
| Host filesystem? | Only the cgroup tree, read-only. `/proc/<pid>/root` still reaches other filesystems through `hostPID`. |
| Reads plaintext? | Yes, in memory only, until a connection is known not to be an LLM call ([Data handling](#3-data-handling)). |
| What leaves the node? | OTLP metrics and log lines. Never request or response bodies, headers or keys. |
| What did it do? | Audit events log every library and executable it attached to, plaintext capture and the providers and prices it loaded, as JSON with `LOG_FORMAT=json` ([Logs](guides/logs.md)). |
| Listening ports? | None. |
| In the request path? | No proxy, but the uprobes run in the application's thread on every `SSL_read`/`SSL_write` ([Failure behaviour](#5-failure-behaviour)). |
| Image? | Chainguard `glibc-dynamic`: glibc, libgcc and CA certificates, no shell or package manager; no known vulnerabilities in a Trivy scan when built. |
| Signed? | Images (with SBOM and provenance) and the release chart, by keyless cosign, from 0.3.1 ([Supply chain](#7-supply-chain)). |

## 1. Kernel hooks

Every program only reads or observes: none changes a syscall result, drops or redirects a packet, or modifies data.

| Group | Programs | Attach to | Read | On by default |
|---|---|---|---|---|
| LLM, OpenSSL | `probe_ssl_*` | `SSL_read`, `SSL_write` and their `_ex` variants, `SSL_do_handshake`, `SSL_free`, in each `libssl` or executable that exports them | up to 16 KB of a read and 4 KB of a write; otherwise pointers only | yes |
| LLM, Go | `probe_go_tls_*` | `crypto/tls` `(*Conn).Write`, `Read`, each return of `Read`, and `Close` in Go 1.20+ executables | the same limits | yes (`builtinProbes.llm.executables`) |
| LLM, built-in TLS | `probe_aead_*` | the `seal`, `open`, `init` and `cleanup` functions of a BoringSSL or aws-lc built into an executable, found by the AEAD table scan ([Data handling](#3-data-handling)); `tcp_sendmsg` and `tcp_recvmsg` (kprobes) once a table is found | the same limits; socket pointers | no (`builtinProbes.llm.aead`, experimental) |
| LLM, discovery | `probe_exec`, `probe_ssl_task_exit`, `probe_lib_open_fentry` (kprobe fallback without BTF) | `sched_process_exec`, `sched_process_exit`, `do_sys_openat2` | pid, `comm`, thread id, opened path | yes |
| Plaintext | `plaintext_connect4`, `plaintext_sockops`, `plaintext_skmsg`, `plaintext_skskb` | the host's cgroup v2 root and a sockhash | up to 4 KB of a send and 64 KB of a receive on watched sockets | only when a provider sets `plaintext` |

- **Plaintext programs** act only on sockets to the configured endpoints. `plaintext_connect4` always allows the
  connect and the data programs always return `SK_PASS`; `sk_msg` may linearize a message before copying it, and the
  bytes delivered are unchanged. They need cgroup v2 and Linux 5.10+, else they stay off with a log line.

<details markdown>
<summary>Every program, one by one</summary>

| Program | Type | Attaches to | Purpose | Reads |
|---|---|---|---|---|
| `probe_ssl_rw_enter` | uprobe | `SSL_read`, `SSL_write` entry | record the buffer pointer and length | `SSL*`, buffer pointer |
| `probe_ssl_read_exit` / `probe_ssl_write_exit` | uretprobe | `SSL_read` / `SSL_write` return | copy the decrypted bytes | up to 16 KB of a read, 4 KB of a write |
| `probe_ssl_rw_ex_enter` | uprobe | `SSL_read_ex`, `SSL_write_ex` entry | the same for the `_ex` variants | `SSL*`, buffer and count pointers |
| `probe_ssl_read_ex_exit` / `probe_ssl_write_ex_exit` | uretprobe | `SSL_read_ex` / `SSL_write_ex` return | copy the decrypted bytes | up to 16 KB of a read, 4 KB of a write |
| `probe_ssl_do_handshake_enter` / `_exit` | uprobe / uretprobe | `SSL_do_handshake` | start the latency timer; mark a reused `SSL*` | the `SSL*` pointer |
| `probe_ssl_free` | uprobe | `SSL_free` | end the connection's stream | the `SSL*` pointer |
| `probe_go_tls_*` | uprobe | Go `crypto/tls` `(*Conn).Write`, `Read`, `Read` returns, `Close` | the same, for Go executables | buffer contents within the same limits |
| `probe_aead_seal` | uprobe | AEAD `seal` entry | copy a TLS record's plaintext before it is encrypted | the same limits; argument pointers |
| `probe_aead_open_enter` / `probe_aead_open_exit` | uprobe / uretprobe | AEAD `open` | copy a TLS record's plaintext after it is decrypted | the same limits |
| `probe_aead_init` / `probe_aead_cleanup` | uprobe | AEAD context init and cleanup | tie a cipher context to its socket, and forget it | context pointer |
| `probe_aead_tcp_sendmsg` / `probe_aead_tcp_recvmsg` | kprobe | `tcp_sendmsg`, `tcp_recvmsg` on the whole node | learn the socket of the records a thread sealed or will open; a map lookup for other processes | socket pointer |
| `probe_ssl_task_exit` | tracepoint | `sched:sched_process_exit` | clear an exiting thread's in-flight state | thread id |
| `probe_exec` | tracepoint | `sched:sched_process_exec` | attach SSL probes to new processes | pid, `comm` |
| `probe_lib_open_fentry` / `probe_lib_open` | fentry, kprobe fallback | `do_sys_openat2` | notice a `libssl.so*` open before its first SSL call | the opened path |
| `plaintext_connect4` | cgroup connect4 | host cgroup v2 root | note the task connecting to a watched endpoint | destination IPv4 and port, pid, cgroup |
| `plaintext_sockops` | sock_ops | host cgroup v2 root | add a watched socket to the sockhash; end its stream on close | the socket 4-tuple |
| `plaintext_skmsg` | sk_msg | the plaintext sockhash | copy each send | up to 4 KB |
| `plaintext_skskb` | sk_skb `stream_verdict` | the plaintext sockhash | copy each receive | up to 64 KB |

Where in the code: the uprobe table in `honeybeepf-llm/honeybeepf-llm/src/probes/builtin/llm/mod.rs` and `gotls.rs`,
the exec and libssl-open attach points in `src/probes/mod.rs`, the eBPF programs under
`honeybeepf-llm/honeybeepf-llm-ebpf/src/probes/builtin/`, plaintext in `src/plaintext.rs`. If `/host/sys/fs/cgroup` is not a cgroup2 mount, the plaintext programs fall back
to the agent's own `/sys/fs/cgroup` and see only the agent pod; the log names the path.

</details>

## 2. Privileges

**Container.** `readOnlyRootFilesystem: true`, `seccompProfile: RuntimeDefault`, `capabilities.drop: [ALL]`, then:

| Capability | Why |
|---|---|
| `BPF` | create the maps and load the programs |
| `PERFMON` | attach uprobes, tracepoints and fentry, and read user memory |
| `SYS_ADMIN` | the uprobe `perf_event_open`, which Ubuntu's 7.0 kernel refuses with `PERFMON` alone |
| `SYS_PTRACE` | read other processes' `/proc/<pid>/root` and `/proc/<pid>/maps` to find their libssl; with the experimental AEAD scan on, also `/proc/<pid>/mem` of some processes ([Data handling](#3-data-handling)) |
| `NET_ADMIN` | create the plaintext sockhash at start, even with plaintext off; kernels before 6.9 only |
| `SYS_RESOURCE` | raise `RLIMIT_MEMLOCK`; kernels before 5.11 only |

CI loads and attaches the agent with exactly these capabilities on every kernel in the matrix (`ci/smoke.sh`).

| Setting | Value |
|---|---|
| User | uid 0: no `USER` in the image, no `runAsUser` or `runAsNonRoot` in the chart |
| `allowPrivilegeEscalation` | true in effect, because `SYS_ADMIN` is added |
| AppArmor | `Unconfined` for the agent container: Ubuntu's containerd profile blocks `/proc/<pid>/root`, and then only pods that share the agent image's libssl are recorded |
| SELinux | `spc_t` only with `debug: true` (off by default) |
| Service account token | mounted |
| `privileged`, `hostNetwork`, `hostIPC` | not set |

**Host access.**

| Access | Why |
|---|---|
| `hostPID: true` | read `/proc/<pid>` of other pods' processes for libssl discovery and pod attribution |
| host `/sys/fs/cgroup`, read-only at `/host/sys/fs/cgroup` | pod attribution and the plaintext attach point |
| `/sys/kernel/tracing`, `/sys/kernel/debug`, read-only | tracefs |
| prices ConfigMap, read-only | only when `builtinProbes.llm.prices` is set |

No host path is mounted writable.

**Kubernetes RBAC** (a ClusterRole with the chart's ServiceAccount):

| Resources | Verbs | Why | Granted |
|---|---|---|---|
| `pods` | get, list, watch | map a process to its pod | always |
| `batch/jobs` | get | find a Job's CronJob | always |
| `services` | get, list, watch | resolve plaintext providers' hosts | only with a `plaintext` provider |
| `discovery.k8s.io/endpointslices` | get, list, watch | resolve those Services to pod IPs | only with a `plaintext` provider |

No write verbs and no Secrets. The role is cluster-wide: the agent watches only its own node's pods, but the token can
read every pod spec in the cluster, including literal environment values. `watch` on services and endpointslices is
granted but unused.

!!! danger "If the agent is compromised"
    These capabilities with uid 0 and `hostPID` amount to root on the node. `SYS_ADMIN` allows BPF programs that read
    or write any process's memory, and `SYS_PTRACE` with `hostPID` reaches every process's `/proc/<pid>/root`,
    including the host's own and filesystems that are not read-only. The agent's programs only read, but the boundary is
    the agent binary and image, not the kernel.

## 3. Data handling

| | |
|---|---|
| Plaintext | decrypted request and response bytes, headers such as `Authorization` included, copied into a ring buffer (8 MB for TLS, 4 MB for plaintext when on), parsed for token counts and dropped |
| Bounds | 16 KB per read, 4 KB per write and 64 KB per plaintext receive in the kernel; 8 MB of request and 16 MB of response per connection in user space |
| Exported | OTLP metrics only: request, token, latency and cost series ([labels](metrics.md)), a provider-config info series and the price table version |
| Logged | per call: model, request path without the query string, pid, pod, latency and token counts. Never a body or header; the model and an OpenAI background call's id are the only values taken from a body |
| Network | no listening port. Outbound: the OTLP collector, the Kubernetes API, and DNS for plaintext hosts outside the cluster |
| Disk | read-only root filesystem and host mounts; the only file written is the heartbeat in a 1 MiB in-memory `emptyDir` |
| Scope | every pod and host process by default; with `builtinProbes.llm.namespaces`, TLS data is copied only from the containers of pods in those namespaces, checked in the kernel by cgroup before any byte is read ([guide](guides/namespace-scope.md)) |

**Process memory read by the AEAD table scan.** Experimental and off by default (`BUILTIN_PROBES__LLM_AEAD`). It
finds the cipher table of a BoringSSL or aws-lc built into a program such as Claude Code, which exports no TLS function.

| | |
|---|---|
| Which processes | only those whose executable file has `BoringSSL` or `AWS-LC` in its `.rodata`, checked once per file by reading the file; with `BUILTIN_PROBES__LLM_AEAD_EXECUTABLES` (e.g. `claude,codex`), only executables of those names |
| Which memory | the executable's own mappings and the one zero-filled mapping right after them, read-only through `/proc/<pid>/mem`. Never the heap, stacks, libraries or other mappings. Rescans read only the pages of those mappings the process wrote (`/proc/<pid>/pagemap`) |
| How long | until a table filled at run time is found, once per executable file; its later processes are not read. Otherwise up to 20 scans per process over about 30 minutes |
| Kept | the file offsets of the table's functions, logged. The bytes read are dropped |

**Which TLS traffic reaches the agent.** The uprobes fire for every TLS connection in a probed process, not only LLM
calls. The kernel drops a connection's bytes once it knows its class:

| Connection | Filtered in the kernel |
|---|---|
| HTTP/1.1 to a host no provider serves, or a server-side connection | after its first request |
| other HTTP/1.1 | requests until the next POST |
| HTTP/2, and TLS that is not HTTP | not filtered: copied to the agent, which drops what matches no provider |

- **Labels an app controls.** The `model` label comes from the application's JSON; the OpenTelemetry SDK caps each
  instrument at 2,000 series.
- **Keys stay out of logs.** A test feeds fake keys in each provider's form (`Authorization: Bearer`, Gemini `?key=` and
  `x-goog-api-key`, Bedrock SigV4 presigned queries) and checks the logs never contain them; it covers HTTP/1.1.
- **OTLP transport.** Plaintext gRPC to a `host:port` or `http://` endpoint, the in-cluster default; TLS to `https://`,
  verified against `output.otlp.caSecret` or the system roots. `output.otlp.headersSecret` passes a token from a Secret.
  With no endpoint, nothing is exported.

The agent reads TLS plaintext on its node by design; keeping it inside the agent is the security boundary, as
`SECURITY.md` states.

<details markdown>
<summary>Where in the code</summary>

Kernel-side classification in `ssl_class.rs` and `conn_filter.rs`; exported series in
`honeybeepf-llm/honeybeepf-llm/src/telemetry.rs` (OTLP TLS in `otlp_tls`); the key test is
`test_credentials_stay_out_of_logs` in `src/probes/builtin/llm/processor/tests.rs`. Libssl discovery is in
`src/probes/discovery.rs` and pod attribution in `src/k8s.rs`.

</details>

## 4. Resource use per node

From the [benchmark](benchmark.md)'s v0.3.2 run: a 4-vCPU VM on kernel 7.0, Python clients against a local mock,
plaintext capture off, no OTLP export.

| Measure | Value |
|---|---|
| Agent CPU | 0.055 CPU-seconds per 1,000 LLM calls; 0.006 at the saturating rate |
| Memory | about 4.4 MB anonymous and 27.3 MB file-backed RSS; PSS about 21.4 MB |
| BPF maps | about 18.8 MB with plaintext capture off |
| Application | −6.6% throughput at the saturating rate; p99 +0.1 to +0.4 ms at 200 requests/s |
| Chart | requests 50m CPU and 128Mi, limits 1 CPU and 1Gi |

- **Kernel memory is fixed by the map sizes**: ring buffers of 8 MB for TLS, 4 MB for plaintext when on (4 KB off), 1 MB
  64 KB each for exec and libssl opens, plus hash maps of up to 65,536 entries.
  The extra memory with plaintext on has not been measured.
- **User-space memory is bounded per connection, not per node.** Each open connection can hold 8 MB of request and 16 MB
  of response (32 MB for HTTP/2), freed after 5 minutes idle. Many concurrent large calls can reach the 1Gi limit and
  get the agent OOM-killed; applications are unaffected. Only speculative HTTP/2 buffering has a node-wide cap, 64 MB.

<details markdown>
<summary>Map sizes</summary>

The per-thread maps of in-flight SSL calls hold 10,240 entries, the connection-class map 32,768 (LRU), the process
start-time and name maps 10,240 each, and the libssl-open map 4,096. With plaintext capture on, the socket, connection
and connect-handoff maps hold 65,536 entries each and the endpoint map 8,192; with it off, one entry each. Sizes are in
`honeybeepf-llm/honeybeepf-llm-ebpf/src/probes/builtin/` and `honeybeepf-llm/honeybeepf-llm-common/src/lib.rs`.

</details>

## 5. Failure behaviour

The agent is not a proxy, so a slow, crashed or detached agent never blocks a call. The uprobes still run in the
calling thread on every `SSL_read`/`SSL_write`, so they cost CPU and latency whatever state the agent is in. If the
agent falls behind, the kernel drops events at the ring buffer (`reserve_fail`) rather than slowing the application.

| Event | What happens |
|---|---|
| Crash or OOM kill | the kubelet restarts it; metrics stop until it is back; nothing is pinned, so the programs detach |
| Fatal at startup: loading the BPF object, creating a map (a missing capability, or memlock too low before 5.11), attaching the exec tracepoint | the pod goes into CrashLoopBackOff; applications are unaffected |
| One library fails to attach, no libssl yet (the agent waits), the task-exit tracepoint or libssl-open watch fails, plaintext capture fails | logged and skipped |
| An error after programs attach | a shutdown flag stops the reader threads and the process exits with an error line |
| The agent hangs | `/run/honeybeepf/heartbeat` is rewritten every 10 s only while every reader loop made progress in 30 s; the probes run `honeybeepf-llm --check-heartbeat`, which fails when it is 60 s old, and the kubelet restarts the agent. A reader takes at most 1,024 records a pass and beats each pass, so a busy agent keeps beating. A startup probe allows 5 minutes for the BPF load and attach |

- **Plaintext capture**, when on, also runs `connect4` on every IPv4 connect and `sock_ops` on every TCP setup on the
  node. On kernel 7.0 that measured about 60 ns per run, 0.6 µs per loopback connection, and no change in connection
  throughput.
- **Lost calls are counted**: in `llm_responses_unparsed_total` by reason, and kernel-side losses in `llm_events_total`
  ([metrics](metrics.md)), when an OTLP endpoint is set.
- Where in the code: the shutdown flag is `ShutdownOnDrop` in `HoneyBeeEngine::run`
  (`honeybeepf-llm/honeybeepf-llm/src/lib.rs`); the heartbeat is `src/health.rs`.

## 6. Coexistence with other eBPF tools

| Alongside | Effect |
|---|---|
| other uprobes on libssl | allowed; each library is probed once. The file on disk is unchanged, but each process's in-memory copy carries a breakpoint that a code-integrity checker can see |
| other tracepoint and kprobe tools | the kernel runs programs side by side on the same hook |
| CNIs and other cgroup programs (plaintext only) | the agent attaches multi-program through a link, so it runs alongside others; another tool's later exclusive attach at the same cgroup fails. Not tested next to a CNI. If the attach fails, plaintext stays off with a log line |
| another tool's sockmap (plaintext only) | one sockmap set per socket: a socket the agent watches cannot also be accelerated by another sockmap, and vice versa. Not tested |
| maps | the agent pins no maps and reads no other tool's maps. Any process with `CAP_SYS_ADMIN` on the node can open its maps by ID, including the TLS ring buffer |

## 7. Supply chain

| | |
|---|---|
| GitHub Actions | pinned to full commit SHAs; Dependabot proposes minor and patch updates weekly |
| Base images | build `rustlang/rust:nightly-trixie` and runtime `cgr.dev/chainguard/glibc-dynamic` (no shell or package manager; `USER 0:0` because it defaults to a non-root uid), pinned by digest, updated weekly by Dependabot. `bpf-linker` is pinned; apt packages and `cargo-chef` are not |
| Dependencies | `cargo audit` on every `Cargo.toml`/`Cargo.lock` change and weekly on `main`; fails on a vulnerability advisory. No `cargo deny` |
| Scanning | Trivy on every image CI builds, failing on a high or critical vulnerability that has a fix, and daily on `latest`, reported under Security; CodeQL (Rust, Python, Actions) through GitHub's default setup; OpenSSF Scorecard weekly, behind the README badge; secret scanning with push protection |
| Workflow tokens | `contents: read` by default; image and chart jobs get `packages: write`, the release job `contents: write` |
| Images | `ghcr.io/honeybee-studio/honeybeepf-llm`: `latest` and `sha-<commit>` from `main`, `X.Y.Z` and `X.Y` from release tags. The same repository holds `pr-<number>` images and a `buildcache` tag, so deploy a release tag or digest |
| Signing | keyless cosign on every pushed image (with SBOM and provenance) and on the release chart, from 0.3.1. [Verify](verify.md) has the commands |
| Binary files | `ci/check-binary-provenance.sh` requires a sha256 and a regeneration command for every binary fixture |

<details markdown>
<summary>Workflow token details</summary>

The image-publish job in `docker-publish.yml` and the image and chart jobs in `release.yml` raise the token to
`packages: write`, and the GitHub release job in `release.yml` to `contents: write`; only these jobs get
`id-token: write` for signing. `build.yml` declares a `contents: write` release job that never runs today. Fork pull
requests get a read-only token and cannot push images; pull requests from branches in the repository push a
`pr-<number>` image and run with `packages: write`, so anyone who can push a branch could change the build cache, which
the workflow writes only from `main`.

</details>

## 8. Known gaps

| Gap | Section |
|---|---|
| A compromised agent has root-level access to the node, including host files through `/proc/<pid>/root` | [2](#2-privileges) |
| The service account can read every pod spec in the cluster, not only its node's | [2](#2-privileges) |
| The agent container runs AppArmor-unconfined; no narrower profile is provided | [2](#2-privileges) |
| HTTP/2 and non-HTTP TLS in probed processes reach the agent's memory unfiltered | [3](#3-data-handling) |
| User-space memory has no node-wide cap, so heavy concurrent traffic can OOM-kill the agent | [4](#4-resource-use-per-node) |
| Images before 0.3.1 are not signed | [7](#7-supply-chain) |

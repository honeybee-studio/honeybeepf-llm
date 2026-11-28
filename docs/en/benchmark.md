# Overhead benchmark

What the agent costs on a node, measured by a script anyone can rerun:
[`tests/benchmark/overhead/overhead.py`](https://github.com/honeybee-studio/honeybeepf-llm/blob/main/tests/benchmark/overhead/overhead.py).
Results from each release are added below and old ones are kept, so changes over time stay visible.

## Method

The script runs on one Linux host or VM as root and needs only `python3` and the `openssl` CLI (no Docker, no
cluster). It starts a local HTTPS mock of the OpenAI Chat Completions API, then runs every case twice per round,
once without the agent and once with a freshly started agent, alternating which goes first, after one discarded
5 s pass. Each cell is the median of three rounds; token accuracy is summed over them.

| Metric | How it is measured |
|---|---|
| Agent CPU | user + system CPU time of the agent process (`/proc/<pid>/stat`) from the start of the load until its log is quiet for 2 s, per 1,000 requests. Uprobe time is spent in the traced processes, so it shows in the application columns instead |
| Agent memory | at the end of the load: `RssAnon` and `RssFile` from `/proc/<pid>/status`, `Pss` from `smaps_rollup`, and the `memlock` of every BPF map the agent holds (the figure `bpftool map` shows) |
| Application impact | requests/s and p50/p99 latency of the client processes, with and without the agent |
| Non-LLM TLS traffic | the `non-llm` case: 95% of requests go to a host no provider serves (GET and POST), 5% are LLM calls |
| Startup | agent start until `Active probe registered: llm` (libraries found and attached; the exec and open watchers load just after), with 1,000 idle processes on the node, 50 of them with `libssl` loaded |
| Accuracy under load | the sum of `Tokens:` in the agent's `LLM SUCCESS` lines over the sum of `prompt_tokens + completion_tokens` the mock returned; for a run below 100%, its `LLM UNPARSED` count and kernel counters (`[LLM stats]`) are listed |

Cases (4 client processes, each with one keep-alive connection per host):

| Case | Load | Requests |
|---|---|---|
| `llm` | fixed rate, 200 requests/s for 30 s | all LLM calls, half JSON and half SSE (chunked, one write per event) |
| `non-llm` | fixed rate, 200 requests/s for 30 s | 95% non-LLM, 5% LLM |
| `max` | as fast as the clients can go for 30 s | all LLM calls |

Every LLM call asks for a random `max_tokens` (1–512) from a seeded generator; the mock returns exactly that many
completion tokens and 13 prompt tokens, a fresh response id, and the response headers api.openai.com sends
(`openai-version`, `openai-organization`, `openai-processing-ms`, `x-request-id`). The tokens a tool should record are
the ones in the responses the clients received. The seed, rate, duration, rounds, process count and a delay before
the clients start (`--client-delay`, 0 by default) are flags; the defaults are the numbers above.

The agent runs as the Helm chart runs it: `BUILTIN_PROBES__LLM=true`, `RUST_LOG=info`. No OTLP endpoint is set, so
export cost is not included.

Things this does not show:

- Agent CPU has a fixed part that does not grow with the request count: periodic timers and the ring-buffer
  readers' per-wake cost. The readers drain in batches (drain, then sleep 10 ms, so one wake covers more events
  as the rate rises) and otherwise block on the ring fd, so per request the CPU is lower at higher rates; compare
  `llm` with `max`.
- The mock answers at once, so the latency columns show the agent's added cost per call in the worst case. Real
  provider calls take seconds, which hides it. A fixed-rate client counts latency from each request's scheduled
  start, so a stall is not hidden; its own wake-up delay is included on both sides.
- Client, mock and agent share the host's CPUs. The mock also uses OpenSSL, so its SSL calls pay the uprobe cost
  too (the kernel filter only keeps them from the agent): `max` loses more throughput than a client of a remote API.
- In `non-llm` each client keeps one connection per host, which the kernel filter drops as a whole after its first
  request. Non-LLM traffic that opens a new TLS connection per request is not measured.
- The clients are CPython (`http.client`, shared `libssl`). Other runtimes and HTTP/2 are not covered.

## Running it

Build the agent (see the README), then on the node to measure:
`sudo python3 tests/benchmark/overhead/overhead.py --agent ./honeybeepf-llm --out results.md`

It takes about 10 minutes with the defaults. Use an otherwise idle host: the agent counts every LLM call on it, and
on a VM a busy host shows as CPU steal in the report. Without `--agent` only the baseline runs, which checks the
harness without root. `--cases` runs a subset; `--help` lists the flags.

The comparisons below use the same mock, clients and load:

- `overhead.py --agent ./obi --obi` and `--agent ./agentsight --agentsight` measure the other eBPF agents the same way.
- `gateway.py --gateway litellm|bifrost|envoy-ai-gateway --binary <path>` sends the calls through a gateway (as root:
  a CA made for the run joins the system certificate store while it lasts, so each gateway verifies the mock).
- `sdk.py --python <env> --instrument otel|openllmetry|openlit` makes the calls with the official `openai` SDK, with
  and without an instrumentation library turned on in the same environment.

## Compared with other tools

Measured on the same VM as the v0.5.0 results on 2026-11-28, with the same load and five rounds per cell;
honeybeepf-llm was measured again with the others (0.055 s per 1,000 `llm` calls, within the noise of the 0.052 below). Each tool
runs as its docs say to run it, with the least it needs to count tokens; every setting is in the scripts. If one is
not how a tool should be run, please [open an issue](https://github.com/honeybee-studio/honeybeepf-llm/issues): it
will be measured again.

The tools count tokens in three ways, which cost in different places: an **agent** on the node (its own process), a
**gateway** every call passes through (its own processes, and a hop on every call), or a **library** in the app (CPU
in the app's own processes). The table puts the cost each way adds side by side; the sections below give the
settings and the full numbers.

| `llm` case, 200 calls/s | Way | Added CPU s / 1,000 calls | Added memory | App p99 ms, without → with | Tokens recorded (`llm` · `max`) |
|---|---|---|---|---|---|
| honeybeepf-llm v0.5.0 | agent | 0.055 | 18 MB | 2.32 → 2.68 | 100% · 100% |
| OBI v1.0.0 | agent | 0.376 | 84 MB | 2.24 → 3.04 | 99.3% · 99.98% |
| AgentSight v1.0.35 | agent | 1.171 | 198 MB | 2.12 → 2.62 | 100% · 0.02% |
| Envoy AI Gateway v1.2.1 | gateway | 1.711 | 284 MB | 2.90 → 6.44 | 100% · 100% |
| Bifrost v2.5.1 | gateway | 2.537 | 491 MB | 2.40 → 8.04 | 100% · 100% |
| LiteLLM 1.110.0 | gateway | 12.56 | 2,365 MB | 2.26 → 4,000 (fell behind) | not exported |
| OpenTelemetry openai-v2 2.4b0 | library | 0.246 | 10 MB per app process | 10.76 → 8.84 | 100% · 100% |
| OpenLIT 1.45.0 | library | 0.993 | 56 MB per app process | 11.99 → 10.60 | 100% · 100% |
| OpenLLMetry 0.63.0 | library | 1.445 | 28 MB per app process | 9.25 → 11.26 | 100% · 100% |

- Every row is what counting adds, and nothing the app spends anyway: for an agent or a gateway, its own processes
  (CPU, and PSS of all of them); for a library, the app's CPU and each app process's maximum RSS with the library on,
  minus the same with it off. With many app processes on a node, a library's memory adds up per process.
- The library rows on their own: the app's CPU with the `openai` SDK alone was 2.67, 2.80 and 2.68 s per 1,000 calls
  (openai-v2, OpenLIT, OpenLLMetry), and 2.91, 3.80 and 4.13 s with the library on; max RSS per app process went
  from 83, 101 and 83 MB to 93, 156 and 111 MB. Their p99 moves more between runs than the library adds.
- A gateway does more than count: keys, routing, retries, budgets and caching. A library also builds traces. The
  agents only record.

### Agents on the node

All three read TLS with eBPF uprobes on the shared `libssl`; the agent runs as a separate process.

| | honeybeepf-llm v0.5.0 | OBI v1.0.0 | AgentSight v1.0.35 |
|---|---|---|---|
| CPU s / 1k calls: `llm` · `non-llm` · `max` | 0.055 · 0.020 · 0.006 | 0.376 · 0.238 · 0.127 | 1.171 · 0.515 · 0.107 |
| PSS MB · BPF maps MB (`llm`) | 18.3 · 22.5 | 84.4 · 180.9 | 198.4 · 40.2 |
| `max` calls/s, without → with | 11,159 → 10,569 (−5.3%) | 11,061 → 7,111 (−35.7%) | 11,140 → 10,025 (−10.0%) |
| `max` p99 ms, without → with | 0.81 → 0.91 | 0.82 → 2.69 | 0.81 → 0.90 |
| Tokens recorded: `llm` · `non-llm` · `max` | 100% · 100% · 100% | 99.3% · 98.3% · 99.98% | 100% · 100% · 0.02% |
| Start, 1,000 idle processes | 0.13 s | 3.39 s | 0.32 s |

- **OBI** instruments the `python3` processes with its OpenAI GenAI capture, a 64 KB HTTP buffer (as in OBI's own
  GenAI tests) and only its `genai` metrics, kept for Prometheus and not scraped during a run; tokens come from its
  `gen_ai_client_token_usage` sums. It tells an OpenAI call over HTTP/1.1 by the response headers the mock sends, and
  instruments a process only once it has found it, so the clients of all three agents start 5 s after the agent.
  OBI keeps no count of the calls it could not read. It builds an HTTP span for every request it sees: the table is
  what counting LLM tokens with it costs, not a measure of OBI as a whole.
- **AgentSight** runs `record -c python3 --no-server`; tokens come from `report token`, and CPU and memory include
  the two loaders it starts (`sslsniff`, `process`). The mock runs under another name so AgentSight does not trace
  it, which spares it the mock's half of the work that the other two agents do. At `max` it recorded 0.02% of the
  tokens: its `capture_loss` counter showed 1.9 to 3.3 million events dropped per run (0 in the other cases), as each
  SSL call reserves a 256 KB slot in a 16 MB ring buffer, so 64 calls in flight fill it. Its lower CPU at `max`
  reflects the calls it dropped.
- honeybeepf-llm keeps every call it cannot read in `llm_responses_unparsed_total` with a reason; none were lost.

### Gateways

`gateway.py` runs the `llm` and `max` cases straight to the mock and through the gateway, which the clients call
over plain HTTP on the node, as an in-cluster app would. Each gateway verifies the mock's certificate and starts
after 5 s of warm-up calls from 8 threads, so its workers have loaded their code.

| | LiteLLM 1.110.0 | Bifrost v2.5.1 | Envoy AI Gateway v1.2.1 |
|---|---|---|---|
| Settings | its production guide for one VM: a worker per vCPU (4), production mode, a master key, error logs only, no database | its 4-vCPU profile: pool 15,000, buffer 20,000, concurrency 5,000, `GOGC=200`; no inference auth (it needs virtual keys) | `aigw run` with its generated OpenAI config (its docs present it for local use; production runs the Kubernetes controller) |
| CPU s / 1k calls: `llm` · `max` | 12.56 · 11.12 | 2.54 · 0.97 | 1.71 · 0.68 |
| PSS MB (processes) | 2,365 (6) | 491 (1) | 284 (2) |
| `llm` calls/s and p99 ms through it (target 200/s) | 186 · 4,000 | 200 · 8.04 | 200 · 6.44 |
| `max` calls/s through it (11,000 straight) | 252 | 1,866 | 3,084 |
| Tokens counted by its metrics | not exported | 100% | 100% |
| Start to its health check | 5.8 s | 0.4 s | 2.0 s |

- `max` through a gateway is bound by its CPU and by the clients, each with one call in flight. The clients, the
  mock and the gateway share 4 vCPU.
- LiteLLM needs about 2.5 cores for 200 calls/s here, which leaves it at the edge of this VM: in this run it fell
  behind (186 calls/s, p99 4 s); in an earlier run on the same VM, with certificate verification off, it kept up
  (200 calls/s, p99 22 ms). Bifrost's profile preallocates 15,000 objects (`initial_pool_size`), which its docs
  describe as more memory for more speed.
- A real provider call takes seconds, which hides the milliseconds a gateway adds.

### Libraries in the app

`sdk.py` makes the same calls with the official `openai` SDK (sync client, JSON and `stream=True` with usage) and
compares the app's CPU, memory and latency with the library off and on in the same Python 3.14 environment. Each
library runs with its defaults, its data kept in memory (an OpenTelemetry in-memory span exporter and metric reader),
so nothing is exported; tokens come from the `gen_ai.usage.*` attributes on its spans.

| | OpenTelemetry openai-v2 2.4b0 | OpenLLMetry 0.63.0 | OpenLIT 1.45.0 |
|---|---|---|---|
| Records prompts and completions by default | no | yes | yes |
| App CPU s / 1k calls, off → on (`llm`) | 2.67 → 2.91 (+0.25) | 2.68 → 4.13 (+1.45) | 2.80 → 3.80 (+0.99) |
| Max RSS per app process MB, off → on | 83 → 93 | 83 → 111 | 101 → 156 |
| `max` calls/s, off → on | 1,832 → 1,645 (−10.2%) | 1,839 → 1,274 (−30.7%) | 1,874 → 1,424 (−24.0%) |
| Tokens recorded: `llm` · `max` | 100% · 100% | 100% · 100% | 100% · 100% |

- openai-v2 is installed with `opentelemetry-util-genai==0.4b0`: later releases of that package rename the module
  2.4b0 imports. By its code, it records the tokens of a streamed call on its span only, not in its token metric. OpenLLMetry's tests cover Python up to 3.13; it ran on 3.14 here. OpenLIT
  1.45.0 requires `openai` below 3, so its environment has `openai` 2.54.0 (3.51.0 in the others), and it also
  instruments `httpx` by default; it is given a local copy of its price file, which it downloads at start otherwise.

### Not compared

| Tool | Why |
|---|---|
| Grafana Beyla | OBI started as Beyla, and Beyla is now built on OBI: the same engine |
| Coroot node agent | its eBPF LLM support counts requests and latency, not tokens |
| Pixie | no LLM token parsing, and it needs Kubernetes |
| Aurva AIOStack | the repository holds Helm charts only; the agent is a prebuilt image that needs an account and sends its data to the vendor's service, where the token counts are shown |
| Portkey Gateway | its released open-source version (1.15.2) counts no tokens; analytics are in its hosted product |
| Helicone AI Gateway | it exports no token metric, and its repository has had no code change since July 2025 |
| TensorZero | archived and no longer maintained |
| Langfuse | it records nothing without a Langfuse server and keys |
| groundcover, New Relic, Sysdig AI Defense, Netskope | commercial products that need an account; some licenses limit publishing benchmarks |

## Results

The newest release comes first; earlier runs are folded below it.

### v0.5.0 (cf4b737), measured 2026-11-28

- Kernel 7.0.0-44-generic (Ubuntu 26.04.1 LTS), multipass VM: 4 vCPU of an Intel Core Ultra 5 225H, 7.7 GiB memory;
  CPU steal 0.0%; Python 3.14.4, OpenSSL 3.5.5; release build of tag v0.5.0, defaults as above except 5 rounds and
  `--client-delay 5` (as in the comparison with OBI)

| Case | req/s off | req/s on | p50 ms off | p50 ms on | p99 ms off | p99 ms on |
|---|---|---|---|---|---|---|
| `llm` | 200.12 | 200.13 | 0.77 | 0.85 | 1.70 | 2.55 |
| `non-llm` | 200.12 | 200.12 | 0.76 | 0.68 | 2.04 | 2.05 |
| `max` | 11,440 | 10,703 (−6.4%) | 0.32 | 0.35 | 0.78 | 0.89 |

| Case | CPU s / 1k req | RSS anon MB | RSS file MB | PSS MB | BPF maps MB | Tokens recorded |
|---|---|---|---|---|---|---|
| `llm` | 0.052 | 4.3 | 25.0 | 18.9 | 22.5 | 100% |
| `non-llm` | 0.022 | 4.3 | 25.3 | 19.1 | 22.5 | 100% |
| `max` | 0.006 | 4.3 | 25.2 | 19.0 | 22.5 | 100% |

Startup with 1,000 idle processes took 0.14 s. Against v0.3.2, CPU and the `max` throughput loss are the same within
the noise (0.052 vs 0.055 s per 1,000 `llm` calls, −6.4% vs −6.6%); the BPF maps grew from 18.8 to 22.5 MB with the
maps added since for Go `crypto/tls` reads, namespace scope and the AEAD probes. Every token was recorded in all runs.

### Earlier releases

<details markdown>
<summary>v0.3.2 (65d194e), measured 2026-11-17</summary>

- Kernel 7.0.0-43-generic (Ubuntu 26.04.1 LTS), multipass VM: 4 vCPU of an Intel Core Ultra 5 225H, 7.7 GiB memory;
  CPU steal 0.0%; Python 3.14.4, OpenSSL 3.5.5; release build of tag v0.3.2, defaults as above except 5 rounds

| Case | req/s off | req/s on | p50 ms off | p50 ms on | p99 ms off | p99 ms on |
|---|---|---|---|---|---|---|
| `llm` | 200.12 | 200.13 | 0.89 | 0.81 | 2.12 | 2.24 |
| `non-llm` | 200.12 | 200.12 | 0.78 | 0.81 | 1.80 | 2.18 |
| `max` | 11,601 | 10,840 (−6.6%) | 0.32 | 0.34 | 0.78 | 0.90 |

| Case | CPU s / 1k req | RSS anon MB | RSS file MB | PSS MB | BPF maps MB | Tokens recorded |
|---|---|---|---|---|---|---|
| `llm` | 0.055 | 4.4 | 27.3 | 21.4 | 18.8 | 100% |
| `non-llm` | 0.020 | 4.4 | 27.3 | 21.3 | 18.8 | 100% |
| `max` | 0.006 | 4.4 | 27.3 | 21.3 | 18.8 | 100% |

Startup with 1,000 idle processes took 0.14 s. v0.3.1 and v0.3.2 change no code on the measured path (release
signing, more OpenAI-compatible hosts, chart defaults), and the numbers match v0.3.0 within the noise: 0.055 vs 0.053
s per 1,000 `llm` calls, the same memory, and −6.6% vs −6.7% at `max`. Every token was recorded in all five runs.

</details>

<details markdown>
<summary>v0.3.0 (9018cd7), measured 2026-11-14</summary>

- Kernel 7.0.0-43-generic (Ubuntu 26.04.1 LTS), multipass VM: 4 vCPU of an Intel Core Ultra 5 225H, 7.7 GiB memory;
  CPU steal 0.0%; Python 3.14.4, OpenSSL 3.5.5; release build of `main` at 9018cd7 with the version set to 0.3.0,
  defaults as above except 5 rounds

| Case | req/s off | req/s on | p50 ms off | p50 ms on | p99 ms off | p99 ms on |
|---|---|---|---|---|---|---|
| `llm` | 200.12 | 200.12 | 1.00 | 0.86 | 1.91 | 2.37 |
| `non-llm` | 200.13 | 200.13 | 0.73 | 0.81 | 1.84 | 2.06 |
| `max` | 11,707 | 10,918 (−6.7%) | 0.31 | 0.34 | 0.77 | 0.90 |

| Case | CPU s / 1k req | RSS anon MB | RSS file MB | PSS MB | BPF maps MB | Tokens recorded |
|---|---|---|---|---|---|---|
| `llm` | 0.053 | 4.4 | 27.2 | 21.2 | 18.8 | 100% |
| `non-llm` | 0.020 | 4.4 | 27.3 | 21.3 | 18.8 | 100% |
| `max` | 0.006 | 4.4 | 27.2 | 21.2 | 18.8 | 100% |

Startup with 1,000 idle processes took 0.16 s. Against v0.2.0, CPU per call is lower (0.053 vs 0.067 s per 1,000
`llm` calls), memory is 0.2 MB higher, and the `max` throughput cost is about the same (−6.7% vs −6.5%). Every token
was recorded in all five runs.

</details>

<details markdown>
<summary>v0.2.0 (f8970f3), measured 2026-10-30</summary>

- Kernel 7.0.0-38-generic (Ubuntu 26.04.1 LTS), multipass VM: 4 vCPU of an Intel Core Ultra 5 225H, 7.7 GiB memory;
  CPU steal 0.0%; Python 3.14.4, OpenSSL 3.5.5; release build of tag v0.2.0, defaults as above except 5 rounds

| Case | req/s off | req/s on | p50 ms off | p50 ms on | p99 ms off | p99 ms on |
|---|---|---|---|---|---|---|
| `llm` | 200.12 | 200.13 | 0.81 | 0.89 | 1.90 | 2.47 |
| `non-llm` | 200.13 | 200.12 | 0.80 | 0.77 | 1.92 | 2.06 |
| `max` | 11,612 | 10,861 (−6.5%) | 0.32 | 0.34 | 0.76 | 0.99 |

| Case | CPU s / 1k req | RSS anon MB | RSS file MB | PSS MB | BPF maps MB | Tokens recorded |
|---|---|---|---|---|---|---|
| `llm` | 0.067 | 4.2 | 26.8 | 20.6 | 18.8 | 100% |
| `non-llm` | 0.048 | 4.2 | 26.7 | 20.6 | 18.8 | 100% |
| `max` | 0.008 | 4.2 | 26.7 | 20.5 | 18.8 | 100% |

Startup with 1,000 idle processes took 0.12 s. At 200 requests/s the latency differences are a few tenths of a
millisecond and go both ways (the run with the agent is sometimes faster), so they are noise at this resolution. In
`max` (about 10,900 LLM calls/s) every token was recorded in all five runs.

</details>

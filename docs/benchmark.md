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
completion tokens and 13 prompt tokens, with the response headers api.openai.com sends (`openai-version`,
`openai-organization`, `openai-processing-ms`, `x-request-id`). The seed, rate, duration, rounds, process count and
a delay before the clients start (`--client-delay`, 0 by default) are flags; the defaults are the numbers above.

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

The comparisons below use the same script: `--agent ./obi --obi` measures OBI the same way, and
`tests/benchmark/overhead/gateway.py --litellm <litellm command>` sends the same calls through LiteLLM.

## Compared with other tools

Measured on the same VM as the v0.5.0 results, with the same load, five rounds per cell. Each tool runs with the
least it needs to count tokens; every setting is in the scripts. If a setting here is not how a tool should be run,
please [open an issue](https://github.com/honeybee-studio/honeybeepf-llm/issues): the numbers will be measured again.

### OBI v1.0.0

[OpenTelemetry eBPF Instrumentation](https://github.com/open-telemetry/opentelemetry-ebpf-instrumentation) also reads
LLM token counts from TLS with eBPF. It instruments the `python3` processes only, with its OpenAI GenAI capture on,
a 64 KB HTTP buffer (as in OBI's own GenAI tests) and only its `genai` metrics, kept for Prometheus and not scraped
during a run; token accuracy comes from its `gen_ai_client_token_usage` sums at the end of each run. OBI tells an
OpenAI call over HTTP/1.1 by its response headers, which the mock sends. OBI instruments a process only once it has
found it, so the clients of both tools start 5 s after the tool (`--client-delay 5`); without it, OBI missed the first
calls of each run.

| Case | | honeybeepf-llm v0.5.0 | OBI v1.0.0 |
|---|---|---|---|
| `llm` | CPU s / 1k req | 0.052 | 0.383 |
| `non-llm` | CPU s / 1k req | 0.022 | 0.212 |
| `max` | CPU s / 1k req | 0.006 | 0.124 |
| `llm` | PSS MB · BPF maps MB | 18.9 · 22.5 | 86.2 · 180.9 |
| `max` | req/s off → on | 11,440 → 10,703 (−6.4%) | 11,426 → 7,296 (−36.1%) |
| `max` | p99 ms off → on | 0.78 → 0.89 | 0.78 → 2.64 |
| `llm` | p99 ms off → on | 1.70 → 2.55 | 1.98 → 2.73 |
| all | tokens recorded (`llm` · `non-llm` · `max`) | 100% · 100% · 100% | 99.2% · 97.8% · 99.99% |
| | start, 1,000 idle processes | 0.14 s | 3.41 s |

- OBI is a general instrumentation tool: it builds HTTP spans for every request it sees, and the GenAI capture is
  one part of it. The table is what it costs to count LLM tokens with it, not a measure of OBI as a whole. With its
  `http` metrics on as well, its CPU was the same (0.365 s per 1,000 `llm` calls).
- OBI keeps no count of the calls it could not read, so its missed tokens have no cause listed.
- honeybeepf-llm was measured before and after OBI to check for drift: 0.052 and 0.060 s per 1,000 `llm` calls, the
  same memory, −6.4% and −6.0% at `max`; the `llm` p99 (2.55 and 2.20 ms on) moves as much between the runs.

### A gateway: LiteLLM 1.110.0

A gateway counts calls the apps send to it instead of the provider, so it is another way to get the same numbers
rather than the same kind of tool. `gateway.py` runs the `llm` and `max` cases straight to the mock and through
LiteLLM over plain HTTP, as an in-cluster app would call it. LiteLLM runs as its production guide says for one VM:
a worker per vCPU (4), `LITELLM_MODE=PRODUCTION`, a master key, error logs only, no database. The one change from
production is `ssl_verify: false`, as the mock's certificate is self-signed. Each run starts after 5 s of warm-up
calls, so every worker has loaded its code; LiteLLM kept its connections to the mock open across calls.

| Case | | straight to the mock | through LiteLLM |
|---|---|---|---|
| `llm` | req/s | 200.13 | 200.01 |
| `llm` | p50 · p99 ms | 0.97 · 1.99 | 13.83 · 22.43 |
| `max` | req/s | 11,346 | 257 |
| `llm` | LiteLLM CPU s / 1k req | | 12.38 (2.5 cores at 200 req/s) |
| `llm` | LiteLLM RSS anon · PSS MB (server and 4 workers) | | 2,326 · 2,368 |
| | start to `/health/liveliness` | | 5.8 s |

- A gateway does more than count: keys, routing, retries, budgets and caching, which an agent on the node does not.
- The clients, the mock and LiteLLM share 4 vCPU, so `max` through LiteLLM is bound by its CPU on this VM; more CPU
  or more replicas raise it. A real provider call takes seconds, which the added 13 ms hides.

### Not compared

| Tool | Why |
|---|---|
| Grafana Beyla | OBI started as Beyla, and Beyla is now built on OBI: the same engine |
| Aurva AIOStack | the repository holds Helm charts only; the agent is a prebuilt image that needs an account and sends its data to the vendor's service, where the token counts are shown |
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

# Overhead benchmark

What the agent costs on a node, measured by a script anyone can rerun:
[`tests/benchmark/overhead/overhead.py`](../tests/benchmark/overhead/overhead.py).
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
completion tokens and 13 prompt tokens. The seed, rate, duration, rounds and process count are flags; the defaults
are the numbers above.

The agent runs as the Helm chart runs it: `BUILTIN_PROBES__LLM=true`, `RUST_LOG=info`. No OTLP endpoint is set, so
export cost is not included.

Things this does not show:

- Agent CPU includes its fixed cost while idle (the ring buffer poll loop and timers), so per request it is lower
  at higher rates; compare `llm` with `max`.
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

## Results

### 2026-10-27, agent at main 9e17302

- Kernel 7.0.0-34-generic (Ubuntu 26.04.1 LTS), multipass VM: 4 vCPU of an Intel Core Ultra 5 225H, 7.7 GiB memory;
  CPU steal 0.0%; Python 3.14.4, OpenSSL 3.5.5; release build, defaults as above

| Case | req/s off | req/s on | p50 ms off | p50 ms on | p99 ms off | p99 ms on |
|---|---|---|---|---|---|---|
| `llm` | 200.13 | 200.13 | 0.74 | 0.76 | 2.12 | 2.75 |
| `non-llm` | 200.12 | 200.13 | 0.90 | 0.62 | 2.34 | 1.98 |
| `max` | 11,261 | 10,578 (−6.1%) | 0.33 | 0.35 | 0.78 | 1.02 |

| Case | CPU s / 1k req | RSS anon MB | RSS file MB | PSS MB | BPF maps MB | Tokens recorded |
|---|---|---|---|---|---|---|
| `llm` | 0.073 | 29.4 | 26.1 | 45.1 | 17.9 | 100% |
| `non-llm` | 0.050 | 29.4 | 26.0 | 45.0 | 17.9 | 100% |
| `max` | 0.008 | 42.7 | 25.9 | 58.2 | 17.9 | 93.50% |

Startup with 1,000 idle processes took 0.20 s. At 200 requests/s the latency differences are a few tenths of a millisecond and go both ways (the run with the
agent is sometimes faster), so they are noise at this resolution. In `max` (about 10,600 LLM calls/s) the agent missed 6.5% of the tokens (47–96% recorded over ten other runs).
No event was lost to a full ring buffer (`reserve_fail=0`), but a few whole SSL calls per run are; when one is a
response head, the rest of that connection goes unrecorded until a buffer limit or close. A fix is tracked apart.

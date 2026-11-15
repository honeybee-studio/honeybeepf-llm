# honeybeepf-llm

Counts the tokens and USD cost of LLM API calls per Kubernetes pod, namespace and owner workload, with no gateway in
the request path and no SDK in the app. The agent runs as a DaemonSet, reads each call's plaintext from OpenSSL with
eBPF uprobes, and exports metrics over OpenTelemetry.

- **No app change.** Pods keep calling the provider directly; nothing to import, wrap or redeploy.
- **Providers built in:** OpenAI, Anthropic, Gemini, Amazon Bedrock, Azure OpenAI, Google Vertex AI and Meta, plus the
  OpenAI-compatible APIs of Groq, DeepSeek, Together, Fireworks, OpenRouter, Mistral and Cerebras. JSON and streaming,
  HTTP/1.1 and HTTP/2, cache and reasoning tokens.
- **Priced** from a dated built-in table, with your own prices and discounts on top.
- **Measured overhead:** about 53 µs of agent CPU per LLM call and 32 MB RSS plus 19 MB of BPF maps
  ([benchmark](https://github.com/honeybee-studio/honeybeepf-llm/blob/main/docs/benchmark.md)).

## Requirements

- Linux 5.10+ or RHEL 8.10 / 9.6 / 9.7 nodes with BTF. x86-64 is tested on every kernel in CI; arm64 images are
  published and tested on fewer kernels.
- Applications that use OpenSSL: Python, Node.js, curl and anything else linked to `libssl`. Go `crypto/tls`, rustls
  and Java are not covered yet.
- An OpenTelemetry Collector (OTLP gRPC) to receive the metrics, or none to only read the agent log.

The agent is not privileged. It adds the capabilities `BPF`, `PERFMON`, `SYS_ADMIN`, `SYS_PTRACE`, `NET_ADMIN` and
`SYS_RESOURCE`, runs with `hostPID`, and mounts the host's cgroup tree, tracefs and debugfs read-only. The
[security review](https://github.com/honeybee-studio/honeybeepf-llm/blob/main/docs/security-review.md) explains each
one and what the agent can and cannot see.

## Install

```bash
helm install honeybeepf-llm oci://ghcr.io/honeybee-studio/charts/honeybeepf-llm \
  -n honeybeepf --create-namespace \
  --set output.otlp.endpoint=my-collector.monitoring:4317
kubectl logs -n honeybeepf ds/honeybeepf-llm -f | grep 'LLM SUCCESS'
```

Without `output.otlp.endpoint` the agent records calls in its log only. The
[install guide](https://github.com/honeybee-studio/honeybeepf-llm/blob/main/docs/install.md) also sets up a Prometheus,
a Collector and the Grafana cost dashboard, and covers uninstall.

## Main values

| Value | Default | What it does |
|---|---|---|
| `output.otlp.endpoint` | `""` | Collector to send metrics to: `host:port`, `http://` or `https://` |
| `output.otlp.caSecret`, `output.otlp.headersSecret` | empty | CA and auth headers for a collector outside the cluster |
| `builtinProbes.llm.enabled` | `true` | Record LLM calls; `false` loads no eBPF program |
| `builtinProbes.llm.podLabels` | `[]` | Pod labels copied onto the metrics, e.g. `team` or `cost-center` |
| `builtinProbes.llm.prices` | `{}` | Your prices or discounts, reloaded without a restart |
| `builtinProbes.llm.providers` | `[]` | More providers by host, path and usage fields, incl. in-cluster plain HTTP |
| `builtinProbes.llm.detectByShape` | `false` | Experimental: record OpenAI-shaped calls to hosts no provider lists |
| `resources` | 50m / 128Mi requested | Sized from the benchmark |

Every value is commented in
[`values.yaml`](https://github.com/honeybee-studio/honeybeepf-llm/blob/main/charts/honeybeepf-llm/values.yaml).

## Metrics

`llm_requests_total`, `llm_tokens_total`, `llm_cost_usd_total` and `llm_latency_seconds` (in Prometheus with the
`honeybeepf_llm_` prefix), labelled by model and by the calling pod's namespace, name and owner workload.
`llm_responses_unparsed_total` counts calls the agent saw but could not record, with a reason, and
`llm_tls_coverage_pods` counts pods it cannot see. The full list is in [docs/metrics.md](https://github.com/honeybee-studio/honeybeepf-llm/blob/main/docs/metrics.md).

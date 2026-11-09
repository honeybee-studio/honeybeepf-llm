# Metrics

| Metric (as stored by the bundled Prometheus) | Type | Labels |
|---|---|---|
| `honeybeepf_llm_llm_requests_total` | counter | `model`, `status`, `target_namespace`, `target_pod_name`, `target_workload_kind`, `target_workload_name` |
| `honeybeepf_llm_llm_tokens_total` | counter | same, plus `token_type` (`prompt` / `completion`) |
| `honeybeepf_llm_llm_cache_read_tokens_total` | counter | same as requests (subset of prompt tokens) |
| `honeybeepf_llm_llm_cache_write_tokens_total` | counter | same as requests (subset of prompt tokens) |
| `honeybeepf_llm_llm_reasoning_tokens_total` | counter | same as requests (subset of completion tokens) |
| `honeybeepf_llm_llm_latency_seconds` | histogram | same as requests |
| `honeybeepf_llm_llm_responses_unparsed_total` | counter | `reason`, `target_namespace`, `target_pod_name`, `target_workload_kind`, `target_workload_name` |

`llm_requests_total`, `llm_tokens_total` and `llm_latency_seconds` also carry `transport` (`tls` or `plain`),
which tells an OpenSSL-captured call apart from one captured on the socket-level plaintext path; the cache and
reasoning metrics do not carry it.

`llm_responses_unparsed_total` counts LLM calls the agent saw but could not record, so a drop in recorded calls
can be told apart from a drop in traffic. `reason` is one of:

| `reason` | The call was given up because |
|---|---|
| `capture_lost` | part of a response read was lost in capture (ring buffer full after its first 4 KB, or one read over 16 KB) |
| `next_request` | a complete HTTP/1.1 response had no usage when the next request started |
| `connection_closed` | the connection closed before usage was found |
| `request_too_large` | the captured request bytes (the first 4 KB of each write) passed the 8 MB buffer limit |
| `response_too_large` | the response passed the 16 MB buffer limit and is neither an uncompressed HTTP/1.1 JSON body with usage in its first or last 64 KB nor an uncompressed HTTP/1.1 SSE stream (compressed and HTTP/2 responses always count here) |
| `background_unfinished` | an OpenAI Responses call made with `"background": true` had no usage in any poll the node saw: not polled for an hour, more than 4096 such calls waiting, or ended without usage |
| `usage_absent` | a complete 2xx response (an HTTP/1.1 body framed by Content-Length, or a finished HTTP/2 stream) carried no usage, for example a `gemini-embedding-001` embedding, which returns none |
| `agent_session` | a Claude Managed Agents session call (`POST /v1/sessions`, `/v1/sessions/{id}/events`): seen, but its usage is a running session total, so it is counted here on purpose and not read as one call's tokens — not a dropped call |

Not counted here: a read lost whole because the ring buffer was full (see `llm_events_total{stage="reserve_fail"}`),
and a connection left idle for 300 s without being closed (for example a killed process).

Unparsed calls per minute by reason, for an alert on `> 0` (excluding `agent_session`, which is counted on
purpose, not a drop, and `usage_absent`, which some providers cause on every call):

```promql
sum by (target_namespace, reason) (rate(honeybeepf_llm_llm_responses_unparsed_total{reason!~"agent_session|usage_absent"}[5m])) * 60
```

Calls whose response had no usage, per minute, for a panel rather than an alert:

```promql
sum by (target_namespace) (rate(honeybeepf_llm_llm_responses_unparsed_total{reason="usage_absent"}[5m])) * 60
```

The collector and Prometheus add more labels, such as `k8s_pod_name`, `k8s_node_name` and `namespace`; those name
the agent's pod and node and the collector's namespace. `target_namespace` and `target_pod_name` name the pod
that made the call. `target_workload_kind` and `target_workload_name` name its owner: a Deployment or an Argo
Rollout (through its ReplicaSet), a CronJob (through its Job, which needs the chart's `jobs get` permission), a
StatefulSet or DaemonSet, or `Pod` and the pod name for a pod without an owner; `unattributed` if the owner cannot
be named.

To compare variants of one workload (an A/B test, two versions), list pod labels in `builtinProbes.llm.podLabels`.
Each becomes `target_pod_label_<name>` on every LLM metric, with characters other than letters, digits and `_`
replaced by `_` (`app.kubernetes.io/version` → `target_pod_label_app_kubernetes_io_version`). A pod without the
label gets none. Each label multiplies the series by its distinct values, so the list is empty by default.

The Quickstart's Prometheus is the `honeybeepf-llm-prometheus-server` service (port 80) in the `honeybeepf`
namespace. Tokens per minute by namespace:

```promql
sum by (target_namespace, token_type) (rate(honeybeepf_llm_llm_tokens_total[5m])) * 60
```

On the demo cluster above:

```
{target_namespace="team-a", token_type="prompt"}       371
{target_namespace="team-a", token_type="completion"}   121
{target_namespace="team-b", token_type="prompt"}      1021
{target_namespace="team-b", token_type="completion"}   123
{target_namespace="team-c", token_type="prompt"}        50
{target_namespace="team-c", token_type="completion"}    21
```

## Grafana

Two dashboards ship in [`docs/grafana/`](grafana/): **LLM Cost Observability**
([`honeybeepf-cost.json`](grafana/honeybeepf-cost.json)) and **Security & Compliance**
([`honeybeepf-security.json`](grafana/honeybeepf-security.json)); the security dashboard needs the file-access
probe, which is off by default (`builtinProbes.filesystem.file_access.enabled`). Import them in Grafana
(**Dashboards → New → Import**), or let the Grafana sidecar load them:

```bash
kubectl -n monitoring create configmap honeybeepf-dashboards \
  --from-file=docs/grafana/honeybeepf-cost.json \
  --from-file=docs/grafana/honeybeepf-security.json
kubectl -n monitoring label configmap honeybeepf-dashboards grafana_dashboard=1
```

The cost dashboard splits token usage, requests and latency by namespace, pod and workload, estimates spend per
workload and per model in USD from the built-in price table, and surfaces Cost-Waste Signals — the measured USD of
refused server-side fallback attempts and the estimated USD a prompt cache could save.

![LLM Cost Observability dashboard on the demo cluster: token usage, requests and latency by namespace, pod and workload; estimated spend per workload and per model in USD; and Cost-Waste Signals showing the measured cost of refused fallback attempts and the estimated savings from prompt caching, across team-a, team-b and team-c](images/grafana.png)

See [`docs/grafana/README.md`](grafana/README.md) for the template variables and other options.

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

A call matched by its request path and response shape alone (`detectByShape`, host not listed) also carries
`match="shape"` on every per-call metric, cost included; a call matched by its host has no `match` label, so
`{match!="shape"}` keeps only host-matched calls ([why](guides/detect-by-shape.md)).

`model` is the model the response names. When it names none, as with the Gemini Decisions API and Bedrock Converse,
it is the top-level `model` of the request body, or the model ID in a Bedrock `/model/{id}/...` path (an inference
profile ARN is left out); with neither, `unknown`.

`llm_responses_unparsed_total` counts LLM calls the agent saw but could not record, so a drop in recorded calls
can be told apart from a drop in traffic. `reason` is one of:

| `reason` | The call was given up because |
|---|---|
| `capture_lost` | part of a response read was lost in capture (ring buffer full after its first 4 KB, or one read over 16 KB) |
| `next_request` | a complete HTTP/1.1 response had no usage when the next request started |
| `connection_closed` | the connection closed before usage was found |
| `request_too_large` | the captured request bytes (the first 4 KB of each write) passed the 8 MB buffer limit |
| `response_too_large` | the response passed the 16 MB buffer limit and is neither an uncompressed JSON body with usage in its first or last 64 KB (for a JSON array, in its last element) nor an uncompressed SSE stream (compressed responses always count here) |
| `background_unfinished` | an OpenAI Responses call made with `"background": true` had no usage in any poll the node saw: not polled for an hour, more than 4096 such calls waiting, or ended without usage |
| `usage_absent` | a complete 2xx response (an HTTP/1.1 body framed by Content-Length, or a finished HTTP/2 stream) carried no usage, for example a `gemini-embedding-001` embedding, which returns none |
| `stream_error` | a response stream ended on a provider `event: error` frame (Gemini Interactions, Anthropic Messages or OpenAI Responses, on an otherwise 200 response) instead of its final usage, so no exact token count reached the wire |
| `agent_session` | a Claude Managed Agents session call (`POST /v1/sessions`, `/v1/sessions/{id}/events`): seen, but its usage is a running session total, so it is counted here on purpose and not read as one call's tokens — not a dropped call |

Not counted here: a read lost whole because the ring buffer was full (see `llm_events_total{stage="reserve_fail"}`),
and a connection left idle for 300 s without being closed (for example a killed process).

A plain-HTTP receive larger than the 16 KB capture window (one skb over 16 KB on loopback or from GRO) is counted
under `llm_events_total{stage="plain_recv_truncated"}`, whether or not the call is still recorded; its usage is read
only when it falls in the first 16 KB.

The chart's alert rules use these counters; see [alerts.md](alerts.md). Unparsed calls per minute by reason, for an
alert on `> 0` (excluding `agent_session`, which is counted on purpose, not a drop, and `usage_absent`, which some
providers cause on every call):

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
be named. To split a workload further, by version or team label, add [pod labels](guides/pod-labels.md).

The [bundled Prometheus](install.md) is the `honeybeepf-llm-prometheus-server` service (port 80) in the `honeybeepf`
namespace. Tokens per minute by namespace:

```promql
sum by (target_namespace, token_type) (rate(honeybeepf_llm_llm_tokens_total[5m])) * 60
```

On the [demo](demo.md) cluster:

```
{target_namespace="team-a", token_type="prompt"}       371
{target_namespace="team-a", token_type="completion"}   121
{target_namespace="team-b", token_type="prompt"}      1021
{target_namespace="team-b", token_type="completion"}   123
{target_namespace="team-c", token_type="prompt"}        50
{target_namespace="team-c", token_type="completion"}    21
```

## More

- [Cost and prices](cost.md): the cost metrics and how calls are priced.
- [Find the workloads that are not seen](guides/coverage.md): the TLS coverage metric.
- [Grafana dashboards](grafana/README.md): the cost and security dashboards.

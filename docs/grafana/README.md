# HoneybeePF Grafana Dashboards

Pre-built Grafana dashboards for visualizing metrics exported by the
HoneybeePF eBPF agent via OpenTelemetry → Prometheus.

## Dashboards

| File | Title | Panels | Focus |
|------|-------|:------:|-------|
| [`honeybeepf-cost.json`](./honeybeepf-cost.json) | HoneybeePF — LLM Cost Observability | 15 | LLM API traffic, token usage, latency, cost attribution by team / pod / model |
| [`honeybeepf-security.json`](./honeybeepf-security.json) | HoneybeePF — Security & Compliance | 11 | File access monitoring, LLM ↔ file-access correlation, per-process auditing |

### Cost Observability (`honeybeepf-cost.json`)

Two sections — **External LLM API** and **On-Premise LLM** — with:

- Requests / minute (by team, by model)
- Token usage / minute (prompt vs completion, by team, by model)
- Token share pie chart
- Latency p50 / p95 / p99
- 5-minute summary stats (total requests, total tokens, active probes)
- Per-pod token usage and p95 latency
- Top 10 pods by token usage

Template variables: `$datasource`, `$team`, `$pod`, `$model`.

### Security & Compliance (`honeybeepf-security.json`)

Three sections:

- **File Access Overview** — events / minute, by namespace, top 10 accessed files
- **Correlation & Hot Paths** — LLM ↔ file-access correlation, file access by path
- **Summary (Last 5 Minutes)** — total events, unique files accessed, active probes

Template variables: `$datasource`, `$process`.

## Required Metrics

These dashboards expect the following Prometheus metrics exported by the
HoneybeePF agent. The agent emits them via OTLP to an OpenTelemetry
Collector, which forwards them to Prometheus.

### LLM metrics

| Metric | Type | Labels |
|--------|------|--------|
| `honeybeepf_llm_requests_total` | Counter | `model`, `status`, `target_namespace`, `target_pod_name` |
| `honeybeepf_llm_tokens_total` | Counter | `model`, `status`, `token_type` (prompt / completion), `target_namespace`, `target_pod_name` |
| `honeybeepf_llm_latency_seconds_bucket` | Histogram | `model`, `status`, `target_namespace`, `target_pod_name` |

### Security / file-access metrics

| Metric | Type | Labels |
|--------|------|--------|
| `honeybeepf_file_access_events_total` | Counter | `filename`, `flags`, `process`, `cgroup_id`, `target_namespace`, `target_pod_name` |

### Agent metrics

| Metric | Type | Labels |
|--------|------|--------|
| `honeybeepf_active_probes` | Gauge | `probe` |

> **Note:** `target_namespace` and `target_pod_name` labels are only populated
> when the agent is built with the `k8s` feature flag (the Docker image uses
> this by default).

## Installation

### Option 1 — Import via Grafana UI (simplest)

1. Open Grafana → **Dashboards → New → Import**
2. Click **Upload JSON file** and select one of the files in this directory
3. When prompted, choose your Prometheus data source (the dashboards default
   to a data source named `Prometheus`)
4. Click **Import**

Repeat for both dashboards.

### Option 2 — ConfigMap + Grafana sidecar (Kubernetes)

If your Grafana is configured with the
[`grafana-sidecar-dashboards`](https://github.com/kiwigrid/k8s-sidecar)
pattern (as in the official Grafana Helm chart with
`sidecar.dashboards.enabled=true`), create a labeled ConfigMap and the
sidecar will pick up the dashboards automatically:

```bash
kubectl -n monitoring create configmap honeybeepf-dashboards \
  --from-file=docs/grafana/honeybeepf-cost.json \
  --from-file=docs/grafana/honeybeepf-security.json

kubectl -n monitoring label configmap honeybeepf-dashboards \
  grafana_dashboard=1
```

Within a minute the sidecar will discover the ConfigMap, and both dashboards
will appear in Grafana.

### Option 3 — Helm values (for the HoneybeePF chart)

See [`charts/honeybeepf-llm/values.yaml`](../../charts/honeybeepf-llm/values.yaml)
for the Helm-based deployment. Dashboards can be shipped alongside the chart
by rendering a ConfigMap from these JSON files.

## Data Source Configuration

Both dashboards use a `$datasource` template variable so they work with any
Prometheus-compatible backend:

- Prometheus
- Thanos
- Mimir
- VictoriaMetrics
- Cortex

The default data source name is `Prometheus`. If your data source has a
different name, select it from the **Datasource** dropdown at the top of
the dashboard after import.

## Updating the dashboards

If you modify a dashboard inside Grafana and want to save the changes back:

1. Open the dashboard
2. Click the **Share** icon → **Export** tab
3. Enable **Export for sharing externally**
4. Click **Save to file** and overwrite the corresponding JSON here
5. Run a quick sanity check:

```bash
jq -e . docs/grafana/honeybeepf-cost.json > /dev/null && echo ok
grep -c VictoriaMetrics docs/grafana/*.json   # should be 0 for both
```

> **Important:** Before committing an updated JSON, make sure every
> `"uid": "..."` under any `"datasource"` field is set to `"${datasource}"`
> — not a hardcoded data source name. Hardcoded UIDs make the dashboard
> unusable on anyone else's Grafana instance.

## License

These dashboards are distributed under the [Apache License 2.0](../../LICENSE),
the same license as the rest of the HoneybeePF project.

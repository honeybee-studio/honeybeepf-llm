# Grafana dashboards

Two dashboards ship with honeybeepf-llm. They read the agent's metrics from Prometheus or any compatible backend.

| Dashboard | Shows | Needs |
|---|---|---|
| [LLM Cost Observability](https://github.com/honeybee-studio/honeybeepf-llm/blob/main/docs/grafana/honeybeepf-cost.json) | LLM traffic, tokens, latency and cost by team, workload, pod and model | the agent's default metrics |
| [Security & Compliance](https://github.com/honeybee-studio/honeybeepf-llm/blob/main/docs/grafana/honeybeepf-security.json) | file access by process and pod, next to LLM calls | the file-access probe, which the chart does not turn on (`BUILTIN_PROBES__FILESYSTEM__FILE_ACCESS=true`) |

## LLM Cost Observability

| Section | Panels |
|---|---|
| External LLM API | requests, tokens (prompt and completion) and latency p50/p95/p99 by team and model; token share; 5-minute totals |
| On-Premise LLM | the same for [plain-HTTP model servers](../guides/plaintext.md) (`transport="plain"`), by namespace and workload |
| Cost-Waste Signals | the billed cost of refused fallback attempts; prompt cache hit rate; the most caching could save |
| Cost (estimate) | spend per workload and per model, and the total, in USD |
| LLM Usage Detail & Data Quality | tokens by `modality` and `operation`; tokens with no price; calls seen but not recorded |

Filters at the top: **Datasource**, **Team**, **Workload**, **Pod** and **Model**.

??? example "See the whole dashboard"
    ![LLM Cost Observability dashboard on the demo cluster: token usage, requests and latency by namespace, pod and workload; estimated spend per workload and per model in USD; and Cost-Waste Signals showing the measured cost of refused fallback attempts and the estimated savings from prompt caching, across team-a, team-b and team-c](../images/grafana.png)

    On the [demo](../demo.md) cluster: a mock provider and fake keys.

!!! info "Measured and estimated"
    - **Measured**: the refused fallback cost (`attempt="declined"`) is what the provider billed.
    - **Estimated**: spend is tokens times the [price table](../cost.md), not an invoice. A model with no price is
      missing from it and shows in the unpriced panel instead.
    - **Leads, not losses**: the cache panels assume every input token is a repeated prefix, so the savings figure is
      a ceiling, and a low hit rate means waste only where prompts really repeat.

## Security & Compliance

| Section | Panels |
|---|---|
| File Access Overview | events per minute, by namespace; the 10 files opened most |
| Correlation & Hot Paths | file access next to LLM calls; access by path |
| Summary (last 5 minutes) | events, distinct files, active probes |

Filters: **Datasource** and **Process**. It reads `honeybeepf_llm_file_access_events_total` (`filename`, `flags`,
`process`, `cgroup_id`, `target_namespace`, `target_pod_name`) and `honeybeepf_llm_active_probes` (`probe`).

## Install

=== "Grafana UI"

    1. In Grafana, open **Dashboards → New → Import**.
    2. **Upload JSON file**: [`honeybeepf-cost.json`](https://github.com/honeybee-studio/honeybeepf-llm/blob/main/docs/grafana/honeybeepf-cost.json)
       or [`honeybeepf-security.json`](https://github.com/honeybee-studio/honeybeepf-llm/blob/main/docs/grafana/honeybeepf-security.json).
    3. Pick your Prometheus data source, then **Import**.

=== "ConfigMap and sidecar"

    With the Grafana Helm chart's dashboard sidecar (`sidecar.dashboards.enabled=true`), a labelled ConfigMap is
    loaded within a minute:

    ```bash
    kubectl -n monitoring create configmap honeybeepf-dashboards \
      --from-file=docs/grafana/honeybeepf-cost.json \
      --from-file=docs/grafana/honeybeepf-security.json
    kubectl -n monitoring label configmap honeybeepf-dashboards grafana_dashboard=1
    ```

    The [demo](../demo.md) sets Grafana up this way.

## Data source

- Any Prometheus-compatible backend works: Prometheus, Thanos, Mimir, VictoriaMetrics or Cortex. Pick it in the
  **Datasource** filter; the default name is `Prometheus`.
- Rate panels use a 1-minute **Min step**, matching the bundled Prometheus's 1-minute scrape. If yours scrapes less
  often, raise the step to match, or the panels show **No data**; if more often, you can lower it.
- The namespace, pod and workload labels need the agent's `k8s` build, which the image uses.

The metrics and their labels are in [Metrics](../metrics.md) and [Cost and prices](../cost.md).

??? note "Changing a dashboard"
    1. Edit it in Grafana, then **Share → Export**, turn on **Export for sharing externally** and **Save to file**
       over the JSON in `docs/grafana/`.
    2. Check it:

        ```bash
        jq -e . docs/grafana/honeybeepf-cost.json > /dev/null && echo ok
        grep -c VictoriaMetrics docs/grafana/*.json   # 0 for both
        ```

    3. Every `"uid"` under a `"datasource"` must be `"${datasource}"`, not a fixed name, or the dashboard breaks on
       other Grafanas.

The dashboards are under the [Apache License 2.0](https://github.com/honeybee-studio/honeybeepf-llm/blob/main/LICENSE).

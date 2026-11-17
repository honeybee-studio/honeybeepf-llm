# Grafana dashboards

The LLM Cost Observability dashboard ships with honeybeepf-llm. It reads the agent's metrics from Prometheus or any
compatible backend.

[`honeybeepf-cost.json`](https://github.com/honeybee-studio/honeybeepf-llm/blob/main/docs/grafana/honeybeepf-cost.json)
shows LLM traffic, tokens, latency and cost by team, workload, pod and model, from the agent's default metrics.

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

## Install

=== "Grafana UI"

    1. In Grafana, open **Dashboards → New → Import**.
    2. **Upload JSON file**: [`honeybeepf-cost.json`](https://github.com/honeybee-studio/honeybeepf-llm/blob/main/docs/grafana/honeybeepf-cost.json).
    3. Pick your Prometheus data source, then **Import**.

=== "ConfigMap and sidecar"

    With the Grafana Helm chart's dashboard sidecar (`sidecar.dashboards.enabled=true`), a labelled ConfigMap is
    loaded within a minute:

    ```bash
    kubectl -n monitoring create configmap honeybeepf-dashboards \
      --from-file=docs/grafana/honeybeepf-cost.json
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
        grep -c VictoriaMetrics docs/grafana/honeybeepf-cost.json   # 0
        ```

    3. Every `"uid"` under a `"datasource"` must be `"${datasource}"`, not a fixed name, or the dashboard breaks on
       other Grafanas.

The dashboards are under the [Apache License 2.0](https://github.com/honeybee-studio/honeybeepf-llm/blob/main/LICENSE).

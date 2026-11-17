# Compare workload variants with pod labels

To compare variants of one workload (an A/B test, two versions), list pod labels in `builtinProbes.llm.podLabels`.
Each becomes `target_pod_label_<name>` on every LLM metric, with characters other than letters, digits and `_`
replaced by `_` (`app.kubernetes.io/version` → `target_pod_label_app_kubernetes_io_version`). A pod without the
label gets none. Each label multiplies the series by its distinct values, so the list is empty by default.

```yaml
builtinProbes:
  llm:
    podLabels: ["app.kubernetes.io/version", "team"]
```

Spend per version of one workload:

```promql
sum by (target_pod_label_app_kubernetes_io_version) (
  increase(honeybeepf_llm_llm_cost_usd_total{target_workload_name="support-bot"}[1d]))
```

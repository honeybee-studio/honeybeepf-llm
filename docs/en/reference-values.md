# Helm values

The values you are most likely to set. Every value is commented in the chart's
[`values.yaml`](https://github.com/honeybee-studio/honeybeepf-llm/blob/main/charts/honeybeepf-llm/values.yaml).

--8<-- "charts/honeybeepf-llm/README.md:values"

Set them with `--set` or a values file:

```bash
helm upgrade --install honeybeepf-llm oci://ghcr.io/honeybee-studio/charts/honeybeepf-llm -n honeybeepf -f my-values.yaml
```

The guides show the values in use: [providers](guides/custom-providers.md), [prices](guides/prices.md),
[plain-HTTP servers](guides/plaintext.md), [pod labels](guides/pod-labels.md) and [budgets and alerts](alerts.md).

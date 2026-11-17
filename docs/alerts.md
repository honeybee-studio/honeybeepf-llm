# Alerts

The chart ships Prometheus alert rules for two questions: is a team spending more than it should, and can the numbers
be trusted. They are in [`charts/honeybeepf-llm/templates/_alerts.tpl`](https://github.com/honeybee-studio/honeybeepf-llm/blob/main/charts/honeybeepf-llm/templates/_alerts.tpl)
and reach Prometheus in one of three ways:

- **Prometheus Operator** (kube-prometheus-stack and similar): set `alerts.prometheusRule.enabled=true` and, if the
  operator selects rules by label, `alerts.prometheusRule.labels` (for example `release: kube-prometheus-stack`).
- **The bundled Prometheus** from [install.md](install.md) loads them by default. Its Alertmanager is off, so firing
  alerts show on the Prometheus `/alerts` page and in Grafana; route them with your own Alertmanager.
- **Any other Prometheus**: render them and add the file to `rule_files`:

  ```bash
  helm template x charts/honeybeepf-llm --set alerts.prometheusRule.enabled=true \
    --show-only templates/prometheusrule.yaml | yq '{"groups": .spec.groups}' > honeybeepf-llm-rules.yml
  ```

Every spend and call rule groups by `target_namespace`, the namespace of the pod that made the call. The `namespace`
label on these series is the collector's.

## Spend

| Alert | Fires when | For | Severity |
|---|---|---|---|
| `LLMSpendOverBudget` | a namespace's spend over the last 24 hours is above its daily budget | 5m | warning |
| `LLMSpendPaceOverBudget` | at the last hour's rate, a namespace would spend more than its daily budget in a day | 15m | warning |
| `LLMSpendSpike` | the last hour's spend is over `alerts.spike.factor` (3) times the same hour yesterday and over `alerts.spike.minUSD` ($5) | 15m | info |

Budgets are daily USD amounts per calling namespace:

```yaml
alerts:
  budgets:
    team-a: 50
    team-b: 200
  defaultDailyBudgetUSD: 20   # every other namespace that spends anything; 0 (the default) sets none
```

They become the series `honeybeepf_llm:namespace_daily_budget_usd{target_namespace="team-a"}`, which the two budget
alerts compare against, so a budget can also come from your own recording rule. With no budget the two alerts never
fire. A "day" is the last 24 hours, not a calendar day. `LLMSpendPaceOverBudget` is the early warning: it fires while
there is still budget left, hours before a provider's usage data would show the spend. `LLMSpendSpike` compares with
yesterday, so a normal weekday-to-weekend change can trigger it; it is `info` for that reason.

## Can the numbers be trusted

| Alert | Fires when | For | Severity |
|---|---|---|---|
| `LLMCallsWentSilent` | a namespace that made at least `alerts.silentMinCalls` (20) calls in this hour yesterday made none in the last hour | 30m | warning |
| `LLMCallsNotRecorded` | over `alerts.notRecordedRatio` (5%) of the calls seen from a namespace in 15 minutes, and at least 5, could not be recorded (`agent_session` and `usage_absent` are not counted) | 15m | warning |
| `LLMTokensUnpriced` | tokens were recorded without a price (a new model, or a price past its last day), so cost is under-reported | 30m | info |
| `LLMCaptureEventsDropped` | the ring buffer on a node was full and captured data was dropped | 10m | warning |
| `LLMWorkloadNotHooked` | a workload has pods that map a libssl the probes are not attached to | 30m | warning |

`LLMCallsWentSilent` is the one to keep: a silent drop to zero looks the same as a quiet day on a cost dashboard, and
the alert asks a person to check which it is. `llm_responses_unparsed_total` has the reasons behind
`LLMCallsNotRecorded`; [metrics.md](metrics.md) lists them.

## The agent

| Alert | Fires when | For | Severity |
|---|---|---|---|
| `HoneybeepfAgentMissing` | the agent on a node reported an hour ago and does not now (the collector drops a series 5 minutes after its last update) | 10m | critical |

`HoneybeepfAgentMissing` also fires for about an hour after a node is removed, for example by an autoscaler.

## Tests

`ci/test-alerts.sh` runs in CI. It checks the rules and their annotation templates with `promtool`, runs the unit
tests in [`tests/alerts/rules.test.yml`](https://github.com/honeybee-studio/honeybeepf-llm/blob/main/tests/alerts/rules.test.yml) (each rule has a case that must fire and one
that must not), and fails when the copy in the bundled Prometheus chart differs from the chart's defaults.

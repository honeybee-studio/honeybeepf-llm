# Cost and prices

Each recorded call is costed from a dated built-in price table, matched by the model name in the response (a dated
name such as `claude-sonnet-4-5@20250929` matches its model), with [your own prices](guides/prices.md) on top. Three
metrics carry the result:

| Metric (as stored by the bundled Prometheus) | Type | Labels |
|---|---|---|
| `honeybeepf_llm_llm_cost_usd_total` | counter (float USD) | the per-call labels (`model`, `status`, `attempt`, `target_namespace`, `target_pod_name`, `target_workload_kind`, `target_workload_name`), plus `price_table_version` and `cost_type` |
| `honeybeepf_llm_llm_cache_savings_ceiling_usd_total` | counter (float USD) | same per-call labels, plus `price_table_version` and `cache_price` |
| `honeybeepf_llm_llm_unpriced_tokens_total` | counter | same per-call labels, plus `price_table_version` and `reason` |

Unlike the request, token and latency metrics, the cost metrics do not carry `transport` or `operation`.

The USD counters carry no unit, so the exporter does not append one to the name. `attempt` is `served` for the
call the provider served and `declined` for an earlier server-side fallback attempt the provider billed but routed
away from (tokens and cost only, never a request count or latency). `price_table_version` is the table's version
string (the built-in `version`, with `+custom-<hash>` appended when custom prices are merged over it), so a price
change shows as a new series. Calls recorded as an error record no cost.

`llm_cost_usd_total` is the realized cost of a call, split by `cost_type` the way providers bill it. A part that
is zero is not added, so the series exist only for the categories a call actually used:

| `cost_type` | Billed for |
|---|---|
| `input` | prompt tokens that were neither read from nor written to the cache |
| `cache_read` | prompt tokens served from the provider cache |
| `cache_write` | prompt tokens written to the provider cache |
| `output` | all completion tokens (reasoning tokens are billed at the output price) |

`llm_cache_savings_ceiling_usd_total` is an upper bound on how much USD prompt caching could save: the call's
uncached input tokens rebilled from the model's input price down to its cache-read price. It is emitted only for
priced, non-embedding calls (embeddings have no prompt cache) and only when the bound is above zero. `cache_price`
is `known` when the model has a cache-read price, or `assumed_free` when it has none and the bound falls back to
the full input cost.

`llm_unpriced_tokens_total` counts the prompt-plus-completion tokens of a call the price table could not price, so
an unpriceable call is never silently recorded as `$0`. `reason` is one of:

| `reason` | The call could not be priced because |
|---|---|
| `unknown_model` | the model name was absent or not in the table |
| `missing_price` | a price the call needs is absent or past its last day |
| `unsupported_modifier` | the call's service tier, speed or geo modifier has no price |
| `inconsistent_usage` | the reported cache token counts exceed the prompt total they are part of |

Cost metrics appear only if a price table loaded; a rejected built-in table only warns, and the agent then runs
without them.

Estimated spend per minute by workload, in USD:

```promql
sum by (target_namespace, target_workload_name) (rate(honeybeepf_llm_llm_cost_usd_total[5m])) * 60
```

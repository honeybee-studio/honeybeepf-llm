# Cost and prices

Every recorded call gets an estimated cost in USD: its token counts times the model's prices.

## Where prices come from

- A dated **built-in price table**, matched by the model name in the response. A dated name such as
  `claude-sonnet-4-5@20250929` matches its model.
- **[Your own prices](guides/prices.md)** on top: models the table lacks, or a contract discount.
- A model with no price is **never recorded as $0**: its tokens go to `llm_unpriced_tokens_total` instead.

!!! note
    Cost metrics appear only when a price table loaded. A rejected built-in table only logs a warning, and the agent
    then runs without them.

## Spend per workload

```promql
sum by (target_namespace, target_workload_name) (rate(honeybeepf_llm_llm_cost_usd_total[5m])) * 60
```

USD per minute, by namespace and workload.

## The cost metrics

| Metric | What it counts | Extra labels |
|---|---|---|
| `honeybeepf_llm_llm_cost_usd_total` | the cost of each call, in USD | `cost_type`, `price_table_version` |
| `honeybeepf_llm_llm_cache_savings_ceiling_usd_total` | at most how much prompt caching could have saved | `cache_price`, `price_table_version` |
| `honeybeepf_llm_llm_unpriced_tokens_total` | tokens of calls that could not be priced | `reason`, `price_table_version` |

All three carry the per-call labels (`model`, `status`, `attempt`, `target_namespace`, `target_pod_name`,
`target_workload_kind`, `target_workload_name`), but not `transport` or `operation`. The USD counters carry no unit, so
the exporter does not append one to the name.

### `cost_type`: how the cost is split

Like a provider's bill. A part that is zero is not added, so a call only has the series it used.

| `cost_type` | Billed for |
|---|---|
| `input` | prompt tokens neither read from nor written to the cache |
| `cache_read` | prompt tokens served from the provider cache |
| `cache_write` | prompt tokens written to the provider cache |
| `output` | all completion tokens, reasoning tokens included |

### `attempt`: served or declined

| `attempt` | The call |
|---|---|
| `served` | the provider served it |
| `declined` | an earlier server-side fallback attempt the provider billed but routed away from: tokens and cost only, no request count or latency |

Calls recorded as an error record no cost.

### `price_table_version`

The built-in table's `version`, with `+custom-<hash>` appended when your prices are merged over it. A price change
shows as a new series.

### Cache savings ceiling

The call's uncached input tokens priced at the model's cache-read price instead of its input price: an upper bound on
what prompt caching could save. Emitted only for priced calls that are not embeddings, and only when above zero.

| `cache_price` | Meaning |
|---|---|
| `known` | the model has a cache-read price |
| `assumed_free` | it has none, so the bound is the full input cost |

### Unpriced tokens

| `reason` | The call could not be priced because |
|---|---|
| `unknown_model` | the model name was absent or not in the table |
| `missing_price` | a price the call needs is absent or past its last day |
| `unsupported_modifier` | the call's service tier, speed or geo modifier has no price |
| `inconsistent_usage` | the reported cache token counts exceed the prompt total they are part of |

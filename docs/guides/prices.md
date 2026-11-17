# Add your own prices

The built-in price table covers OpenAI, Anthropic and Gemini models. Add prices for other models, such as a
self-hosted one or an OpenAI-compatible provider, or a discount from your contract, in the chart values. Prices are in
USD per million tokens, in the format of the
[built-in table](https://github.com/honeybee-studio/honeybeepf-llm/blob/main/honeybeepf-llm/honeybeepf-llm-core/src/prices.json),
and are merged over it per model.

```yaml
builtinProbes:
  llm:
    prices:
      version: "acme-2026-10"
      providers:
        - name: "openai"
          source: "our contract"
          checked: "2026-10-01"
          price_multiplier: 0.8        # pay 80% of the built-in OpenAI prices
          models:
            gpt-5: {input: 1.0, output: 8.0, cache_read: 0.1}
        - name: "vllm"
          source: "internal"
          checked: "2026-10-01"
          models:
            my-vllm-llama: {input: 0.1, output: 0.2}
```

## How your table combines with the built-in one

| You write | Effect |
|---|---|
| a model the built-in table has | replaces its whole entry: list its tiers, speed and geo prices too, or they are gone |
| a model it does not have | adds it |
| `price_multiplier` on a provider | scales that provider's built-in prices (`0.8` = pay 80%), not the models you list; a provider with only a multiplier still needs `source`, `checked` and `models: {}` |

With the built-in table `2026-10-26`, the example above gives (USD per million tokens, input / output / cache read):

| Model | Built-in | After | Why |
|---|---|---|---|
| `gpt-4o` | 2.50 / 10.00 / 1.25 | 2.00 / 8.00 / 1.00 | × 0.8 |
| `gpt-5-mini` | 0.25 / 2.00 / 0.025 | 0.20 / 1.60 / 0.02 | × 0.8, flex and priority tiers too |
| `gpt-5` | 1.25 / 10.00 / 0.125 | 1.00 / 8.00 / 0.10 | set in the file: no multiplier, and no flex or priority price unless listed |
| `my-vllm-llama` | none (`unknown_model`) | 0.10 / 0.20 / none | added |
| `claude-sonnet-4-5` | 3.00 / 15.00 / 0.30 | unchanged | another provider |

## When it applies

- The chart mounts the table as a file (`LLM_PRICES_CONFIG_FILE`) that the agent rereads every 10 seconds: after a
  `helm upgrade`, new prices apply without a restart once the kubelet refreshes the volume, in about a minute.
- Turning `prices` on or off changes the DaemonSet, so that one upgrade restarts the pods.
- Your table goes through the same checks as the built-in one. A rejected table is logged and the previous prices stay.
- Outside the chart, point `LLM_PRICES_CONFIG_FILE` at the file. An inline `LLM_PRICES_CONFIG` is read at startup only.

## Check it

- The agent log names what changed, in the `prices_loaded` audit event: `replaced="gpt-5" added="my-vllm-llama"` ([logs](logs.md)).
- `price_table_version` on the cost metrics becomes `<built-in version>+custom-<hash>`, so a price change shows as a
  new series.
- Models that had no price should stop counting as unpriced:

```promql
sum by (model, price_table_version) (increase(honeybeepf_llm_llm_unpriced_tokens_total{reason="unknown_model"}[1h]))
```

Tokens that still have no price are counted in `llm_unpriced_tokens_total` with a `reason`; see
[Cost and prices](../cost.md).

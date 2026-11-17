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

- `price_multiplier` scales the provider's built-in prices, not the models you list.
- Changes reach the agent within about a minute, without a restart. A file that is rejected is logged and ignored.
- `price_table_version` on the cost metrics becomes the built-in version with `+custom-<hash>` appended, so a price
  change shows as a new series.

Tokens that still have no price are counted in `llm_unpriced_tokens_total` with a `reason`; see
[Cost and prices](../cost.md).

# Detect unlisted hosts by shape

!!! warning "Experimental"
    Off by default and may change. On a test VM, 44 ordinary HTTPS calls (GitHub, PyPI, npm, Docker Hub and echo
    services on LLM paths) and 15 non-LLM JSON responses on LLM paths recorded nothing with it on.

Normally only calls to known hosts are recorded. With detection by shape, the agent also records calls to hosts no
provider lists, such as a new provider, a gateway or an in-cluster model server, when they look like an LLM call.

```yaml
builtinProbes:
  llm:
    detectByShape: true
```

## When a call counts

A call to an unlisted host is recorded only when **both** are true:

1. The request path matches a built-in pattern: `/chat/completions`, `/v1/responses`, `/v1/messages`,
   `generateContent` and the like.
2. The response carries that API family's usage fields.

A path match alone never records anything. It is not counted in `llm_responses_unparsed_total` either, since it may
not have been an LLM call.

| Covered | Not covered |
|---|---|
| OpenAI-compatible APIs: Groq, DeepSeek, Together, Fireworks, OpenRouter, Mistral, xAI, Perplexity, Cerebras, vLLM, LM Studio | responses with no `usage` object |
| Anthropic- and Gemini-shaped APIs | other paths, such as Ollama's native `/api/chat`: [add it as a provider](custom-providers.md) |

## Telling them apart

A call recorded this way carries `match="shape"` on its request, token and cost metrics. A call matched by its host
has no `match` label.

## Gateways are counted twice

An in-cluster gateway reached over TLS (LiteLLM, a company proxy) is then recorded twice:

| Record | Namespace | Tells you |
|---|---|---|
| app → gateway (`match="shape"`) | the app's | which team spent it |
| gateway → provider | the gateway's | what the provider bills |

Spend per namespace is right either way: each record belongs to its own namespace. For a total over all of them,
list the gateway as a provider with `gateway: true`; its calls then carry `hop="gateway"` instead of
`match="shape"`, and the dashboard totals leave them out ([Calls through a gateway](../cost.md#calls-through-a-gateway)).
Without that entry, leave out the shape-matched calls:

```promql
sum(increase(honeybeepf_llm_llm_cost_usd_total{match!="shape"}[1d]))
```

A gateway outside the cluster (OpenRouter, a hosted proxy), or one called over plain HTTP without a `plaintext`
entry, is recorded once.

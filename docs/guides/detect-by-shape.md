# Detect unlisted hosts by shape (experimental)

This mode is experimental and may change. On a VM, 44 ordinary HTTPS calls (GitHub, PyPI, npm, Docker Hub and
others over HTTP/1.1 and HTTP/2, plus echo services on LLM paths) and 15 local non-LLM JSON responses on LLM paths
recorded nothing with it on.

`builtinProbes.llm.detectByShape` (default `false`) records OpenAI/Anthropic/Gemini-compatible calls to hosts no
entry lists — new providers, gateways, in-cluster model servers — by request path and response shape instead of
host:

```yaml
builtinProbes:
  llm:
    detectByShape: true
```

When on, an HTTP/1.1 or HTTP/2 request whose path matches a built-in pattern (`/chat/completions`, `/v1/responses`,
`/v1/messages`, `generateContent`, …) is kept even if its `Host` is unlisted, and it is recorded **only** if the
response carries that family's usage fields; a response without them is dropped, so a path match alone never
records. When off (the default) only listed hosts are recorded, exactly as before. This covers the common
OpenAI-compatible APIs (Groq, DeepSeek, Together, Fireworks, OpenRouter, Mistral, xAI, Perplexity, Cerebras, vLLM,
LM Studio); it does not catch shapes with no `usage` object or a non-matching path (e.g. Ollama's native
`/api/chat`, which needs an explicit `providers` entry).

A call recorded this way carries `match="shape"` on its request, token and cost metrics; a call matched by its
host has no `match` label. A path match whose response has no usage is not counted in
`llm_responses_unparsed_total` either, since it may not have been an LLM call.

An in-cluster gateway reached over TLS (LiteLLM, a company proxy) is then recorded twice: the app's call to the
gateway, under the app's namespace, and the gateway's call to the provider, under the gateway's. The first is the
one that says which team spent it; the second is the one the provider bills. To total spend without counting such
calls twice, leave out one side, for example the shape-matched calls when the gateway's own upstream calls are
recorded:

```promql
sum by (target_namespace) (increase(honeybeepf_llm_llm_cost_usd_total{match!="shape"}[1d]))
```

A gateway outside the cluster (OpenRouter, a hosted proxy), or one called over plain HTTP without a `plaintext`
entry, is recorded once.

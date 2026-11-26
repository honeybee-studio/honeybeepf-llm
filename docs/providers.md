# What is supported

Four questions decide whether a call is recorded: which **provider** it goes to, which **runtime** makes it, which
**protocol** carries it and which **kernel** the node runs.

!!! info "What ✅ means"
    The agent recorded exactly the expected token counts on Linux 7.0, against a local TLS mock of the provider's API
    called by curl and by the official Python and Node.js SDKs, with recorded real responses replayed as well. A blank
    cell was not checked.

## Providers

Built in, no configuration:

| Provider / API | JSON | Streaming | Cache tokens |
|---|:-:|:-:|:-:|
| OpenAI Chat Completions | ✅ | ✅ | ✅ read |
| OpenAI Responses API | ✅ | ✅ | |
| Anthropic Messages | ✅ | ✅ | ✅ read/write |
| Google Gemini (`generateContent`, `streamGenerateContent`) | ✅ | ✅ | ✅ read |
| Google Gemini Interactions API (Gemini API and Vertex AI) | ✅ | ✅ | ✅ read |
| Google Gemini Decisions API (`gemini-decision-1`)¹ | ✅ | | |
| Azure OpenAI | ✅ Chat Completions | ✅ Responses API | |
| Google Vertex AI (Gemini) | ✅ | ✅ | |
| Claude on Vertex AI, Google Cloud and Microsoft Foundry | ✅ | ✅ | ✅ read/write |
| Amazon Bedrock Converse | ✅ | ✅ ConverseStream | ✅ read/write |
| Amazon Bedrock InvokeModel (Claude models) | ✅ | ✅ every model | ✅ read/write |
| Meta Model API, Muse Spark (`api.meta.ai`) | ✅ | ✅ | |
| [OpenAI-compatible providers](#openai-compatible-providers) | Chat Completions | | |

¹ Input tokens only. The response names no model, so these calls are unpriced.

Compressed responses (`gzip`, `br`, `zstd`, `deflate`) are decoded, streams included.

### OpenAI-compatible providers

Their Chat Completions calls are read like OpenAI's:

| | | |
|---|---|---|
| Groq | DeepSeek | Together |
| Fireworks | OpenRouter | Mistral |
| Cerebras | Alibaba Qwen (Model Studio, DashScope) | Moonshot (Kimi) |
| Zhipu / Z.ai (GLM) | MiniMax | SiliconFlow |
| Volcengine Ark, BytePlus ModelArk | DeepInfra | Nebius Token Factory |
| SambaNova | Novita | Hyperbolic |
| Baseten | NVIDIA API Catalog | Hugging Face router |
| Cloudflare Workers AI | Vercel AI Gateway | Upstage |
| AI21 | | |

- **Hosts** come from each provider's API reference, regional and workspace hosts included; the full list is the
  `openai-compatible` entry of the
  [built-in list](https://github.com/honeybee-studio/honeybeepf-llm/blob/main/honeybeepf-llm/honeybeepf-llm-core/src/providers/default_providers.json).
- **Only Chat Completions**, not their Responses or embeddings endpoints.
- **No cost by default.** Their models are not in the price table, so calls are recorded with tokens only until you
  [add their prices](guides/prices.md).
- **Routers** (Hugging Face, Vercel, OpenRouter) give one record per app call, with the usage the routed provider
  reported.

??? note "Left out for now, and why"
    | Provider | Why |
    |---|---|
    | xAI | its `completion_tokens` leave out reasoning tokens, so it would be undercounted |
    | Databricks | its reasoning and cache counts sit at the top of `usage` |
    | Cohere | its compatibility API documents no usage |
    | Cloudflare AI Gateway | its cache hits return usage the provider never billed |

!!! tip "Streaming without usage"
    Some providers send streaming usage only when the request sets `stream_options.include_usage`, as OpenAI does. A
    stream without it is counted under `llm_responses_unparsed_total{reason="usage_absent"}`.

### Not listed?

- [Add a provider](guides/custom-providers.md) by host, path and usage fields.
- Add an in-cluster model server as a [plain-HTTP provider](guides/plaintext.md).
- Record calls to unlisted hosts by their [shape](guides/detect-by-shape.md).

## Runtimes

Also checked in pods on k3s: `python:3.12-slim`, `python:3.13-alpine`, `node:24-slim`.

| Runtime | TLS library | Seen |
|---|---|:-:|
| Python (CPython: slim, Alpine, distro) | shared `libssl` | ✅ |
| Node.js official binaries and `node:` images | OpenSSL inside the `node` executable | ✅ |
| Node.js from distro packages | shared `libssl` | ✅ |
| curl and other programs linked to `libssl` | shared `libssl` | ✅ |
| Go 1.20+, stripped or not, amd64 and arm64 | `crypto/tls` inside the executable | ✅ |
| Go before 1.20, `crypto/tls` forks (uTLS), rustls, Java (JSSE), GnuTLS, NSS | not probed | ❌ |

To list the workloads in your cluster that are not seen, see [Find the workloads that are not seen](guides/coverage.md).

## Protocols

| Protocol | Seen |
|---|:-:|
| HTTP/1.1 (keep-alive, chunked) | ✅ |
| HTTP/2 | ✅ |
| WebSocket: OpenAI Realtime, Responses over a WebSocket (OpenAI and Azure) | ✅ |
| WebSocket: other APIs (Gemini Live) | ❌ |
| HTTP/3 (QUIC) | ❌ |

- **WebSocket** calls are read from the server's messages only: each `response.done` (Realtime) or
  `response.completed` (Responses) is one call, with its audio tokens split out. Compressed messages are read. The
  client's messages, such as streamed audio, are not kept. Realtime models have no built-in price yet, so they count
  as unpriced until one is [added](guides/prices.md).
- **HTTP/2** is recorded from the start of a connection. A connection the agent joins mid-stream is still recorded,
  with the provider host unknown; a request head lost to a capture gap counts under
  `llm_responses_unparsed_total{reason="headers_lost"}`.
- **Background Responses calls** (`"background": true`) are linked to the later poll that carries their usage, over
  HTTP/1.1 and HTTP/2.
- **Node.js 26** uses HTTP/2 by default in its built-in `fetch`, and the official OpenAI and Anthropic Node SDKs record
  with no change (checked with Node.js 26.13.0, `openai` 7.39.0 and `@anthropic-ai/sdk` 0.135.0).

## Kernels

CI loads and attaches the probes and checks exact token counts on each of these, against a local mock:

| Architecture | Kernels |
|---|---|
| x86-64 | 5.10, 5.15, 6.1, 6.6, 6.12, 6.18, 7.2 |
| x86-64, RHEL | 8.10 (4.18), 9.6 and 9.7 (5.14) |
| arm64 | 5.15, 6.1 |

Other RHEL minor releases are not tested. The full provider and runtime tables above are checked on Linux 7.0. The
lanes are in the
[kernel matrix](https://github.com/honeybee-studio/honeybeepf-llm/blob/main/.github/workflows/kernel-matrix.yml).

CPU, memory, latency and accuracy under load are in the [overhead benchmark](benchmark.md).

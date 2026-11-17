# What is supported

✅ means the agent recorded exactly the expected token counts on Linux 7.0, against a local TLS mock of each
provider's API called by curl and by the official Python and Node.js SDKs; recorded real provider responses are
replayed against it as well. A blank cell was not checked.

**Providers** (built in, no configuration)

| Provider / API | JSON | Streaming (SSE) | Cache tokens |
|---|:-:|:-:|:-:|
| OpenAI Chat Completions | ✅ | ✅ | ✅ read |
| OpenAI Responses API | ✅ | ✅ | |
| Anthropic Messages | ✅ | ✅ | ✅ read/write |
| Google Gemini (`generateContent`, `streamGenerateContent`) | ✅ | ✅ | ✅ read |
| Google Gemini Interactions API (Gemini API and Vertex AI) | ✅ | ✅ | ✅ read |
| Google Gemini Decisions API (`gemini-decision-1`, input tokens only; the response names no model, so calls are unpriced) | ✅ | | |
| Azure OpenAI | ✅ Chat Completions | ✅ Responses API | |
| Google Vertex AI (Gemini) | ✅ | ✅ | |
| Claude on Vertex AI, Google Cloud and Microsoft Foundry | ✅ | ✅ | ✅ read/write |
| Amazon Bedrock Converse | ✅ | ✅ ConverseStream (event stream) | ✅ read/write |
| Amazon Bedrock InvokeModel (Anthropic Claude models) | ✅ | ✅ InvokeModelWithResponseStream, every model | ✅ read/write |
| Meta Model API — Muse Spark (`api.meta.ai`: Chat Completions, Responses, Messages) | ✅ | ✅ | |
| OpenAI-compatible Chat Completions of 25 hosted providers, listed below (usage read as OpenAI Chat Completions) | | | |

Compressed responses (`gzip`, `br`, `zstd`, `deflate`) are decoded, SSE included.

The OpenAI-compatible providers are Groq, DeepSeek, Together, Fireworks, OpenRouter, Mistral, Cerebras, Alibaba Qwen
(Model Studio / DashScope), Moonshot (Kimi), Zhipu / Z.ai (GLM), MiniMax, SiliconFlow, Volcengine Ark and BytePlus
ModelArk, DeepInfra, Nebius Token Factory, SambaNova, Novita, Hyperbolic, Baseten, NVIDIA API Catalog, the Hugging Face
router, Cloudflare Workers AI, Vercel AI Gateway, Upstage and AI21. Their hosts, regional and workspace hosts included,
come from each provider's API reference, which documents OpenAI's usage with reasoning tokens inside
`completion_tokens`; the full list is the `openai-compatible` entry of the built-in list. Only Chat Completions is
listed, not their Responses or embeddings endpoints. Their models are not in the price table, so these calls are
recorded with tokens and no cost unless a price file names the model. A router or gateway (Hugging Face, Vercel,
OpenRouter) gives one record per app call, with the usage the routed provider reported.

Left out for now: xAI, whose `completion_tokens` leave out reasoning tokens (usage is read by response shape, so it
would be undercounted); Databricks, whose reasoning and cache counts sit at the top of `usage`; Cohere, whose
compatibility API documents no usage; and Cloudflare AI Gateway, whose cache hits return usage the provider never
billed. Some providers send streaming usage only with `stream_options.include_usage`, as OpenAI does; a stream without
it is counted under `llm_responses_unparsed_total{reason="usage_absent"}`.

Not listed? [Add a provider](guides/custom-providers.md) by host, path and usage fields, add an in-cluster model
server as a [plain-HTTP provider](guides/plaintext.md), or record calls to unlisted hosts by their
[shape](guides/detect-by-shape.md).

**Runtimes** (also checked in pods on k3s: `python:3.12-slim`, `python:3.13-alpine`, `node:24-slim`)

| Runtime | TLS library | Status |
|---|---|:-:|
| Python (CPython: `python:*-slim`, Alpine, distro) | shared `libssl` | ✅ |
| Node.js official binaries and `node:` images | OpenSSL inside the `node` executable | ✅ |
| Node.js from distro packages | shared `libssl` | ✅ |
| curl and other programs linked to `libssl` | shared `libssl` | ✅ |
| Go 1.20+ (`crypto/tls`), stripped or not, amd64 and arm64 | `crypto/tls` inside the executable | ✅ |
| Go before 1.20, forks of `crypto/tls` (such as uTLS), rustls, Java (JSSE), GnuTLS, NSS | not probed | ❌ not seen |

**Protocols**

| Protocol | Status |
|---|:-:|
| HTTP/1.1 (keep-alive, chunked) | ✅ |
| HTTP/2 | ✅ |
| WebSocket (e.g. OpenAI Realtime) | ❌ |
| HTTP/3 (QUIC) | ❌ |

HTTP/2 is recorded from the start of a connection. A connection the agent joins mid-stream is still recorded, but
with the provider host unknown; a request head lost to a capture gap counts under
`llm_responses_unparsed_total{reason="headers_lost"}`. A Responses call made with `"background": true` is linked
to the later poll that carries its usage over HTTP/2 as well as HTTP/1.1. Node.js 26 uses HTTP/2 by default (its built-in `fetch`,
undici 8), so the official OpenAI and Anthropic Node SDKs now record with no change, checked with Node.js 26.13.0,
`openai` 7.39.0 and `@anthropic-ai/sdk` 0.135.0, JSON and streaming.

**Kernels:** CI loads and attaches the probes and records exact token counts (OpenAI JSON, streaming and gzip,
and Anthropic with a cache read, against a local mock) on 5.10, 5.15, 6.1, 6.6, 6.12, 6.18 and 7.2, on the
RHEL 8.10 (4.18), 9.6 and 9.7 (5.14) kernels, and on 5.15 and 6.1 on arm64
([kernel matrix](https://github.com/honeybee-studio/honeybeepf-llm/blob/main/.github/workflows/kernel-matrix.yml)); other RHEL minor releases are not tested. The full
provider and runtime tables above are checked on Linux 7.0.

**Overhead:** CPU, memory, latency and accuracy under load are measured in [docs/benchmark.md](benchmark.md).

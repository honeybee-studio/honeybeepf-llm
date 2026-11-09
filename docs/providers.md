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
| Azure OpenAI | ✅ Chat Completions | ✅ Responses API | |
| Google Vertex AI (Gemini) | ✅ | ✅ | |
| Claude on Vertex AI, Google Cloud and Microsoft Foundry | ✅ | ✅ | ✅ read/write |
| Amazon Bedrock Converse | ✅ | ✅ ConverseStream (event stream) | ✅ read/write |
| Amazon Bedrock InvokeModel (Anthropic Claude models) | ✅ | ✅ InvokeModelWithResponseStream, every model | ✅ read/write |
| Meta Model API — Muse Spark (`api.meta.ai`: Chat Completions, Responses, Messages) | ✅ | ✅ | |

Compressed responses (`gzip`, `br`, `zstd`, `deflate`) are decoded, SSE included.

`builtinProbes.llm.providers` in `values.yaml` describes providers by host, path and usage fields, in the format of
the [built-in list](../honeybeepf-llm/honeybeepf-llm-core/src/providers/default_providers.json). They are added to the
built-in providers; a provider with a built-in name (`openai`, `anthropic`, `gemini`, `bedrock`, matched exactly)
replaces every built-in entry of that name, so an `openai` entry must also cover the Responses API and an
`anthropic` entry Claude on Vertex AI. Usage is read by matching the response's fields, not its host, with the
built-in entries tried first: a gateway that returns an OpenAI-, Anthropic- or Gemini-shaped body is read with the
built-in fields.

By default only HTTPS calls made through OpenSSL are seen. An in-cluster **plain-HTTP** endpoint (a typical Ollama
or vLLM) is captured at the socket layer when a provider opts in with `plaintext`:

```yaml
builtinProbes:
  llm:
    providers:
      - name: vllm                 # a non-built-in name, so it is added, not replacing a built-in
        hosts: ["vllm.default.svc.cluster.local"]   # must match the Host header the app sends
        paths: ["/v1/chat/completions"]             # optional; omit to match any path on the host
        plaintext: { port: 8000 }
```

The agent resolves those Service endpoints and records matching calls with `transport="plain"` (TLS calls carry
`transport="tls"`), with no proxy and no app change. It needs a node with cgroup v2 and kernel 5.10+; the chart
grants read access to `services` and `endpointslices` only when a provider sets `plaintext`. OpenAI-compatible
servers need no `response` block, and the request's `Host` header must match one of the provider's `hosts`. IPv4 only so far.

**Runtimes** (also checked in pods on k3s: `python:3.12-slim`, `python:3.13-alpine`, `node:24-slim`)

| Runtime | TLS library | Status |
|---|---|:-:|
| Python (CPython: `python:*-slim`, Alpine, distro) | shared `libssl` | ✅ |
| Node.js official binaries and `node:` images | OpenSSL inside the `node` executable | ✅ |
| Node.js from distro packages | shared `libssl` | ✅ |
| curl and other programs linked to `libssl` | shared `libssl` | ✅ |
| Go (`crypto/tls`), rustls, Java (JSSE), GnuTLS, NSS | not OpenSSL | ❌ not seen |

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

**Kernels:** CI loads and attaches the probes and detects an LLM request on 5.10, 5.15, 6.1, 6.6, 6.12, 6.18
and 7.2, and on the RHEL 8.10 (4.18), 9.6 and 9.7 (5.14) kernels
([kernel matrix](../.github/workflows/kernel-matrix.yml)); other RHEL minor releases are not tested. Token counts
are checked on Linux 7.0 (the tables above).

**Overhead:** CPU, memory, latency and accuracy under load are measured in [docs/benchmark.md](benchmark.md).

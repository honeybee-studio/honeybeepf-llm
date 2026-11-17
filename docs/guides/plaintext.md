# Self-hosted models over plain HTTP

By default only HTTPS calls made through OpenSSL or Go `crypto/tls` are seen. An in-cluster **plain-HTTP** endpoint
(a typical Ollama or vLLM) is captured at the socket layer when a provider opts in with `plaintext`:

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

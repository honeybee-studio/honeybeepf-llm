# Self-hosted models over plain HTTP

By default the agent reads HTTPS calls made through OpenSSL or Go `crypto/tls`. A model server inside the cluster is
often called over plain HTTP instead (a typical Ollama or vLLM), so there is no TLS library to read. For those, the
agent copies the bytes at the socket, still with no proxy and no change to the app.

## Turn it on

Add the server as a provider with `plaintext`:

```yaml
builtinProbes:
  llm:
    providers:
      - name: vllm                                  # not a built-in name, so it is added
        hosts: ["vllm.default.svc.cluster.local"]   # must match the Host header the app sends
        paths: ["/v1/chat/completions"]             # optional; omit to match any path on the host
        plaintext: { port: 8000 }
```

An OpenAI-compatible server such as vLLM needs no `response` block. For another API, describe its usage fields as in
[Add a provider](custom-providers.md), and [add its prices](prices.md) to cost it.

## What happens

- The agent resolves the Service's endpoints and watches connections to them.
- Matching calls are recorded with `transport="plain"`; TLS calls carry `transport="tls"`.
- The chart grants read access to `services` and `endpointslices` only when a provider sets `plaintext`.

## Requirements and limits

- Nodes with cgroup v2 and Linux 5.10 or later.
- The request's `Host` header must match one of the provider's `hosts`.
- IPv4 only for now.
- A receive is captured up to 64 KB in one record; a larger one is marked in
  `llm_events_total{stage="plain_recv_truncated"}`.

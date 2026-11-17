# FAQ

## Do I have to change my apps?

No. Apps keep calling the provider directly with their own SDKs and keys. There is no base URL to change, no library
to import and nothing to redeploy. See [How it works](how-it-works.md).

## Does it sit in the request path or slow my calls down?

No. The agent reads copies of the TLS buffers on the node; requests go to the provider exactly as before, and nothing
waits for the agent. The CPU and memory it uses, and the effect on throughput under load, are measured in the
[overhead benchmark](benchmark.md).

## Does it read my prompts and API keys?

The probes copy the decrypted bytes of each call into the agent's memory on the node, as any TLS-reading tool must. The
agent parses them for the usage fields and drops them: prompts, responses, headers and API keys are never exported or
logged. The [security review](security-review.md#3-data-handling) says what is exported and logged, field by field.

## How accurate are the numbers?

Token counts are the provider's own: the agent reads the usage fields of each response, cache and reasoning tokens
included. Cost is an estimate: those tokens multiplied by a dated price table, which you can overlay with your
contract prices ([Cost and prices](cost.md)). Calls the agent saw but could not read are counted, with a reason,
rather than dropped silently.

## Which providers, languages and protocols does it cover?

OpenAI, Anthropic, Gemini, Bedrock, Azure OpenAI, Vertex AI, Meta and many OpenAI-compatible APIs, from Python,
Node.js, Go, curl and anything else using OpenSSL, over HTTP/1.1 and HTTP/2 with streaming. Not yet: rustls, Java,
WebSocket and HTTP/3. The full matrix is in [What is supported](providers.md), and
[a metric](guides/coverage.md) lists the workloads in your cluster that are not seen.

## We already run an LLM gateway such as LiteLLM. Do we need this?

A gateway sees the calls routed through it; the agent sees every call made from the node, including the ones that go
around the gateway, and names the pod that made each one, with no change to the apps. If you turn on
[detection by shape](guides/detect-by-shape.md), a gateway inside the cluster is recorded twice (app to gateway,
gateway to provider); the page shows how to count it once.

## What does it need on the nodes?

Linux 5.10 or later, or a RHEL 8.10, 9.6 or 9.7 kernel, with BTF, on x86-64 or arm64. The agent is not privileged: it
runs with `hostPID` and a short list of capabilities, explained in [Install](install.md#permissions-and-resources)
and the [security review](security-review.md#2-privileges).

## Does it work without Kubernetes?

Yes, as a binary on a Linux host, built without the `k8s` feature; the metrics then carry no namespace, pod or
workload labels. See [Build from source](build.md).

## Where do I report a bug or ask for a provider?

In [GitHub Issues](https://github.com/honeybee-studio/honeybeepf-llm/issues).

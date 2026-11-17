# How it works

An app that calls an LLM provider hands the request to its TLS library in plaintext and gets the response back from it
in plaintext. honeybeepf-llm reads a copy of those bytes at that point, on the node, with eBPF. The app, its SDK and the
network path to the provider stay as they are.

![uprobes on SSL_write/SSL_read copy plaintext into a ring buffer; the agent parses HTTP, matches the provider, maps the cgroup to a pod and exports OTLP metrics](images/how-it-works.svg)

## 1. Find the TLS code

The agent runs as a DaemonSet, one pod per node. It looks at every process on its node and finds where the process does
TLS:

| The process | The agent attaches to |
|---|---|
| maps a shared `libssl.so` (Python, Node.js from distro packages, curl) | `SSL_read`, `SSL_write` and their `_ex` variants in that library |
| has OpenSSL built into its executable (the official Node.js binaries) | the same functions in the executable |
| is a Go 1.20+ executable | `crypto/tls` `(*Conn).Write`, `Read` and each return of `Read`, found from the Go function table, stripped binaries included |

It attaches to processes that start later too: a probe on `exec` and on opening a `libssl` file tells it about new
ones. Programs that use another TLS stack, such as rustls or Java's JSSE, are not seen yet; the
[coverage metric](guides/coverage.md) lists them.

## 2. Copy the plaintext

Each probe is an eBPF program that runs when the function is called or returns. It copies the plaintext buffer (up to
16 KB of a read and 4 KB of a write), the process id and the cgroup id into a ring buffer shared with the agent. The
programs only read: none of them changes the call, the data or the connection.

An in-cluster model server called over plain HTTP, such as Ollama or vLLM, has no TLS library to hook. For a provider
you mark with `plaintext`, the agent copies the bytes at the socket instead (see
[Self-hosted models over plain HTTP](guides/plaintext.md)).

## 3. Turn bytes into calls

In user space, the agent reassembles each connection's bytes into HTTP/1.1 or HTTP/2 requests and responses,
decompresses them (`gzip`, `br`, `zstd`, `deflate`) and follows server-sent event streams to their end. It picks the
provider from the request's host and path and reads the token counts from the response's usage fields: prompt,
completion, cache read and write, and reasoning tokens.

A call the agent saw but could not read is not dropped silently: it is counted in `llm_responses_unparsed_total` with
the reason.

## 4. Find the pod

The process's cgroup gives its container ID. A watch on this node's pods maps the container to its namespace, pod and
owner workload, such as a Deployment or a CronJob.

## 5. Price the call

The model name in the response is looked up in a dated built-in price table, with your own prices or discounts on top.
The call's cost is split the way providers bill it: input, cache read, cache write and output. A model with no price
is counted in `llm_unpriced_tokens_total`, never recorded as $0.

## 6. Export

The agent exports tokens, requests, latency and cost as OpenTelemetry metrics over OTLP, labelled by namespace, pod,
workload and model, and logs one line per call with its model, request path, pod, latency and token counts. The
captured bytes stay in the agent's memory and are dropped once parsed: prompts, responses, headers and API keys are
never exported or logged.

The [security review](security-review.md) lists every eBPF program, the privileges the agent needs and what it can and
cannot see.

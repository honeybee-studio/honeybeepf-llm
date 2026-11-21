# Experimental features

These features are off by default, may change between releases, and are turned on one by one. At startup the
agent logs the ones that are on, so an operator or a security team can see them in the pod log:

```
experimental features on: aead
```

| Feature | Setting | What it adds | What to weigh | To leave experimental |
|---|---|---|---|---|
| [Detect unlisted hosts by shape](guides/detect-by-shape.md) | `builtinProbes.llm.detectByShape` (`BUILTIN_PROBES__LLM_DETECT_BY_SHAPE`) | Calls to hosts no provider lists, when they look like an LLM call | A non-LLM service on an LLM path could be counted; a gateway's calls are counted twice unless it is marked | No false positive reported for one release, and the false-positive check run on a larger set of public services |
| [Programs with built-in TLS](guides/built-in-tls.md) | `builtinProbes.llm.aead` (`BUILTIN_PROBES__LLM_AEAD`) | Programs with their own BoringSSL or aws-lc, such as Claude Code | Reads part of those programs' memory to find the cipher table ([security review](security-review.md#3-data-handling)); once a table is found, every TCP send and receive on the node runs one map lookup | The weekly canary ([#324](https://github.com/honeybee-studio/honeybeepf-llm/issues/324)) passes three weeks in a row; overhead measured on a node with Envoy sidecars, which also use BoringSSL; the memory scan follows `namespaces`; tested on arm64 |

A feature leaves experimental by turning its default on, with its setting name unchanged, so existing values keep
working.

# Logs and audit events

The agent writes its log to stdout. `RUST_LOG` sets the levels (the chart's `rustLog`, `info` by default) and
`LOG_FORMAT` the format (the chart's `logFormat`).

| `LOG_FORMAT` | Each line |
|---|---|
| `text` (default) | time, level, target, message, then any fields as `name=value` |
| `json` | one JSON object: `timestamp`, `level`, `target` and `fields` with the message and the event's fields |

## What `info` logs

- One `LLM SUCCESS` line per recorded call: process, model, tokens and pod.
- `LLM UNPARSED` for a call the agent saw but could not record, with the reason.
- Kernel event counters and open streams, once a minute when they changed.
- Audit events, below.

Per-connection lines (`[LLM] Detected …`) and HTTP/2 streams whose request was never seen are at `debug`.

## Audit events

Audit events say what the agent did on the node. They have target `audit`, a fixed `event` name and their details
as fields, and they stay on at `info` unless `RUST_LOG` names `audit` itself.

| `event` | When | Fields |
|---|---|---|
| `startup` | the agent starts | `version`, `kernel` |
| `attach` | LLM probes attached to a library or executable | `kind` (`libssl`, `exe_openssl`, `exe_go`, `exe_aead` with the experimental AEAD probes), `path`, `ino`, `detail` |
| `attach_failed` | a library or executable with TLS code could not be probed (warn) | `path`, `error` |
| `plaintext_capture` | plaintext capture turned on, or configured but off (warn) | `enabled`, `cgroup`, `endpoints` or `reason` |
| `providers_loaded` | the provider set the probe runs with | `providers`, `entries`, `config_hash` |
| `prices_loaded`, `prices_reloaded` | the price table used for cost, when metrics export starts and when your price file changes | `version` (or `from`, `to`), `replaced`, `added` |

A JSON audit event:

```json
{"timestamp":"2026-11-17T15:19:54.437691Z","level":"INFO","fields":{"message":"LLM probes attached","event":"attach","kind":"libssl","path":"/usr/lib/x86_64-linux-gnu/libssl.so.3","ino":102273,"detail":""},"target":"audit"}
```

To keep only warnings and audit events:

```bash
helm upgrade --install honeybeepf-llm charts/honeybeepf-llm -n honeybeepf --reuse-values \
  --set rustLog=warn --set logFormat=json
```

Field values are escaped, so a path or provider name with a newline in it cannot start a line of its own.

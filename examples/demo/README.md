# Demo without API keys

`demo.sh` runs honeybeepf-llm against a mock LLM provider inside the cluster, so you can see per-pod tokens and
dollars in Grafana without an API key or a single real LLM call.

It installs, in this order:

| Namespace | What | From |
|---|---|---|
| `llm-demo` | mock provider answering OpenAI, Anthropic and Gemini calls over TLS with fixed usage | `mock.py` on `python:3.12-slim` |
| `honeybeepf` | the agent, a Prometheus and an OpenTelemetry Collector | the release chart, and the bundled charts in `charts/` |
| `monitoring` | Grafana with the LLM Cost Observability dashboard | the `grafana/grafana` chart and `docs/grafana/honeybeepf-cost.json` |
| `team-a`, `team-b`, `team-c` | LLM clients: Python (OpenAI, Anthropic, with cache hits and a server-side fallback) and the official Node.js image (Gemini) | `client.py`, `client.mjs` |

The clients resolve `api.openai.com`, `api.anthropic.com` and `generativelanguage.googleapis.com` to the mock
through `hostAliases` and trust a CA the script creates for the demo, so TLS is verified and nothing leaves the
cluster. The model names are real and priced; the token counts are fixed demo values.

## Run it

Requires a Kubernetes cluster (kind, k3d, minikube, k3s or a cloud cluster), `kubectl`, Helm 3 and `openssl`.

```bash
examples/demo/demo.sh up
kubectl -n honeybeepf logs ds/honeybeepf-llm -f | grep 'LLM SUCCESS'
kubectl -n monitoring port-forward svc/grafana 3000:80
```

Open <http://localhost:3000> and the "LLM Cost Observability" dashboard. Calls show in the agent log within
seconds, and in Grafana after the first scrape, about a minute.

`HB_DEMO_CHART=local examples/demo/demo.sh up` installs `charts/honeybeepf-llm` from this checkout with the image
built from `main` (`latest`) instead of the release chart.

Remove everything:

```bash
examples/demo/demo.sh down
```

## Notes

- `team-c` uses the official Node.js image, whose `node` binary has OpenSSL built in, so the agent hooks the
  executable rather than a shared libssl.

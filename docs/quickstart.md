# Quickstart

See the LLM calls of every pod on your cluster in a few minutes. This installs the agent only and reads the calls from
its log; [Install](install.md) adds Prometheus, an OpenTelemetry Collector and the Grafana cost dashboard.

## Before you start

- A Kubernetes cluster whose nodes run Linux 5.10 or later, or a RHEL 8.10, 9.6 or 9.7 kernel.
- `kubectl` and Helm 3.
- Pods that call an LLM provider over HTTPS from Python, Node.js, Go, curl or anything else that uses OpenSSL. No LLM
  traffic yet? Run the [demo](demo.md) instead.

## 1. Install the agent

```bash
kubectl create namespace honeybeepf
helm install honeybeepf-llm oci://ghcr.io/honeybee-studio/charts/honeybeepf-llm -n honeybeepf
```

The chart runs one agent pod per node as a DaemonSet. Your apps, their SDKs and their API keys stay as they are.

## 2. Watch the calls

```bash
kubectl logs -n honeybeepf ds/honeybeepf-llm -f | grep 'LLM SUCCESS'
```

Each LLM call is one line, with the model, the token counts and the pod that made it:

```
LLM SUCCESS | PID: 1686847 | Model: claude-test | Latency: 0.00s | Tokens: 140 (Prompt: 125, Compl: 15, Cache read: 100, Cache write: 0) | Pod: team-b/support-bot-589db898dd-rv9ts
```

`ds/honeybeepf-llm` follows one agent pod. On a cluster with several nodes, each node's calls are in that node's
agent pod: `kubectl logs -n honeybeepf -l app.kubernetes.io/name=honeybeepf-llm --prefix -f` follows them all.

## 3. Next

- [Send the metrics to Prometheus and open the cost dashboard](install.md).
- [Check which of your workloads are seen](metrics.md#tls-coverage).
- [See how calls are priced](metrics.md#cost).

To remove it:

```bash
helm uninstall honeybeepf-llm -n honeybeepf
kubectl delete namespace honeybeepf
```

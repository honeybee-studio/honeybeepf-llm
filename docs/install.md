# Installation

## Quickstart

Requires a Kubernetes cluster whose nodes run a Linux 5.10+ or RHEL 8.10 / 9.6 / 9.7 kernel, `kubectl` and Helm 3.

```bash
git clone https://github.com/honeybee-studio/honeybeepf-llm.git
cd honeybeepf-llm
helm repo add prometheus-community https://prometheus-community.github.io/helm-charts
helm repo add open-telemetry https://open-telemetry.github.io/opentelemetry-helm-charts
helm dependency build charts/honeybeepf-llm-prometheus
helm dependency build charts/honeybeepf-llm-otel-collector

kubectl create namespace honeybeepf
helm install honeybeepf-llm-prometheus charts/honeybeepf-llm-prometheus -n honeybeepf
helm install honeybeepf-llm-otel-collector charts/honeybeepf-llm-otel-collector -n honeybeepf
helm install honeybeepf-llm charts/honeybeepf-llm -n honeybeepf \
  --set builtinProbes.llm.enabled=true \
  --set output.otlp.endpoint=honeybeepf-llm-otel-collector-opentelemetry-collector:4317
```

Watch LLM calls as they happen:

```bash
kubectl logs -n honeybeepf ds/honeybeepf-llm -f | grep 'LLM SUCCESS'
```

`ds/honeybeepf-llm` follows one agent pod; on a cluster with several nodes, each node's calls are in that node's
agent pod.

```
2026-10-26T08:43:35.510264Z  INFO honeybeepf_llm::probes::builtin::llm::processor: LLM SUCCESS | PID: 1686847 | Model: claude-test | Latency: 0.00s | Tokens: 140 (Prompt: 125, Compl: 15, Cache read: 100, Cache write: 0) | Pod: team-b/support-bot-589db898dd-rv9ts
```

The agent needs `hostPID` and the `BPF`, `PERFMON`, `SYS_ADMIN`, `SYS_PTRACE`, `NET_ADMIN` and `SYS_RESOURCE` capabilities, and requests 200m CPU / 512Mi per node (limits 1 CPU / 1Gi); see
[`values.yaml`](../charts/honeybeepf-llm/values.yaml).

Two of those capabilities are only needed on older kernels. The chart grants them so one install works on every node;
if all your nodes run a newer kernel, you can remove them:

| Capability | Needed when a node runs | Why | Can be removed when every node runs |
|---|---|---|---|
| `NET_ADMIN` | a kernel before 6.9 | the agent creates a socket map at startup, even with plaintext capture off, and those kernels require `NET_ADMIN` for it | 6.9 or later |
| `SYS_RESOURCE` | a kernel before 5.11 | those kernels count BPF map memory against the memlock limit, which the agent raises at startup | 5.11 or later |

Setting the list replaces it, so keep the other four, for example on nodes that all run 6.9 or later:

```yaml
securityContext:
  capabilities:
    add: [BPF, PERFMON, SYS_ADMIN, SYS_PTRACE]
```

If a capability the node's kernel needs is missing, the agent exits at startup with `Operation not permitted` while
creating a map.

Uninstall:

```bash
helm uninstall honeybeepf-llm honeybeepf-llm-otel-collector honeybeepf-llm-prometheus -n honeybeepf
kubectl delete namespace honeybeepf
```

## Building from source

Requires the nightly toolchain with `rust-src` and `bpf-linker`; see the
[prerequisites](../honeybeepf-llm/docs/BINARY_DEPLOYMENT.md#-prerequisites).

```bash
cd honeybeepf-llm
cargo +nightly build --release --package honeybeepf-llm --features k8s
```

The `k8s` feature adds the pod labels and is not enabled by default; the Docker image is built with it. Without
Kubernetes, see [running the binary](../honeybeepf-llm/docs/BINARY_DEPLOYMENT.md).

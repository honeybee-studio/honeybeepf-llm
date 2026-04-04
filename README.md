# eBPF Observability Platform

**Lightweight eBPF observability for AI workloads**

---

# About

## 1. Project Overview
A lightweight, eBPF-based observability platform designed to identify cost and performance bottlenecks in AI workloads by selectively collecting essential data such as LLM token usage and system metrics.

## 2. Background / Introduction
Traditional observability tools often introduce significant operational overhead due to excessive resource consumption, required application code changes, and complex configuration processes.  

To address these limitations, our platform is built around a Rust-based eBPF agent that collects only essential data at the kernel level without any code modifications.  

The agent can be deployed via Helm Charts in Kubernetes environments or as a standalone binary in traditional AI data centers, enabling cost reduction and performance optimization across heterogeneous infrastructures.

## 3. Core Values
- We practice selective observability—collecting only decision-driving data directly from the kernel.  
- Minimal overhead by design  
- Infrastructure-agnostic: works on Kubernetes and traditional AI data centers  
- Built for AI efficiency: enabling cheaper, faster, and more efficient AI workloads  

---

# Getting Started

## 4. Prerequisites

| Item | Minimum Requirement |
|------|-------------------|
| Kubernetes | v1.23+ |
| Helm | v3.0+ |
| Node CPU | 200m (request) / 1000m (limit) |
| Node Memory | 512Mi (request) / 1Gi (limit) |
| Kernel | Linux 5.8+ (eBPF support required) |
| Capabilities | CAP_BPF, CAP_NET_ADMIN, CAP_PERFMON |

**Required tools:**
- `kubectl` — cluster access and verification
- `helm` — chart installation
- `git` — source cloning

## 5. Installation (under 5 minutes)

### 1) Add Helm repositories and update dependencies

```bash
helm repo add prometheus-community https://prometheus-community.github.io/helm-charts
helm repo add open-telemetry https://open-telemetry.github.io/opentelemetry-helm-charts
helm repo update
```

### 2) Clone and install charts

```bash
git clone https://github.com/honeybee-studio/honeybeepf-llm.git
cd honeybeepf-llm

# Create namespace
kubectl create namespace <your-namespace>

# 1. Install Prometheus
helm dependency build ./charts/honeybeepf-llm-prometheus
helm install honeybeepf-llm-prometheus ./charts/honeybeepf-llm-prometheus -n <your-namespace>

# 2. Install OTel Collector
helm dependency build ./charts/honeybeepf-llm-otel-collector
helm install honeybeepf-llm-otel-collector ./charts/honeybeepf-llm-otel-collector -n <your-namespace>

# 3. Install HoneybeePF agent (edit demo-template.yaml before running)
helm install honeybeepf-llm ./charts/honeybeepf-llm -n <your-namespace> \
  -f ./charts/honeybeepf-llm/values.yaml \
  -f ./charts/honeybeepf-llm/demo-template.yaml
```

> **Note:** Before installing, edit `charts/honeybeepf-llm/demo-template.yaml` and replace `<REPLACE_ME: ...>` placeholders with your actual environment values.

### 3) Verify installation

```bash
kubectl get pods -n <your-namespace>
```

If all Pods show `Running` status, the installation is successful:

```
NAME                                          READY   STATUS    RESTARTS   AGE
honeybeepf-llm-XXXXX                              1/1     Running   0          1m
honeybeepf-llm-otel-collector-XXXXX               1/1     Running   0          2m
honeybeepf-llm-prometheus-server-XXXXX            2/2     Running   0          3m
```

### 4) Uninstall

```bash
helm uninstall honeybeepf-llm -n <your-namespace>
helm uninstall honeybeepf-llm-otel-collector -n <your-namespace>
helm uninstall honeybeepf-llm-prometheus -n <your-namespace>
kubectl delete namespace <your-namespace>
```

## 6. First Scenario: Verify Probe Operation (under 10 minutes)

Once installed, the LLM probe and file access probe are already enabled via `demo-template.yaml`.

> **LLM Probe:** OpenAI, Anthropic, and Gemini are supported as built-in providers by default. No additional configuration is needed for these providers. If you use private or self-hosted LLMs (e.g., Ollama, vLLM), add them to the `providers` field in `demo-template.yaml`. See [`charts/honeybeepf-llm/values.yaml`](charts/honeybeepf-llm/values.yaml) for the full configuration example.

### Step 1: Check agent logs

```bash
kubectl logs -n <your-namespace> -l app.kubernetes.io/name=honeybeepf-llm --tail=50
```

You should see logs indicating the LLM probe and file access probe are active.

### Step 2: Check collected metrics

```bash
# Port-forward Prometheus
kubectl port-forward -n <your-namespace> svc/honeybeepf-llm-prometheus-server 9090:80 &

# Open http://localhost:9090 in your browser
```

If metrics appear in the Prometheus UI, data collection is working correctly.

---

# Development

## 7. Building

```bash
# Standard build (without Kubernetes support)
cargo build --release --package honeybeepf-llm

# With Kubernetes pod metadata support (namespace, pod name in metrics)
cargo build --release --features k8s --package honeybeepf-llm
```

> **Note:** The `k8s` feature is **not** enabled by default. When deploying to Kubernetes, always build with `--features k8s` to include pod metadata resolution. The Docker build (`Dockerfile`) already includes this flag.

## 8. How to Contribute
- **Issues:** Use GitHub Issues for bug reports or feature requests  
- **PRs:** Contributions must open PRs  
- **Guide:** Follow [`CONTRIBUTING.md`](CONTRIBUTING.md) for coding standards and
	review expectations  

---

# Project Info

## 9. Team

| Name   | ID | Role       | SNS | Responsibilities                 |
|--------|----|------------|-----|---------------------------------|
| Jundorok |    | Team Leader | TBU | Roadmap & Feature Development   |
| pmj-chosim |    | Core Dev   | TBU | CI/CD & Observability           |
| sammiee5311 |    | Core Dev   | TBU | Feature Development             |
| vanillaturtlechips |    | Core Dev   | TBU | CI/CD & Observability           |

## 10. Tech Stack
- **Languages:** eBPF, Kernel, Rust  
- **Infrastructure:** Kubernetes, Helm, OpenTelemetry, Prometheus, Grafana  
- **Communication:** Discord, GitHub Discussions  

## 11. Roadmap
- **Phase 1:** CI/CD and Observability Setup  
- **Phase 2:** Core Module Development  
- **Phase 3:** Monitoring and Testing  
- **Phase 4:** Release & Operator Integrations  

> We track roadmap execution via GitHub Projects and release multi-architecture
> container images using `publish.sh` once CI pipelines pass.

## 12. Resources & Links
- GitHub Repository: [github.com/honeybee-studio/honeybeepf-llm](https://github.com/honeybee-studio/honeybeepf-llm)
- Helm Charts: [`charts/honeybeepf-llm`](charts/honeybeepf-llm)
- Governance: [`GOVERNANCE.md`](GOVERNANCE.md)

## 13. Governance & Community
- **Code of Conduct:** See [`CODE_OF_CONDUCT.md`](CODE_OF_CONDUCT.md). Report
	incidents privately via [GitHub Issues](https://github.com/honeybee-studio/honeybeepf-llm/issues).
- **Decision Process:** Maintainers document proposals via Issues/Discussions
	with a 72-hour community review window before landing major changes.  
- **Meetings:** We host quarterly community syncs announced in GitHub
	Discussions. Notes are published alongside meeting issues.  
- **Membership:** Active contributors who review and merge work over two
	consecutive releases are invited to join the maintainer group.

## 14. Licensing
- **Source Code:** MIT License (`LICENSE`).  
- **Documentation:** MIT License unless otherwise noted within the document.  
- **Third-Party Assets:** Refer to each component's directory for licensing
	notices.

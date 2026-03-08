# HoneybeePF Pipeline Installation Guide

## Architecture Overview

```
┌─────────────────┐      gRPC(4317)     ┌──────────────────────┐    scrape(8889)    ┌─────────────────┐
│   honeybeepf    │ ─────────────────►  │  OTel Collector      │ ◄───────────────── │   Prometheus    │
│   (DaemonSet)   │                     │  (Deployment)        │                    │   (Server)      │
│                 │                     │                      │                    │                 │
│  eBPF metrics   │                     │  OTLP → Prometheus   │                    │  hbpf_* metrics │
│  collect & send │                     │  Exporter            │                    │  store & query  │
└─────────────────┘                     └──────────────────────┘                    └─────────────────┘
```

## Installation Modes

1.  **Kubernetes Mode (Recommended):** Full pipeline deployment using Helm.
2.  **Standalone Mode:** Run binary directly on Linux host (requires external or local OTel Collector).

---

## 1. Kubernetes Mode Installation (Helm)

### Prerequisites
* Kubernetes Cluster (v1.23+)
* Helm 3+ installed

### Step 1. Add Required Helm Repositories

```bash
helm repo add prometheus-community https://prometheus-community.github.io/helm-charts
helm repo add open-telemetry https://open-telemetry.github.io/opentelemetry-helm-charts
helm repo update
```

### Step 2. Update Helm Repository Dependencies

```bash
cd charts/honeybeepf-otel-collector
helm dependency update

cd ../honeybeepf-prometheus
helm dependency update
```

### Step 3. Create Namespace

```bash
kubectl create namespace monitoring
```

### Step 4. Sequential Installation

```bash
# 1. Install Prometheus (first, so it's ready to scrape)
helm install honeybeepf-prometheus ./charts/honeybeepf-prometheus \
  --namespace monitoring

# 2. Install OpenTelemetry Collector
helm install honeybeepf-otel-collector ./charts/honeybeepf-otel-collector \
  --namespace monitoring

# 3. Install honeybeepf DaemonSet
helm install honeybeepf ./charts/honeybeepf \
  --namespace monitoring
```

### Step 5. Verify Installation

```bash
# Check Pod status
kubectl get pods -n monitoring

# Check Services
kubectl get svc -n monitoring
```

---

## 2. Standalone Mode Installation (Binary)

For bare-metal servers, VMs, or local development without Kubernetes.

### Prerequisites
* Linux Kernel 5.x+ (BTF support required)
* Root privileges (`sudo`)
* Local or Remote OTel Collector (The agent needs an endpoint to push metrics)

### Step 1. Download & Run

```bash
# 1. Download latest binary

# 2. Set Environment Variables (Equivalent to values.yaml)

# 3. Run (Must be root)

```

### Step 2. How to check Metrics? (Standalone)

Since the agent pushes metrics via OTLP, you must have an OTel Collector running.

**Option A: Run a local Collector (Docker)**


**Option B: Use Remote Collector**


---

## Configuration Reference

### Port Configuration

| Component | Port | Protocol | Purpose |
|-----------|------|----------|---------|
| OTel Collector | 4317 | gRPC | OTLP metrics receiver |
| OTel Collector | 4318 | HTTP | OTLP HTTP receiver |
| OTel Collector | 8889 | HTTP | Prometheus exporter |

> **Note**: honeybeepf Agent does NOT expose metrics directly. All metrics flow through OTel Collector.

### Configuration Mapping Table

| Feature | Helm Value (`values.yaml`) | Environment Variable (Binary) |
| :--- | :--- | :--- |
| **Log Level** | `rustLog` | `RUST_LOG` |
| **OTLP Endpoint** | `output.otlp.endpoint` | `OTEL_EXPORTER_OTLP_ENDPOINT` |
| **Service Name** | (Internal template) | `OTEL_SERVICE_NAME` |
| **LLM Probe** | `builtinProbes.llm.enabled` | `BUILTIN_PROBES__LLM` |

---

## Troubleshooting

### 1. Targets Not Visible in Prometheus (K8s)

```bash
# Check Endpoints
kubectl get endpoints -n monitoring

# Check Prometheus targets page
kubectl port-forward svc/honeybeepf-prometheus-server -n monitoring 9090:80
# Open http://localhost:9090/targets in browser
```

### 2. Metrics Not Being Collected

```bash
# Check OTel Collector logs
kubectl logs -n monitoring -l app.kubernetes.io/name=opentelemetry-collector

# Check honeybeepf logs (verify OTLP endpoint output)
kubectl logs -n monitoring -l app.kubernetes.io/name=honeybeepf

# Check OTel Collector metrics directly
kubectl port-forward svc/honeybeepf-otel-collector-opentelemetry-collector -n monitoring 8889:8889
curl http://localhost:8889/metrics | grep honeybeepf_
```

---

## Metrics List

| Metric Name | Type | Description |
|-------------|------|-------------|
| `honeybeepf_active_probes` | Gauge | Number of currently active eBPF probes |
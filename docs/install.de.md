<!-- translated from docs/install.md at 250d516 -->

# Installation { #install }

!!! note ""
    Diese Seite ist eine Übersetzung des englischen Originals. Bei Abweichungen gilt das [englische Original](https://honeybee-studio.github.io/honeybeepf-llm/install/).

Der [Schnellstart](quickstart.md) installiert nur den Agenten und liest die Aufrufe aus seinem Log. Diese Seite
installiert ihn zusammen mit der Metrik-Pipeline: Der Agent sendet OpenTelemetry-Metriken an einen Collector, den
Prometheus ausliest, und die [Grafana-Dashboards](grafana/README.md) lesen aus Prometheus.

## Agent, Collector und Prometheus { #agent-collector-and-prometheus }

Erfordert einen Kubernetes-Cluster, auf dessen Nodes ein Linux-Kernel 5.10+ oder ein RHEL-Kernel 8.10 / 9.6 / 9.7
läuft, sowie `kubectl` und Helm 3. Diese Schritte installieren das neueste Release, v0.5.0, mit einem mitgelieferten Prometheus und
OpenTelemetry Collector. Die Charts werden am Release-Tag ausgecheckt, weil die Charts auf `main` einen neueren Agenten
erwarten können als das Release-Image.

```bash
git clone --branch v0.5.0 https://github.com/honeybee-studio/honeybeepf-llm.git
cd honeybeepf-llm
helm repo add prometheus-community https://prometheus-community.github.io/helm-charts
helm repo add open-telemetry https://open-telemetry.github.io/opentelemetry-helm-charts
helm dependency build charts/honeybeepf-llm-prometheus
helm dependency build charts/honeybeepf-llm-otel-collector

kubectl create namespace honeybeepf
helm install honeybeepf-llm-prometheus charts/honeybeepf-llm-prometheus -n honeybeepf
helm install honeybeepf-llm-otel-collector charts/honeybeepf-llm-otel-collector -n honeybeepf
helm install honeybeepf-llm charts/honeybeepf-llm -n honeybeepf \
  --set output.otlp.endpoint=honeybeepf-llm-otel-collector-opentelemetry-collector:4317
```

LLM-Aufrufe live verfolgen:

```bash
kubectl logs -n honeybeepf ds/honeybeepf-llm -f | grep 'LLM SUCCESS'
```

`ds/honeybeepf-llm` folgt einem einzigen Agent-Pod; in einem Cluster mit mehreren Nodes stehen die Aufrufe jedes Nodes
im Agent-Pod dieses Nodes.

```
2026-10-26T08:43:35.510264Z  INFO honeybeepf_llm::probes::builtin::llm::processor::background: LLM SUCCESS | PID: 1686847 | Model: claude-test | Transport: tls | Latency: 0.00s | Tokens: 140 (Prompt: 125, Compl: 15, Cache read: 100, Cache write: 0) | Pod: team-b/support-bot-589db898dd-rv9ts
```

Das mitgelieferte Prometheus ist der Service `honeybeepf-llm-prometheus-server` (Port 80) im Namespace `honeybeepf`;
die [Metrik-Referenz](metrics.md) enthält Beispielabfragen, und [Grafana-Dashboards](grafana/README.md) zeigt, wie das
Kosten-Dashboard geladen wird.

## Ein Collector außerhalb des Clusters { #a-collector-outside-the-cluster }

Um Metriken an einen Collector außerhalb des Clusters zu senden, verwenden Sie TLS und, falls der Collector es
verlangt, einen Auth-Header:

```bash
kubectl -n honeybeepf create secret generic otel-ca --from-file=ca.crt=./collector-ca.crt   # only for a private CA
kubectl -n honeybeepf create secret generic otel-auth --from-literal=headers='authorization=Bearer <token>'
helm upgrade --install honeybeepf-llm charts/honeybeepf-llm -n honeybeepf --reuse-values \
  --set output.otlp.endpoint=https://otel.example.com:4317 \
  --set output.otlp.caSecret=otel-ca \
  --set output.otlp.headersSecret.name=otel-auth --set output.otlp.headersSecret.key=headers
```

Ein `https://`-Endpunkt wird gegen die CA aus `caSecret` geprüft, sofern gesetzt, andernfalls gegen die
Root-Zertifikate des Systems; `host:port` und `http://` bleiben unverschlüsselt.

## Berechtigungen und Ressourcen { #permissions-and-resources }

Der Agent benötigt `hostPID` sowie die Capabilities `BPF`, `PERFMON`, `SYS_ADMIN`, `SYS_PTRACE`, `NET_ADMIN` und `SYS_RESOURCE` und fordert pro Node 50m CPU / 128Mi an (Limits 1 CPU / 1Gi); siehe
[`values.yaml`](https://github.com/honeybee-studio/honeybeepf-llm/blob/main/charts/honeybeepf-llm/values.yaml).

Zwei dieser Capabilities werden nur auf älteren Kerneln benötigt. Das Chart vergibt sie, damit eine einzige Installation
auf jedem Node funktioniert; wenn alle Ihre Nodes mit einem neueren Kernel laufen, können Sie sie entfernen:

| Capability | Benötigt auf einem Node mit | Grund | Entfernbar, wenn jeder Node diesen Kernel hat |
|---|---|---|---|
| `NET_ADMIN` | einem Kernel vor 6.9 | der Agent erstellt beim Start eine Socket-Map, auch wenn die Klartext-Erfassung aus ist, und diese Kernel verlangen dafür `NET_ADMIN` | 6.9 oder neuer |
| `SYS_RESOURCE` | einem Kernel vor 5.11 | diese Kernel rechnen den Speicher von BPF-Maps auf das memlock-Limit an, das der Agent beim Start anhebt | 5.11 oder neuer |

Wer die Liste setzt, ersetzt sie vollständig. Behalten Sie daher die anderen vier bei, zum Beispiel auf Nodes, die
alle mit 6.9 oder neuer laufen:

```yaml
securityContext:
  capabilities:
    add: [BPF, PERFMON, SYS_ADMIN, SYS_PTRACE]
```

Fehlt eine Capability, die der Kernel des Nodes benötigt, bricht der Agent beim Start mit
`Operation not permitted` ab, sobald er eine Map anlegt.

## Deinstallation { #uninstall }

```bash
helm uninstall honeybeepf-llm honeybeepf-llm-otel-collector honeybeepf-llm-prometheus -n honeybeepf
kubectl delete namespace honeybeepf
```

Vor der Installation eines Releases können Sie [seine Signaturen prüfen](verify.md). Wie Sie ein aus Ihrem eigenen
Checkout gebautes Image ausführen, beschreibt [Aus dem Quellcode bauen](build.md).

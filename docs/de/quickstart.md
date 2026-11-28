<!-- translated from docs/en/quickstart.md, git blob 39a92ae8a6a4 -->

# Schnellstart { #quickstart }

!!! note ""
    Diese Seite ist eine Übersetzung des englischen Originals. Bei Abweichungen gilt das [englische Original](https://honeybee-studio.github.io/honeybeepf-llm/quickstart/).

In wenigen Minuten sehen Sie die LLM-Aufrufe aller Pods in Ihrem Cluster. Dabei wird nur der Agent installiert, und
die Aufrufe lesen Sie aus seinem Log; [Installation](install.md) ergänzt Prometheus, einen OpenTelemetry Collector und das
Grafana-Kosten-Dashboard.

## Voraussetzungen { #before-you-start }

- Ein Kubernetes-Cluster, dessen Nodes mit Linux 5.10 oder neuer oder mit einem RHEL-Kernel 8.10, 9.6 oder 9.7 laufen.
- `kubectl` und Helm 3.
- Pods, die einen LLM-Anbieter über HTTPS aus Python, Node.js, Go, curl oder einem anderen Programm, das OpenSSL nutzt,
  aufrufen. Noch kein LLM-Traffic? Dann stattdessen die [Demo](demo.md) ausführen.

## 1. Agent installieren { #1-install-the-agent }

```bash
kubectl create namespace honeybeepf
helm install honeybeepf-llm oci://ghcr.io/honeybee-studio/charts/honeybeepf-llm -n honeybeepf
```

Das Chart startet als DaemonSet einen Agent-Pod pro Node. Ihre Apps, ihre SDKs und ihre API-Schlüssel bleiben, wie
sie sind.

## 2. Aufrufe verfolgen { #2-watch-the-calls }

```bash
kubectl logs -n honeybeepf ds/honeybeepf-llm -f | grep 'LLM SUCCESS'
```

Jeder LLM-Aufruf ist eine Zeile mit dem Modell, den Token-Zahlen und dem Pod, der ihn abgesetzt hat:

```
LLM SUCCESS | PID: 1686847 | Model: claude-test | Transport: tls | Latency: 0.00s | Tokens: 140 (Prompt: 125, Compl: 15, Cache read: 100, Cache write: 0) | Pod: team-b/support-bot-589db898dd-rv9ts
```

`ds/honeybeepf-llm` verfolgt nur einen einzigen Agent-Pod. In einem Cluster mit mehreren Nodes stehen die Aufrufe jedes Nodes
im Agent-Pod dieses Nodes: `kubectl logs -n honeybeepf -l app.kubernetes.io/name=honeybeepf-llm --prefix -f` verfolgt
sie alle.

Keine Zeilen? Siehe [Fehlerbehebung](troubleshooting.md).

## 3. Nächste Schritte { #3-next }

- [Metriken an Prometheus senden und das Kosten-Dashboard öffnen](install.md).
- [Prüfen, welche Ihrer Workloads erfasst werden](guides/coverage.md).
- [Nachlesen, wie Aufrufe bepreist werden](cost.md), und [eigene Preise hinzufügen](guides/prices.md).

Zum Entfernen:

```bash
helm uninstall honeybeepf-llm -n honeybeepf
kubectl delete namespace honeybeepf
```

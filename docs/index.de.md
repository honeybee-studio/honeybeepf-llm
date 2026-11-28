---
title: Ihre LLM-Rechnung, aufgeteilt nach Team
description: Ihre LLM-Rechnung, aufgeteilt nach Team. Token und geschätzte Kosten pro Kubernetes-Namespace, Pod und Modell, gelesen mit eBPF. Keine Codeänderungen, kein Proxy, kein SDK.
hide:
  - navigation
  - toc
---

<!-- translated from docs/index.md at 250d516 -->

<div class="hb-hero" markdown>

<div class="hb-hero__text" markdown>

<h1>Ihre LLM-Rechnung, <em>aufgeteilt nach Team.</em></h1>

<p class="hb-sub">Ihr Anbieter schickt eine einzige Gesamtsumme. honeybeepf-llm zeigt, welcher Namespace, welcher Pod und
welches Modell sie verursacht hat, ohne Ihre Apps anzufassen.</p>

[Schnellstart :material-arrow-right:](quickstart.md){ .md-button .md-button--primary }
[GitHub :fontawesome-brands-github:](https://github.com/honeybee-studio/honeybeepf-llm){ .md-button }

<p class="hb-meta">Open Source, Apache-2.0 · eBPF-DaemonSet für Kubernetes · OpenTelemetry-Metriken</p>

</div>

<div class="hb-receipt-wrap">
<div class="hb-receipt" data-hb-live role="img" aria-label="Ein Kassenbon, der die LLM-Aufrufe im Demo-Cluster live ausdruckt, mit einer Zwischensumme pro Team: team-b $1.96, team-a $0.85, team-c $0.17, gesamt $2.98">
  <div class="hb-receipt__head">
    <strong>honeybeepf-llm</strong>
    <span>Cluster demo · Node worker-1</span>
    <span class="hb-receipt__live"><i></i>druckt live</span>
  </div>
  <ol class="hb-receipt__items" data-hb-log>
    <li><b>team-b/support-bot</b><span>claude-sonnet-4-5 × 1,840 tok</span><em>$0.011</em></li>
    <li><b>team-a/chat-api</b><span>gpt-5.4-mini × 2,310 tok</span><em>$0.002</em></li>
    <li><b>team-c/summarizer</b><span>gemini-2.5-flash × 3,020 tok</span><em>$0.001</em></li>
    <li><b>team-b/support-bot</b><span>claude-opus-5-5 × 960 tok</span><em>$0.019</em></li>
  </ol>
  <div class="hb-receipt__subs">
    <div data-hb-team="team-b" data-hb-usd="1.96"><span>team-b</span><i></i><span class="hb-amount">$1.96</span></div>
    <div data-hb-team="team-a" data-hb-usd="0.85"><span>team-a</span><i></i><span class="hb-amount">$0.85</span></div>
    <div data-hb-team="team-c" data-hb-usd="0.17"><span>team-c</span><i></i><span class="hb-amount">$0.17</span></div>
  </div>
  <div class="hb-receipt__total"><span>Gesamt</span><span data-hb-total>$2.98</span></div>
  <div class="hb-receipt__foot">Geänderter Anwendungscode: 0 Zeilen<br>Simulierte Aufrufe auf den Demo-Workloads</div>
</div>
</div>

</div>

## Der Weg dorthin { #how-you-get-it }

<div class="hb-steps" markdown>

<div class="hb-step" markdown>
<div class="hb-step__art hb-step__art--code">$ helm install honeybeepf-llm \
    oci://ghcr.io/honeybee-studio/\
    charts/honeybeepf-llm

<span>DaemonSet bereit: 1 Agent pro Node</span></div>
<div class="hb-step__n">1</div>
<h3>Ein DaemonSet installieren</h3>

Ein Agent pro Node. Ihre Apps, SDKs und API-Schlüssel bleiben, wie sie sind.
</div>

<div class="hb-step" markdown>
<div class="hb-step__art hb-step__art--log"><span>pod</span>    team-b/support-bot
<span>host</span>   api.anthropic.com
<span>model</span>  claude-sonnet-4-5
<span>tokens</span> 1,840  <b>$0.011</b></div>
<div class="hb-step__n">2</div>
<h3>Jeder LLM-Aufruf wird gelesen</h3>

Jeder Aufruf wird im Kernel genau an der Stelle gelesen, an der Ihre App ihn an TLS übergibt, und seinem Pod zugeordnet.
</div>

<div class="hb-step" markdown>
<div class="hb-step__art hb-step__art--shot"><img src="images/grafana-cost-narrow.png" alt="Grafana-Panel: geschätzte Ausgaben pro Workload, team-b support-bot $1.96, team-a chat-api $0.85, team-c summarizer $0.17"></div>
<div class="hb-step__n">3</div>
<h3>Kosten pro Team sehen</h3>

Token und Dollar pro Namespace, Pod und Modell in Prometheus, samt [Grafana-Dashboard](grafana/README.md).
</div>

</div>

## Ausprobieren { #try-it }

=== "Helm"

    Erfordert einen Kubernetes-Cluster mit Nodes unter Linux 5.10+, `kubectl` und Helm 3.

    ```bash
    kubectl create namespace honeybeepf
    helm install honeybeepf-llm oci://ghcr.io/honeybee-studio/charts/honeybeepf-llm -n honeybeepf
    kubectl logs -n honeybeepf ds/honeybeepf-llm -f | grep 'LLM SUCCESS'
    ```

    Für das Grafana-Kosten-Dashboard zusätzlich das mitgelieferte Prometheus und den OpenTelemetry Collector
    installieren: siehe [Installation](install.md).

=== "Demo, ohne API-Schlüssel"

    Startet einen Mock-Anbieter, LLM-Clients in drei Team-Namespaces und Grafana in einem beliebigen Cluster (kind, k3d,
    minikube).

    ```bash
    git clone https://github.com/honeybee-studio/honeybeepf-llm.git && cd honeybeepf-llm
    examples/demo/demo.sh up
    kubectl -n monitoring port-forward svc/grafana 3000:80
    ```

    Danach <http://localhost:3000> und dort das Dashboard „LLM Cost Observability“ öffnen.

## Warum kein Gateway und kein SDK? { #why-not-a-gateway-or-an-sdk }

<div class="hb-compare" markdown>

| | Gateway / Proxy | SDK-Instrumentierung | honeybeepf-llm (eBPF) |
|---|---|---|---|
| Änderung an der App | neue Basis-URL | Code hinzufügen, neu deployen | keine |
| Im Anfragepfad | ja | im Prozess | nein: liest Kopien der Puffer |
| Erfasste Aufrufe | nur die darüber gerouteten | nur instrumentierter Code | jeder Pod auf dem Node |
| API-Schlüssel | laufen hindurch | im App-Prozess | nie geloggt (nur Pfad, Modell, Token) |

</div>

## Funktionsweise { #how-it-works }

![uprobes auf SSL_write/SSL_read kopieren den Klartext in einen Ringpuffer; der Agent parst HTTP, ordnet den Anbieter zu, bildet die cgroup auf einen Pod ab und exportiert OTLP-Metriken](images/how-it-works.svg)

1. Der Agent hängt uprobes an `SSL_write` und `SSL_read` in jeder `libssl`, die ein Prozess lädt, und an `crypto/tls`
   in Go-Programmen.
2. Die uprobes kopieren den Klartext-Puffer, die PID und die cgroup-ID in einen BPF-Ringpuffer.
3. Im User Space setzt der Parser für HTTP/1.1, HTTP/2 und SSE jeden Aufruf wieder zusammen, bestimmt den Anbieter
   anhand von Host und Pfad und liest die Usage-Felder aus der Antwort.
4. Über die cgroup wird der Prozess seinem Pod zugeordnet, und das Ergebnis wird als OpenTelemetry-Metriken exportiert.

## Weiterlesen { #read-next }

<div class="hb-index" markdown>

- [Schnellstart](quickstart.md): den Agenten installieren und die Aufrufe in wenigen Minuten verfolgen.
- [Funktionsweise](how-it-works.md): vom TLS-Puffer zu Dollar pro Pod, Schritt für Schritt.
- [Was unterstützt wird](providers.md): Anbieter, Laufzeitumgebungen, Protokolle und Kernel.
- [Nicht erfasste Workloads finden](guides/coverage.md): eine einzige Abfrage listet sie auf.
- [Einen Anbieter hinzufügen](guides/custom-providers.md) oder [eigene Preise](guides/prices.md).
- [Metriken](metrics.md): jede Metrik und jedes Label, und warum ein Aufruf nicht erfasst wurde.
- [Budgets und Alerts](alerts.md): Regeln für Ausgaben und Datenqualität, die mit dem Chart ausgeliefert werden.
- [Fehlerbehebung](troubleshooting.md) und [FAQ](faq.md).
- [Sicherheitsprüfung](security-review.md): Kernel-Hooks, Berechtigungen, Datenverarbeitung und bekannte Lücken.
- [Overhead-Benchmark](benchmark.md): CPU, Speicher und Durchsatz mit und ohne Agent.

</div>

!!! note ""
    Diese Seite ist eine Übersetzung des englischen Originals. Bei Abweichungen gilt das [englische Original](https://honeybee-studio.github.io/honeybeepf-llm/).

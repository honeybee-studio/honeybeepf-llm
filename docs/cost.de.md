<!-- translated from docs/cost.md at 250d516 -->

# Kosten und Preise { #cost-and-prices }

!!! note ""
    Diese Seite ist eine Übersetzung des englischen Originals. Bei Abweichungen gilt das [englische Original](https://honeybee-studio.github.io/honeybeepf-llm/cost/).

Für jeden erfassten Aufruf werden die Kosten in USD geschätzt: seine Token-Zahlen mal die Preise des Modells.

## Woher die Preise kommen { #where-prices-come-from }

- Eine datierte **eingebaute Preistabelle**, zugeordnet über den Modellnamen in der Antwort. Ein datierter Name wie
  `claude-sonnet-4-5@20250929` wird seinem Modell zugeordnet.
- **[Eigene Preise](guides/prices.md)** darübergelegt: Modelle, die in der Tabelle fehlen, oder ein vertraglicher Rabatt.
- Ein Modell ohne Preis wird **nie als $0 erfasst**: Seine Token landen stattdessen in `llm_unpriced_tokens_total`.

**Wie ein Preis gewählt wird**

| | |
|---|---|
| Nach | dem Modellnamen, nicht dem Host: Azure OpenAI, Bedrock, Vertex AI und regionale Endpunkte werden wie die eigene API des Anbieters bepreist, daher wird ein regionaler Aufschlag wie bei OpenAI Data Residency (+10 %) nicht aufgeschlagen |
| Preise | die veröffentlichten Listenpreise der Anbieter; jeder Anbietereintrag nennt seine Preisseite und das Datum der Prüfung |
| Nicht enthalten | Tool-Gebühren pro Anfrage, etwa für die Websuche |

Bevor Sie einer Summe vertrauen, vergleichen Sie die Kosten mit `llm_unpriced_tokens_total`.

!!! note
    Kostenmetriken erscheinen nur, wenn eine Preistabelle geladen wurde. Eine abgelehnte eingebaute Tabelle erzeugt nur
    eine Warnung im Log, und der Agent läuft dann ohne sie.

## Ausgaben pro Workload { #spend-per-workload }

```promql
sum by (target_namespace, target_workload_name) (rate(honeybeepf_llm_llm_cost_usd_total[5m])) * 60
```

USD pro Minute, nach Namespace und Workload.

## Aufrufe über ein Gateway { #calls-through-a-gateway }

Ein Aufruf, der über ein Gateway im Cluster läuft (LiteLLM, ein Firmen-Proxy), wird zweimal ausgeführt: von der App zum
Gateway und vom Gateway zum Anbieter. Wenn der Agent beide erfasst, tragen Sie das Gateway als
[Anbieter](guides/custom-providers.md) mit `gateway: true` ein, damit sich die beiden Datensätze unterscheiden lassen:

| Datensatz | Labels | Verwendung |
|---|---|---|
| App → Gateway | der Pod der App, `hop="gateway"` | Ausgaben pro Team oder Workload |
| Gateway → Anbieter | der Pod des Gateways, kein `hop` | Gesamtausgaben: Modell und Nutzung laut Anbieter |

Die Zeitreihen eines Workloads enthalten nur dessen eigene Aufrufe. Für Ausgaben pro Namespace oder Workload ist daher
kein Filter nötig, und das Gateway erscheint als ein weiterer Workload. Eine Summe über alle Workloads ohne den Gateway-Hop zählt jeden
Aufruf genau einmal:

```promql
sum(increase(honeybeepf_llm_llm_cost_usd_total{hop!="gateway"}[1d]))
```

Das mitgelieferte Kosten-Dashboard wendet das in seinen Gesamtsummen und in seinen Panels pro Modell, pro Modalität und
pro Operation an.

!!! warning
    Setzen Sie `gateway: true` nur für ein Gateway, dessen Aufrufe an die Anbieter dieser Agent erfasst: eines, das auf den
    Nodes des Clusters läuft und die Anbieter über TLS oder einen `plaintext`-Eintrag aufruft. Bei einem Gateway
    außerhalb des Clusters ist sein Datensatz der einzige, und der obige Filter würde ihn verwerfen.

## Die Kostenmetriken { #the-cost-metrics }

| Metrik | Was sie zählt | Zusätzliche Labels |
|---|---|---|
| `honeybeepf_llm_llm_cost_usd_total` | die Kosten jedes Aufrufs in USD | `cost_type`, `price_table_version` |
| `honeybeepf_llm_llm_cache_savings_ceiling_usd_total` | wie viel Prompt-Caching höchstens hätte einsparen können | `cache_price`, `price_table_version` |
| `honeybeepf_llm_llm_unpriced_tokens_total` | Token von Aufrufen, die nicht bepreist werden konnten | `reason`, `price_table_version` |

Alle drei tragen die Labels pro Aufruf (`model`, `status`, `attempt`, `target_namespace`, `target_pod_name`,
`target_workload_kind`, `target_workload_name`), aber nicht `transport` oder `operation`. Die USD-Zähler haben keine
Einheit, daher hängt der Exporter keine an den Namen an.

### `cost_type`: wie die Kosten aufgeteilt werden { #cost_type-how-the-cost-is-split }

Wie auf der Rechnung eines Anbieters. Anteile mit dem Wert null werden nicht addiert, ein Aufruf hat also nur die Zeitreihen,
die er tatsächlich genutzt hat.

| `cost_type` | Abgerechnet für |
|---|---|
| `input` | Prompt-Token, die weder aus dem Cache gelesen noch in ihn geschrieben wurden |
| `cache_read` | Prompt-Token, die aus dem Cache des Anbieters bedient wurden |
| `cache_write` | Prompt-Token, die in den Cache des Anbieters geschrieben wurden |
| `output` | alle Completion-Token, einschließlich Reasoning-Token |

### `attempt`: bedient oder abgelehnt { #attempt-served-or-declined }

| `attempt` | Der Aufruf |
|---|---|
| `served` | der Anbieter hat ihn bedient |
| `declined` | ein früherer serverseitiger Fallback-Versuch, den der Anbieter zwar abgerechnet, dann aber umgeleitet hat: nur Token und Kosten, keine Anfragezahl und keine Latenz |

Für Aufrufe, die als Fehler erfasst werden, werden keine Kosten verbucht.

### `price_table_version` { #price_table_version }

Die `version` der eingebauten Tabelle, mit angehängtem `+custom-<hash>`, wenn Ihre Preise darübergelegt werden. Eine
Preisänderung erscheint als neue Zeitreihe.

### Obergrenze der Cache-Einsparung { #cache-savings-ceiling }

Die nicht gecachten Input-Token des Aufrufs, bepreist mit dem Cache-Read-Preis des Modells statt mit seinem Input-Preis:
eine Obergrenze dafür, was Prompt-Caching einsparen könnte. Wird nur für bepreiste Aufrufe ausgegeben, die keine
Embeddings sind, und nur, wenn der Wert über null liegt.

| `cache_price` | Bedeutung |
|---|---|
| `known` | das Modell hat einen Cache-Read-Preis |
| `assumed_free` | es hat keinen, daher entspricht die Grenze den vollen Input-Kosten |

### Nicht bepreiste Token { #unpriced-tokens }

| `reason` | Der Aufruf konnte nicht bepreist werden, weil |
|---|---|
| `unknown_model` | der Modellname fehlt oder nicht in der Tabelle steht |
| `missing_price` | ein Preis, den der Aufruf benötigt, fehlt oder abgelaufen ist |
| `unsupported_modifier` | es für das Service-Tier, die Geschwindigkeit oder den Geo-Modifikator des Aufrufs keinen Preis gibt |
| `inconsistent_usage` | die gemeldeten Cache-Token-Zahlen die Prompt-Summe übersteigen, zu der sie gehören |

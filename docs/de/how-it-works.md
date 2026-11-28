<!-- translated from docs/en/how-it-works.md, git blob 4df014123bcb -->

# Funktionsweise { #how-it-works }

!!! note ""
    Diese Seite ist eine Übersetzung des englischen Originals. Bei Abweichungen gilt das [englische Original](https://honeybee-studio.github.io/honeybeepf-llm/how-it-works/).

Eine App, die einen LLM-Anbieter aufruft, übergibt die Anfrage im Klartext an ihre TLS-Bibliothek und erhält die Antwort
von ihr im Klartext zurück. honeybeepf-llm liest genau an dieser Stelle mit eBPF eine Kopie dieser Bytes, direkt auf dem Node.
Die App, ihr SDK und der Netzwerkpfad zum Anbieter bleiben unverändert.

![uprobes auf SSL_write/SSL_read kopieren den Klartext in einen Ringpuffer; der Agent parst HTTP, ordnet den Anbieter zu, bildet die cgroup auf einen Pod ab und exportiert OTLP-Metriken](../images/how-it-works.svg)

## 1. TLS-Code finden { #1-find-the-tls-code }

Der Agent läuft als DaemonSet, ein Pod pro Node. Er untersucht jeden Prozess auf seinem Node und ermittelt, wo der
Prozess TLS ausführt:

| Der Prozess | Der Agent hängt sich an |
|---|---|
| lädt eine gemeinsam genutzte `libssl.so` (Python, Node.js aus Distributionspaketen, curl) | `SSL_read`, `SSL_write` und deren `_ex`-Varianten in dieser Bibliothek |
| hat OpenSSL in seine ausführbare Datei einkompiliert (die offiziellen Node.js-Binaries) | dieselben Funktionen in der ausführbaren Datei |
| ist ein Go-Binary ab Go 1.20 | `crypto/tls` `(*Conn).Write`, `Read` und jeden Rücksprung aus `Read`, gefunden über die Go-Funktionstabelle, auch bei gestrippten Binaries |

Er hängt sich auch an Prozesse, die später starten: Eine Probe auf `exec` und auf das Öffnen einer `libssl`-Datei meldet
ihm neue Prozesse. Programme mit einem anderen TLS-Stack, etwa rustls oder Javas JSSE, werden noch nicht erfasst; die
[Abdeckungsmetrik](guides/coverage.md) listet sie auf.

## 2. Klartext kopieren { #2-copy-the-plaintext }

Jede Probe ist ein eBPF-Programm, das läuft, wenn die Funktion aufgerufen wird oder zurückkehrt. Es kopiert den
Klartext-Puffer (bis zu 16 KB pro Lese- und 4 KB pro Schreibvorgang), die Prozess-ID und die cgroup-ID in einen Ringpuffer,
den es mit dem Agenten teilt. Die Programme lesen nur: Keines von ihnen verändert den Aufruf, die Daten oder die
Verbindung.

Ein Modellserver im Cluster, der über unverschlüsseltes HTTP aufgerufen wird, etwa Ollama oder vLLM, hat keine TLS-Bibliothek,
an die sich der Agent hängen kann. Für einen mit `plaintext` markierten Anbieter kopiert der Agent die Bytes stattdessen
am Socket (siehe [Selbst gehostete Modelle über unverschlüsseltes HTTP](guides/plaintext.md)).

## 3. Bytes zu Aufrufen zusammensetzen { #3-turn-bytes-into-calls }

Im User Space setzt der Agent die Bytes jeder Verbindung wieder zu HTTP/1.1- oder HTTP/2-Anfragen und -Antworten oder zu
WebSocket-Nachrichten (Realtime, Responses, Gemini Live) zusammen,
dekomprimiert sie (`gzip`, `br`, `zstd`, `deflate`) und folgt Server-Sent-Event-Streams bis zu ihrem Ende. Er bestimmt
den Anbieter anhand von Host und Pfad der Anfrage und liest die Token-Zahlen aus den Usage-Feldern der Antwort: Prompt-,
Completion-, Cache-Read- und Cache-Write- sowie Reasoning-Token.

Ein Aufruf, den der Agent gesehen, aber nicht lesen konnte, wird nicht stillschweigend verworfen: Er wird mit dem Grund
in `llm_responses_unparsed_total` gezählt.

## 4. Pod finden { #4-find-the-pod }

Die cgroup des Prozesses liefert seine Container-ID. Ein Watch auf die Pods dieses Nodes ordnet den Container seinem
Namespace, seinem Pod und dem übergeordneten Workload zu, etwa einem Deployment oder einem CronJob.

## 5. Aufruf bepreisen { #5-price-the-call }

Der Modellname aus der Antwort wird in einer datierten, eingebauten Preistabelle nachgeschlagen; Ihre eigenen Preise
oder Rabatte werden darübergelegt. Die Kosten des Aufrufs werden so aufgeteilt, wie die Anbieter abrechnen: Input, Cache-Read,
Cache-Write und Output. Ein Modell ohne Preis wird in `llm_unpriced_tokens_total` gezählt und nie als $0 erfasst.

## 6. Exportieren { #6-export }

Der Agent exportiert Token, Anfragen, Latenz und Kosten als OpenTelemetry-Metriken über OTLP, mit Labels für Namespace,
Pod, Workload und Modell, und loggt pro Aufruf eine Zeile mit Modell, Pod, Latenz und Token-Zahlen (auf Level `debug` zusätzlich den Anfragepfad). Die
erfassten Bytes bleiben im Speicher des Agenten und werden nach dem Parsen verworfen: Prompts, Antworten, Header und
API-Schlüssel werden nie exportiert oder geloggt.

Die [Sicherheitsprüfung](security-review.md) führt jedes eBPF-Programm auf, die Berechtigungen, die der Agent
benötigt, und was er sehen kann und was nicht.

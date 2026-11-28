<!-- translated from docs/en/faq.md, git blob 66c7c3dff217 -->

# FAQ { #faq }

!!! note ""
    Diese Seite ist eine Übersetzung des englischen Originals. Bei Abweichungen gilt das [englische Original](https://honeybee-studio.github.io/honeybeepf-llm/faq/).

## Muss ich meine Apps ändern? { #do-i-have-to-change-my-apps }

Nein. Apps rufen den Anbieter weiterhin direkt mit ihren eigenen SDKs und Schlüsseln auf. Sie müssen keine Basis-URL
ändern, keine Bibliothek importieren und nichts neu deployen. Siehe [Funktionsweise](how-it-works.md).

## Liegt es im Anfragepfad oder bremst es meine Aufrufe aus? { #does-it-sit-in-the-request-path-or-slow-my-calls-down }

Nein. Der Agent liest Kopien der TLS-Puffer auf dem Node; Anfragen gehen genau wie vorher an den Anbieter, und nichts
wartet auf den Agenten. Seinen CPU- und Speicherverbrauch sowie die Auswirkung auf den Durchsatz unter Last misst
der [Overhead-Benchmark](benchmark.md).

## Liest es meine Prompts und API-Schlüssel? { #does-it-read-my-prompts-and-api-keys }

Die Probes kopieren die entschlüsselten Bytes jedes Aufrufs in den Speicher des Agenten auf dem Node, wie es jedes
Werkzeug tun muss, das TLS mitliest. Der Agent liest daraus die Usage-Felder aus und verwirft sie dann: Prompts, Antworten,
Header und API-Schlüssel werden nie exportiert oder geloggt. Die [Sicherheitsprüfung](security-review.md#3-data-handling)
führt Feld für Feld auf, was exportiert und geloggt wird.

## Wie genau sind die Zahlen? { #how-accurate-are-the-numbers }

Die Token-Zahlen stammen vom Anbieter selbst: Der Agent liest die Usage-Felder jeder Antwort, einschließlich Cache- und
Reasoning-Token. Die Kosten sind eine Schätzung: diese Token multipliziert mit einer datierten Preistabelle, die sich mit
Ihren Vertragspreisen überlagern lässt ([Kosten und Preise](cost.md)). Aufrufe, die der Agent gesehen, aber nicht lesen
konnte, werden mit Grund gezählt statt stillschweigend verworfen.

## Welche Anbieter, Sprachen und Protokolle werden abgedeckt? { #which-providers-languages-and-protocols-does-it-cover }

OpenAI, Anthropic, Gemini, Bedrock, Azure OpenAI, Vertex AI, Meta und viele OpenAI-kompatible APIs, aus Python,
Node.js, Go, curl und allem anderen, das OpenSSL verwendet, über HTTP/1.1 und HTTP/2 mit Streaming sowie OpenAI Realtime,
Responses und Gemini Live über WebSocket. Noch nicht: rustls, Java, andere WebSocket-APIs und HTTP/3. Die vollständige
Matrix steht unter [Was unterstützt wird](providers.md), und
[eine Metrik](guides/coverage.md) listet die Workloads in Ihrem Cluster auf, die nicht erfasst werden.

## Wir betreiben bereits ein LLM-Gateway wie LiteLLM. Brauchen wir das hier? { #we-already-run-an-llm-gateway-such-as-litellm-do-we-need-this }

Ein Gateway sieht die Aufrufe, die darüber geroutet werden; der Agent sieht jeden Aufruf, der vom Node ausgeht, auch die, die am
Gateway vorbeigehen, und nennt für jeden den Pod, der ihn abgesetzt hat, ohne Änderung an den Apps. Wenn Sie die
[Erkennung anhand der Aufrufstruktur](guides/detect-by-shape.md) aktivieren, wird ein Gateway im Cluster doppelt erfasst (App zum
Gateway, Gateway zum Anbieter); die Seite zeigt, wie Sie es nur einmal zählen.

## Was wird auf den Nodes benötigt? { #what-does-it-need-on-the-nodes }

Linux 5.10 oder neuer oder ein RHEL-Kernel 8.10, 9.6 oder 9.7, mit BTF, auf x86-64 oder arm64. Der Agent läuft nicht
privilegiert: Er läuft mit `hostPID` und einer kurzen Liste von Capabilities, erklärt unter
[Installation](install.md#permissions-and-resources) und in der [Sicherheitsprüfung](security-review.md#2-privileges).

## Funktioniert es ohne Kubernetes? { #does-it-work-without-kubernetes }

Ja, als Binary auf einem Linux-Host, gebaut ohne das Feature `k8s`; die Metriken tragen dann keine Labels für Namespace,
Pod oder Workload. Siehe [Ohne Kubernetes ausführen](run-binary.md).

## Wo melde ich einen Bug oder wünsche mir einen Anbieter? { #where-do-i-report-a-bug-or-ask-for-a-provider }

In den [GitHub Issues](https://github.com/honeybee-studio/honeybeepf-llm/issues).

<!-- translated from docs/en/quickstart.md, git blob 39a92ae8a6a4 -->

# Inicio rápido { #quickstart }

!!! note ""
    Esta página es una traducción del original en inglés. Si difieren, el [original en inglés](https://honeybee-studio.github.io/honeybeepf-llm/quickstart/) es el correcto.

Vea en pocos minutos las llamadas al LLM de cada Pod de su clúster. Esta guía instala solo el agente y lee las llamadas en
su log; [Instalación](install.md) añade Prometheus, un OpenTelemetry Collector y el dashboard de costos de Grafana.

## Antes de empezar { #before-you-start }

- Un clúster de Kubernetes cuyos nodos ejecuten Linux 5.10 o posterior, o un kernel de RHEL 8.10, 9.6 o 9.7.
- `kubectl` y Helm 3.
- Pods que llamen a un proveedor de LLM por HTTPS desde Python, Node.js, Go, curl o cualquier otro programa que use OpenSSL. ¿Aún no
  tiene tráfico de LLM? Ejecute la [demo](demo.md) en su lugar.

## 1. Instalar el agente { #1-install-the-agent }

```bash
kubectl create namespace honeybeepf
helm install honeybeepf-llm oci://ghcr.io/honeybee-studio/charts/honeybeepf-llm -n honeybeepf
```

El chart ejecuta un Pod del agente por nodo como DaemonSet. Sus aplicaciones, sus SDK y sus claves de API siguen igual.

## 2. Ver las llamadas { #2-watch-the-calls }

```bash
kubectl logs -n honeybeepf ds/honeybeepf-llm -f | grep 'LLM SUCCESS'
```

Cada llamada al LLM ocupa una línea, con el modelo, los recuentos de tokens y el Pod que la hizo:

```
LLM SUCCESS | PID: 1686847 | Model: claude-test | Transport: tls | Latency: 0.00s | Tokens: 140 (Prompt: 125, Compl: 15, Cache read: 100, Cache write: 0) | Pod: team-b/support-bot-589db898dd-rv9ts
```

`ds/honeybeepf-llm` sigue un único Pod del agente. En un clúster con varios nodos, las llamadas de cada nodo están en el Pod
del agente de ese nodo: `kubectl logs -n honeybeepf -l app.kubernetes.io/name=honeybeepf-llm --prefix -f` los sigue a todos.

¿No aparece ninguna línea? Consulte [Solución de problemas](troubleshooting.md).

## 3. Siguientes pasos { #3-next }

- [Enviar las métricas a Prometheus y abrir el dashboard de costos](install.md).
- [Comprobar cuáles de sus cargas de trabajo se ven](guides/coverage.md).
- [Ver cómo se calcula el precio de las llamadas](cost.md) y [añadir sus propios precios](guides/prices.md).

Para desinstalarlo:

```bash
helm uninstall honeybeepf-llm -n honeybeepf
kubectl delete namespace honeybeepf
```

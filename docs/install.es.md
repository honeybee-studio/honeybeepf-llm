<!-- translated from docs/install.md at 250d516 -->

# Instalación { #install }

!!! note ""
    Esta página es una traducción del original en inglés. Si difieren, el [original en inglés](https://honeybee-studio.github.io/honeybeepf-llm/install/) es el correcto.

El [Inicio rápido](quickstart.md) instala solo el agente y lee las llamadas de su log. Esta página lo instala con el
pipeline de métricas: el agente envía métricas de OpenTelemetry a un Collector, del que Prometheus las recoge, y los
[dashboards de Grafana](grafana/README.md) leen de Prometheus.

## Agente, Collector y Prometheus { #agent-collector-and-prometheus }

Requiere un clúster de Kubernetes cuyos nodos ejecuten un kernel Linux 5.10+ o RHEL 8.10 / 9.6 / 9.7, `kubectl` y Helm 3.
Estos pasos instalan la última versión, v0.5.0, con un Prometheus y un OpenTelemetry Collector incluidos. Los charts se
clonan en la etiqueta de la versión porque los de `main` pueden requerir un agente más reciente que la imagen publicada.

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

Para ver las llamadas al LLM a medida que ocurren:

```bash
kubectl logs -n honeybeepf ds/honeybeepf-llm -f | grep 'LLM SUCCESS'
```

`ds/honeybeepf-llm` sigue un único Pod del agente; en un clúster con varios nodos, las llamadas de cada nodo están en el Pod
del agente de ese nodo.

```
2026-10-26T08:43:35.510264Z  INFO honeybeepf_llm::probes::builtin::llm::processor::background: LLM SUCCESS | PID: 1686847 | Model: claude-test | Transport: tls | Latency: 0.00s | Tokens: 140 (Prompt: 125, Compl: 15, Cache read: 100, Cache write: 0) | Pod: team-b/support-bot-589db898dd-rv9ts
```

El Prometheus incluido es el Service `honeybeepf-llm-prometheus-server` (puerto 80) en el espacio de nombres `honeybeepf`; la
[referencia de métricas](metrics.md) tiene consultas de ejemplo, y [Dashboards de Grafana](grafana/README.md) explica cómo cargar el
dashboard de costos.

## Un collector fuera del clúster { #a-collector-outside-the-cluster }

Para enviar métricas a un collector fuera del clúster, use TLS y, si el collector lo requiere, un encabezado de autenticación:

```bash
kubectl -n honeybeepf create secret generic otel-ca --from-file=ca.crt=./collector-ca.crt   # only for a private CA
kubectl -n honeybeepf create secret generic otel-auth --from-literal=headers='authorization=Bearer <token>'
helm upgrade --install honeybeepf-llm charts/honeybeepf-llm -n honeybeepf --reuse-values \
  --set output.otlp.endpoint=https://otel.example.com:4317 \
  --set output.otlp.caSecret=otel-ca \
  --set output.otlp.headersSecret.name=otel-auth --set output.otlp.headersSecret.key=headers
```

Un endpoint `https://` se verifica con la CA de `caSecret` si está definida y, si no, con los certificados raíz del sistema; `host:port` y
`http://` no se cifran.

## Permisos y recursos { #permissions-and-resources }

El agente necesita `hostPID` y las capacidades `BPF`, `PERFMON`, `SYS_ADMIN`, `SYS_PTRACE`, `NET_ADMIN` y `SYS_RESOURCE`, y solicita 50m de CPU / 128Mi por nodo (límites de 1 CPU / 1Gi); consulte
[`values.yaml`](https://github.com/honeybee-studio/honeybeepf-llm/blob/main/charts/honeybeepf-llm/values.yaml).

Dos de esas capacidades solo hacen falta en kernels antiguos. El chart las concede para que una misma instalación funcione en todos los nodos;
si todos sus nodos ejecutan un kernel más reciente, puede quitarlas:

| Capacidad | Necesaria cuando un nodo ejecuta | Motivo | Se puede quitar cuando todos los nodos ejecutan |
|---|---|---|---|
| `NET_ADMIN` | un kernel anterior a 6.9 | el agente crea un mapa de sockets al iniciarse, incluso con la captura de texto plano desactivada, y esos kernels requieren `NET_ADMIN` para ello | 6.9 o posterior |
| `SYS_RESOURCE` | un kernel anterior a 5.11 | esos kernels computan la memoria de los mapas BPF dentro del límite de memlock, que el agente eleva al iniciarse | 5.11 o posterior |

Al definir la lista se reemplaza la original, así que conserve las otras cuatro. Por ejemplo, si todos los nodos ejecutan 6.9 o posterior:

```yaml
securityContext:
  capabilities:
    add: [BPF, PERFMON, SYS_ADMIN, SYS_PTRACE]
```

Si falta una capacidad que necesita el kernel del nodo, el agente se detiene al iniciarse con `Operation not permitted` cuando
crea un mapa.

## Desinstalación { #uninstall }

```bash
helm uninstall honeybeepf-llm honeybeepf-llm-otel-collector honeybeepf-llm-prometheus -n honeybeepf
kubectl delete namespace honeybeepf
```

Antes de instalar una versión puede [verificar sus firmas](verify.md); para ejecutar una imagen compilada a partir de su propia copia del repositorio,
consulte [Compilar desde el código fuente](build.md).

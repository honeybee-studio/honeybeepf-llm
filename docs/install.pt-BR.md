<!-- translated from docs/install.md at 250d516 -->

# Instalação { #install }

!!! note ""
    Esta página é uma tradução do original em inglês. Se houver diferença, vale o [original em inglês](https://honeybee-studio.github.io/honeybeepf-llm/install/).

O [Início rápido](quickstart.md) instala só o agente e lê as chamadas no log dele. Esta página o instala junto com o
pipeline de métricas: o agente envia métricas OpenTelemetry para um Collector, que o Prometheus coleta, e os
[dashboards do Grafana](grafana/README.md) leem do Prometheus.

## Agente, Collector e Prometheus { #agent-collector-and-prometheus }

Requer um cluster Kubernetes cujos nós rodem um kernel Linux 5.10+ ou RHEL 8.10 / 9.6 / 9.7, `kubectl` e Helm 3.
Estes passos instalam a versão mais recente, v0.5.0, com um Prometheus e um OpenTelemetry Collector incluídos. Os
charts são usados a partir da tag da versão, porque os de `main` podem exigir um agente mais novo do que a imagem dessa versão.

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

Acompanhe as chamadas ao LLM à medida que acontecem:

```bash
kubectl logs -n honeybeepf ds/honeybeepf-llm -f | grep 'LLM SUCCESS'
```

`ds/honeybeepf-llm` acompanha um único Pod do agente; em um cluster com vários nós, as chamadas de cada nó ficam no Pod
do agente daquele nó.

```
2026-10-26T08:43:35.510264Z  INFO honeybeepf_llm::probes::builtin::llm::processor::background: LLM SUCCESS | PID: 1686847 | Model: claude-test | Transport: tls | Latency: 0.00s | Tokens: 140 (Prompt: 125, Compl: 15, Cache read: 100, Cache write: 0) | Pod: team-b/support-bot-589db898dd-rv9ts
```

O Prometheus incluído é o Service `honeybeepf-llm-prometheus-server` (porta 80) no namespace `honeybeepf`; a
[referência de métricas](metrics.md) traz exemplos de consultas, e [Dashboards do Grafana](grafana/README.md) mostra como
carregar o dashboard de custos.

## Um coletor fora do cluster { #a-collector-outside-the-cluster }

Para enviar métricas a um coletor fora do cluster, use TLS e, se o coletor exigir, um cabeçalho de autenticação:

```bash
kubectl -n honeybeepf create secret generic otel-ca --from-file=ca.crt=./collector-ca.crt   # only for a private CA
kubectl -n honeybeepf create secret generic otel-auth --from-literal=headers='authorization=Bearer <token>'
helm upgrade --install honeybeepf-llm charts/honeybeepf-llm -n honeybeepf --reuse-values \
  --set output.otlp.endpoint=https://otel.example.com:4317 \
  --set output.otlp.caSecret=otel-ca \
  --set output.otlp.headersSecret.name=otel-auth --set output.otlp.headersSecret.key=headers
```

Um endpoint `https://` é validado com a CA definida em `caSecret` ou, se ela não existir, com as CAs raiz do sistema;
`host:port` e `http://` continuam sem criptografia.

## Permissões e recursos { #permissions-and-resources }

O agente precisa de `hostPID` e das capabilities `BPF`, `PERFMON`, `SYS_ADMIN`, `SYS_PTRACE`, `NET_ADMIN` e `SYS_RESOURCE`, e solicita 50m de CPU / 128Mi por nó (limites de 1 CPU / 1Gi); veja
[`values.yaml`](https://github.com/honeybee-studio/honeybeepf-llm/blob/main/charts/honeybeepf-llm/values.yaml).

Duas dessas capabilities só são necessárias em kernels mais antigos. O chart as concede para que uma única instalação
funcione em todos os nós; se todos os seus nós rodarem um kernel mais novo, você pode removê-las:

| Capability | Necessária quando um nó roda | Por quê | Pode ser removida quando todos os nós rodam |
|---|---|---|---|
| `NET_ADMIN` | um kernel anterior ao 6.9 | o agente cria um socket map na inicialização, mesmo com a captura de texto em claro desligada, e esses kernels exigem `NET_ADMIN` para isso | 6.9 ou posterior |
| `SYS_RESOURCE` | um kernel anterior ao 5.11 | esses kernels contam a memória dos maps BPF no limite de memlock, que o agente aumenta na inicialização | 5.11 ou posterior |

Definir a lista substitui a lista padrão inteira, então mantenha as outras quatro. Por exemplo, se todos os nós rodam
6.9 ou posterior:

```yaml
securityContext:
  capabilities:
    add: [BPF, PERFMON, SYS_ADMIN, SYS_PTRACE]
```

Se faltar uma capability de que o kernel do nó precisa, o agente encerra na inicialização com `Operation not permitted`
ao criar um map.

## Desinstalação { #uninstall }

```bash
helm uninstall honeybeepf-llm honeybeepf-llm-otel-collector honeybeepf-llm-prometheus -n honeybeepf
kubectl delete namespace honeybeepf
```

Antes de instalar uma versão, você pode [verificar as assinaturas dela](verify.md); para rodar uma imagem compilada a
partir do seu próprio checkout, veja [Compilar a partir do código-fonte](build.md).

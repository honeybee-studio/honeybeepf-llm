<!-- translated from docs/quickstart.md at 250d516 -->

# Início rápido { #quickstart }

!!! note ""
    Esta página é uma tradução do original em inglês. Se houver diferença, vale o [original em inglês](https://honeybee-studio.github.io/honeybeepf-llm/quickstart/).

Veja em poucos minutos as chamadas ao LLM de todos os Pods do seu cluster. Este guia instala só o agente e lê as
chamadas no log dele; a [Instalação](install.md) adiciona o Prometheus, um OpenTelemetry Collector e o dashboard de
custos do Grafana.

## Antes de começar { #before-you-start }

- Um cluster Kubernetes cujos nós rodem Linux 5.10 ou posterior, ou um kernel RHEL 8.10, 9.6 ou 9.7.
- `kubectl` e Helm 3.
- Pods que chamem um provedor de LLM via HTTPS a partir de Python, Node.js, Go, curl ou qualquer outra coisa que use
  OpenSSL. Ainda não tem tráfego de LLM? Comece pela [demo](demo.md).

## 1. Instale o agente { #1-install-the-agent }

```bash
kubectl create namespace honeybeepf
helm install honeybeepf-llm oci://ghcr.io/honeybee-studio/charts/honeybeepf-llm -n honeybeepf
```

O chart roda um Pod do agente por nó, como um DaemonSet. Suas aplicações, os SDKs delas e as chaves de API continuam
como estão.

## 2. Acompanhe as chamadas { #2-watch-the-calls }

```bash
kubectl logs -n honeybeepf ds/honeybeepf-llm -f | grep 'LLM SUCCESS'
```

Cada chamada ao LLM gera uma linha, com o modelo, as contagens de tokens e o Pod que a fez:

```
LLM SUCCESS | PID: 1686847 | Model: claude-test | Transport: tls | Latency: 0.00s | Tokens: 140 (Prompt: 125, Compl: 15, Cache read: 100, Cache write: 0) | Pod: team-b/support-bot-589db898dd-rv9ts
```

`ds/honeybeepf-llm` acompanha um único Pod do agente. Em um cluster com vários nós, as chamadas de cada nó ficam no Pod
do agente daquele nó: `kubectl logs -n honeybeepf -l app.kubernetes.io/name=honeybeepf-llm --prefix -f` acompanha
todos eles.

Não apareceu nenhuma linha? Veja [Solução de problemas](troubleshooting.md).

## 3. Próximos passos { #3-next }

- [Envie as métricas para o Prometheus e abra o dashboard de custos](install.md).
- [Verifique quais das suas cargas de trabalho o agente vê](guides/coverage.md).
- [Veja como as chamadas são precificadas](cost.md) e [adicione seus próprios preços](guides/prices.md).

Para remover:

```bash
helm uninstall honeybeepf-llm -n honeybeepf
kubectl delete namespace honeybeepf
```

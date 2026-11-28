---
title: Sua conta de LLM, dividida por equipe
description: Sua conta de LLM, dividida por equipe. Tokens e custo estimado por namespace, Pod e modelo do Kubernetes, lidos com eBPF. Sem mudanças no código, sem proxy, sem SDK.
hide:
  - navigation
  - toc
---
<!-- translated from docs/index.md at 250d516 -->

<div class="hb-hero" markdown>

<div class="hb-hero__text" markdown>

<h1>Sua conta de LLM, <em>dividida por equipe.</em></h1>

<p class="hb-sub">Seu provedor manda só o valor total. O honeybeepf-llm mostra qual namespace, Pod e modelo gastou esse
valor, sem mexer nas suas aplicações.</p>

[Início rápido :material-arrow-right:](quickstart.md){ .md-button .md-button--primary }
[GitHub :fontawesome-brands-github:](https://github.com/honeybee-studio/honeybeepf-llm){ .md-button }

<p class="hb-meta">Código aberto, Apache-2.0 · DaemonSet eBPF para Kubernetes · Métricas OpenTelemetry</p>

</div>

<div class="hb-receipt-wrap">
<div class="hb-receipt" data-hb-live role="img" aria-label="Um recibo que imprime as chamadas ao LLM à medida que acontecem no cluster de demonstração, com um subtotal por equipe: team-b $1.96, team-a $0.85, team-c $0.17, total $2.98">
  <div class="hb-receipt__head">
    <strong>honeybeepf-llm</strong>
    <span>cluster demo · nó worker-1</span>
    <span class="hb-receipt__live"><i></i>imprimindo ao vivo</span>
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
  <div class="hb-receipt__total"><span>Total</span><span data-hb-total>$2.98</span></div>
  <div class="hb-receipt__foot">Código da aplicação alterado: 0 linhas<br>Chamadas simuladas nas cargas de trabalho da demo</div>
</div>
</div>

</div>

## Como obter isso { #how-you-get-it }

<div class="hb-steps" markdown>

<div class="hb-step" markdown>
<div class="hb-step__art hb-step__art--code">$ helm install honeybeepf-llm \
    oci://ghcr.io/honeybee-studio/\
    charts/honeybeepf-llm

<span>DaemonSet pronto: 1 agente por nó</span></div>
<div class="hb-step__n">1</div>
<h3>Instale um único DaemonSet</h3>

Um agente por nó. Suas aplicações, SDKs e chaves de API continuam como estão.
</div>

<div class="hb-step" markdown>
<div class="hb-step__art hb-step__art--log"><span>pod</span>    team-b/support-bot
<span>host</span>   api.anthropic.com
<span>model</span>  claude-sonnet-4-5
<span>tokens</span> 1,840  <b>$0.011</b></div>
<div class="hb-step__n">2</div>
<h3>Ele lê todas as chamadas ao LLM</h3>

Cada chamada é lida no kernel, no ponto em que sua aplicação a entrega ao TLS, e associada ao seu Pod.
</div>

<div class="hb-step" markdown>
<div class="hb-step__art hb-step__art--shot"><img src="images/grafana-cost-narrow.png" alt="Painel do Grafana: gasto estimado por carga de trabalho, team-b support-bot $1.96, team-a chat-api $0.85, team-c summarizer $0.17"></div>
<div class="hb-step__n">3</div>
<h3>Veja o custo por equipe</h3>

Tokens e dólares por namespace, Pod e modelo no Prometheus, com um [dashboard do Grafana](grafana/README.md) incluído.
</div>

</div>

## Experimente { #try-it }

=== "Helm"

    Requer um cluster Kubernetes com nós Linux 5.10+, `kubectl` e Helm 3.

    ```bash
    kubectl create namespace honeybeepf
    helm install honeybeepf-llm oci://ghcr.io/honeybee-studio/charts/honeybeepf-llm -n honeybeepf
    kubectl logs -n honeybeepf ds/honeybeepf-llm -f | grep 'LLM SUCCESS'
    ```

    Para o dashboard de custos do Grafana, adicione o Prometheus e o OpenTelemetry Collector incluídos: veja
    [Instalação](install.md).

=== "Demo, sem chaves de API"

    Executa um provedor simulado, clientes de LLM em três namespaces de equipe e o Grafana em qualquer cluster (kind,
    k3d, minikube).

    ```bash
    git clone https://github.com/honeybee-studio/honeybeepf-llm.git && cd honeybeepf-llm
    examples/demo/demo.sh up
    kubectl -n monitoring port-forward svc/grafana 3000:80
    ```

    Depois abra <http://localhost:3000> e o dashboard "LLM Cost Observability".

## Por que não um gateway ou um SDK? { #why-not-a-gateway-or-an-sdk }

<div class="hb-compare" markdown>

| | Gateway / proxy | Instrumentação via SDK | honeybeepf-llm (eBPF) |
|---|---|---|---|
| Mudança na aplicação | nova URL base | adicionar código, refazer o deploy | nenhuma |
| No caminho da requisição | sim | no mesmo processo | não: lê cópias dos buffers |
| Chamadas que ele vê | só as roteadas por ele | só o código instrumentado | todos os Pods do nó |
| Chaves de API | passam por ele | no processo da aplicação | nunca vão para o log (só caminho, modelo e tokens) |

</div>

## Como funciona { #how-it-works }

![uprobes em SSL_write/SSL_read copiam o texto em claro para um ring buffer; o agente interpreta o HTTP, identifica o provedor, associa o cgroup a um Pod e exporta métricas OTLP](images/how-it-works.svg)

1. O agente anexa uprobes a `SSL_write` e `SSL_read` em cada `libssl` que um processo mapeia, e a `crypto/tls` em
   executáveis Go.
2. As uprobes copiam o buffer com o texto em claro, o pid e o id do cgroup para um ring buffer BPF.
3. No espaço de usuário, o parser de HTTP/1.1, HTTP/2 e SSE remonta cada chamada, identifica o provedor pelo host e
   pelo caminho e lê os campos de uso da resposta.
4. O cgroup associa o processo ao seu Pod, e o resultado é exportado como métricas OpenTelemetry.

## Leia a seguir { #read-next }

<div class="hb-index" markdown>

- [Início rápido](quickstart.md): instale o agente e veja as chamadas em poucos minutos.
- [Como funciona](how-it-works.md): de um buffer TLS a dólares por Pod, passo a passo.
- [O que é suportado](providers.md): provedores, runtimes, protocolos e kernels.
- [Encontre as cargas de trabalho que o agente não vê](guides/coverage.md): uma única consulta lista todas elas.
- [Adicione um provedor](guides/custom-providers.md) ou [seus próprios preços](guides/prices.md).
- [Métricas](metrics.md): cada métrica e rótulo, e por que uma chamada não foi registrada.
- [Orçamentos e alertas](alerts.md): regras de gasto e de qualidade dos dados que vêm com o chart.
- [Solução de problemas](troubleshooting.md) e [Perguntas frequentes](faq.md).
- [Revisão de segurança](security-review.md): hooks do kernel, privilégios, tratamento de dados e lacunas conhecidas.
- [Benchmark de overhead](benchmark.md): CPU, memória e throughput com e sem o agente.

</div>

!!! note ""
    Esta página é uma tradução do original em inglês. Se houver diferença, vale o [original em inglês](https://honeybee-studio.github.io/honeybeepf-llm/).

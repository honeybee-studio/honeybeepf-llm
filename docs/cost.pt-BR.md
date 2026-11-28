<!-- translated from docs/cost.md at 250d516 -->

# Custos e preços { #cost-and-prices }

!!! note ""
    Esta página é uma tradução do original em inglês. Se houver diferença, vale o [original em inglês](https://honeybee-studio.github.io/honeybeepf-llm/cost/).

Cada chamada registrada recebe um custo estimado em USD: a contagem de tokens dela multiplicada pelos preços do modelo.

## De onde vêm os preços { #where-prices-come-from }

- Uma **tabela de preços embutida** e datada, consultada pelo nome do modelo na resposta. Um nome datado como
  `claude-sonnet-4-5@20250929` corresponde ao seu modelo.
- **[Seus próprios preços](guides/prices.md)** por cima: modelos que faltam na tabela ou um desconto de contrato.
- Um modelo sem preço **nunca é registrado como $0**: os tokens dele vão para `llm_unpriced_tokens_total`.

**Como um preço é escolhido**

| | |
|---|---|
| Por | o nome do modelo, não o host: Azure OpenAI, Bedrock, Vertex AI e endpoints regionais são precificados como a API do próprio provedor, então uma sobretaxa regional, como a de residência de dados da OpenAI (+10%), não é somada |
| Preços | os preços de tabela publicados pelos provedores; a entrada de cada provedor indica a página de preços dele e a data em que foi conferida |
| Não incluído | taxas de ferramentas por requisição, como busca na web |

Compare o custo com `llm_unpriced_tokens_total` antes de confiar em um total.

!!! note
    As métricas de custo só aparecem quando há uma tabela de preços carregada. Uma tabela embutida rejeitada só gera
    um aviso no log, e o agente passa a rodar sem elas.

## Gasto por carga de trabalho { #spend-per-workload }

```promql
sum by (target_namespace, target_workload_name) (rate(honeybeepf_llm_llm_cost_usd_total[5m])) * 60
```

USD por minuto, por namespace e carga de trabalho.

## Chamadas que passam por um gateway { #calls-through-a-gateway }

Uma chamada que passa por um gateway dentro do cluster (LiteLLM, um proxy da empresa) é feita duas vezes: pela
aplicação até o gateway e pelo gateway até o provedor. Quando o agente registra as duas, cadastre o gateway como um
[provedor](guides/custom-providers.md) com `gateway: true`, para distinguir os dois registros:

| Registro | Rótulos | Use para |
|---|---|---|
| aplicação → gateway | o Pod da aplicação, `hop="gateway"` | gasto por equipe ou carga de trabalho |
| gateway → provedor | o Pod do gateway, sem `hop` | gasto total: o modelo e o uso informados pelo próprio provedor |

As séries de cada carga de trabalho são as próprias chamadas dela, então o gasto por namespace ou carga de trabalho não
precisa de filtro, e o gateway aparece como mais uma carga de trabalho. Somando todas as cargas de trabalho sem o salto do gateway,
cada chamada é contada uma vez só:

```promql
sum(increase(honeybeepf_llm_llm_cost_usd_total{hop!="gateway"}[1d]))
```

O dashboard de custos incluído faz isso nos totais e nos painéis por modelo, por modalidade e por operação.

!!! warning
    Defina `gateway: true` apenas para um gateway cujas chamadas aos provedores este agente registra: um que rode nos
    nós do cluster e chame os provedores via TLS ou por uma entrada `plaintext`. Para um gateway fora do cluster, o
    registro dele é o único, e o filtro acima o descartaria.

## As métricas de custo { #the-cost-metrics }

| Métrica | O que conta | Rótulos extras |
|---|---|---|
| `honeybeepf_llm_llm_cost_usd_total` | o custo de cada chamada, em USD | `cost_type`, `price_table_version` |
| `honeybeepf_llm_llm_cache_savings_ceiling_usd_total` | quanto, no máximo, o cache de prompt poderia ter economizado | `cache_price`, `price_table_version` |
| `honeybeepf_llm_llm_unpriced_tokens_total` | tokens de chamadas que não puderam ser precificadas | `reason`, `price_table_version` |

As três têm os rótulos por chamada (`model`, `status`, `attempt`, `target_namespace`, `target_pod_name`,
`target_workload_kind`, `target_workload_name`), mas não `transport` nem `operation`. Os contadores em USD não têm
unidade, então o exportador não acrescenta uma ao nome.

### `cost_type`: como o custo é dividido { #cost_type-how-the-cost-is-split }

Como na fatura de um provedor. Uma parte igual a zero não é somada, então uma chamada só tem as séries que usou.

| `cost_type` | Cobrado por |
|---|---|
| `input` | tokens de prompt que não foram lidos do cache nem gravados nele |
| `cache_read` | tokens de prompt servidos a partir do cache do provedor |
| `cache_write` | tokens de prompt gravados no cache do provedor |
| `output` | todos os tokens de completion, incluindo os tokens de raciocínio |

### `attempt`: atendida ou recusada { #attempt-served-or-declined }

| `attempt` | A chamada |
|---|---|
| `served` | o provedor a atendeu |
| `declined` | uma tentativa anterior de fallback do lado do servidor que o provedor cobrou, mas desviou para outro destino: só tokens e custo, sem contagem de requisições nem latência |

Chamadas registradas como erro não geram custo.

### `price_table_version` { #price_table_version }

A `version` da tabela embutida, com `+custom-<hash>` acrescentado quando seus preços são mesclados sobre ela. Uma
mudança de preço aparece como uma nova série.

### Teto de economia com cache { #cache-savings-ceiling }

Os tokens de entrada não cacheados da chamada, calculados pelo preço de leitura de cache do modelo em vez do preço de
entrada: um limite superior do que o cache de prompt poderia economizar. Emitido apenas para chamadas precificadas que
não sejam de embeddings, e apenas quando maior que zero.

| `cache_price` | Significado |
|---|---|
| `known` | o modelo tem um preço de leitura de cache |
| `assumed_free` | ele não tem, então o limite é o custo total de entrada |

### Tokens sem preço { #unpriced-tokens }

| `reason` | A chamada não pôde ser precificada porque |
|---|---|
| `unknown_model` | o nome do modelo estava ausente ou não constava na tabela |
| `missing_price` | um preço de que a chamada precisa está ausente ou passou do último dia de validade |
| `unsupported_modifier` | o service tier, a velocidade ou o modificador geográfico da chamada não tem preço |
| `inconsistent_usage` | as contagens de tokens de cache informadas excedem o total do prompt do qual fazem parte |

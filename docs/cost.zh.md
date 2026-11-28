<!-- translated from docs/cost.md at 250d516 -->

# 成本与价格 { #cost-and-prices }

!!! note ""
    本页译自英文原文。如有出入，以[英文原文](https://honeybee-studio.github.io/honeybeepf-llm/cost/)为准。

每次记录下来的调用都有一个以美元（USD）计的估算成本，即 token 数乘以模型单价。

## 价格来源 { #where-prices-come-from }

- 带日期的**内置价格表**，按响应中的模型名称匹配。带日期的名称（如 `claude-sonnet-4-5@20250929`）会匹配到对应的模型。
- 叠加在其上的**[自定义价格](guides/prices.md)**：用于价格表中没有的模型，或合同折扣。
- 没有价格的模型**绝不会记为 $0**：其 token 改为计入 `llm_unpriced_tokens_total`。

**如何选择价格**

| | |
|---|---|
| 依据 | 模型名称，而非主机：Azure OpenAI、Bedrock、Vertex AI 和区域端点都按服务商自身 API 的价格计算，因此不会加上区域附加费，例如 OpenAI 数据驻留（+10%） |
| 价格 | 服务商公布的标价；每个服务商条目都注明了其定价页面和核对日期 |
| 不包括 | 按请求收取的工具费用，例如网页搜索 |

在采信总额之前，先把成本和 `llm_unpriced_tokens_total` 对照一下。

!!! note
    只有在价格表成功加载时才会出现成本指标。内置价格表被拒绝时只会记录一条警告，agent 随后照常运行，只是没有成本指标。

## 每个工作负载的支出 { #spend-per-workload }

```promql
sum by (target_namespace, target_workload_name) (rate(honeybeepf_llm_llm_cost_usd_total[5m])) * 60
```

按命名空间和工作负载统计，单位为美元/分钟。

## 经过网关的调用 { #calls-through-a-gateway }

经过集群内网关（LiteLLM、公司代理）的调用会发生两次：一次由应用发往网关，一次由网关发往服务商。如果 agent 两次都记录到了，把网关列为一个[服务商](guides/custom-providers.md)并设置 `gateway: true`，以便区分这两条记录：

| 记录 | 标签 | 用途 |
|---|---|---|
| 应用 → 网关 | 应用的 Pod，`hop="gateway"` | 按团队或工作负载统计支出 |
| 网关 → 服务商 | 网关的 Pod，无 `hop` | 总支出：服务商自身的模型和用量 |

每个工作负载的序列只包含它自己发起的调用，因此按命名空间或工作负载统计支出时无需过滤，网关只是多出的一个工作负载。对所有工作负载求和时，排除网关这一跳，每次调用就只计一次：

```promql
sum(increase(honeybeepf_llm_llm_cost_usd_total{hop!="gateway"}[1d]))
```

随附的成本仪表板在总计面板以及按模型、按模态、按操作的面板中都做了这种处理。

!!! warning
    只有当本 agent 能记录到网关发往服务商的调用时，才设置 `gateway: true`：也就是网关运行在集群节点上，并通过 TLS 或 `plaintext` 条目调用服务商。如果网关在集群外，它的记录就是唯一的一条，上面的过滤会把它丢掉。

## 成本指标 { #the-cost-metrics }

| 指标 | 统计内容 | 额外标签 |
|---|---|---|
| `honeybeepf_llm_llm_cost_usd_total` | 每次调用的成本，以 USD 计 | `cost_type`、`price_table_version` |
| `honeybeepf_llm_llm_cache_savings_ceiling_usd_total` | 提示缓存最多能节省多少 | `cache_price`、`price_table_version` |
| `honeybeepf_llm_llm_unpriced_tokens_total` | 无法计价的调用的 token | `reason`、`price_table_version` |

这三个指标都带有调用级标签（`model`、`status`、`attempt`、`target_namespace`、`target_pod_name`、`target_workload_kind`、`target_workload_name`），但不带 `transport` 或 `operation`。USD 计数器不带单位，因此导出器不会在名称后附加单位。

### `cost_type`：成本如何拆分 { #cost_type-how-the-cost-is-split }

拆分方式与服务商账单一致。为零的部分不会生成序列，因此一次调用只会有它实际用到的序列。

| `cost_type` | 计费对象 |
|---|---|
| `input` | 既未从缓存读取、也未写入缓存的提示 token |
| `cache_read` | 由服务商缓存提供的提示 token |
| `cache_write` | 写入服务商缓存的提示 token |
| `output` | 全部补全 token，包括推理 token |

### `attempt`：已处理或已放弃 { #attempt-served-or-declined }

| `attempt` | 该调用 |
|---|---|
| `served` | 服务商处理了它 |
| `declined` | 服务端回退中较早的一次尝试：服务商已对它计费，但随后转由别处处理；只有 token 和成本，没有请求数或延迟 |

被记录为错误的调用不记录成本。

### `price_table_version` { #price_table_version }

内置价格表的 `version`；合并了你的自定义价格时，会附加 `+custom-<hash>`。价格变化会显示为一个新的序列。

### 缓存节省上限 { #cache-savings-ceiling }

将调用中未缓存的输入 token 按模型的缓存读取价格而非输入价格计价：这是提示缓存所能节省金额的上限。仅针对已计价的非 embedding 调用上报，且仅在值大于零时上报。

| `cache_price` | 含义 |
|---|---|
| `known` | 该模型有缓存读取价格 |
| `assumed_free` | 该模型没有缓存读取价格，因此上限为全部输入成本 |

### 未计价的 token { #unpriced-tokens }

| `reason` | 调用无法计价的原因 |
|---|---|
| `unknown_model` | 模型名称缺失或不在价格表中 |
| `missing_price` | 调用所需的某个价格缺失或已过最后有效日期 |
| `unsupported_modifier` | 调用的服务等级、速度或地域修饰符没有价格 |
| `inconsistent_usage` | 上报的缓存 token 数超过了它们所属的提示 token 总数 |

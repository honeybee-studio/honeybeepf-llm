---
title: 你的 LLM 账单，按团队拆分
description: 你的 LLM 账单，按团队拆分。用 eBPF 读取每个 Kubernetes 命名空间、Pod 和模型的 token 用量与估算成本。无需改代码，无需代理，无需 SDK。
hide:
  - navigation
  - toc
---
<!-- translated from docs/en/index.md, git blob 16eafce5ce85 -->

<div class="hb-hero" markdown>

<div class="hb-hero__text" markdown>

<h1>你的 LLM 账单，<em>按团队拆分。</em></h1>

<p class="hb-sub">服务商只给你一个总数。honeybeepf-llm 告诉你这笔钱花在了哪个命名空间、哪个 Pod、哪个模型上，而且无需改动应用。</p>

[快速开始 :material-arrow-right:](quickstart.md){ .md-button .md-button--primary }
[GitHub :fontawesome-brands-github:](https://github.com/honeybee-studio/honeybeepf-llm){ .md-button }

<p class="hb-meta">开源，Apache-2.0 · 适用于 Kubernetes 的 eBPF DaemonSet · OpenTelemetry 指标</p>

</div>

<div class="hb-receipt-wrap">
<div class="hb-receipt" data-hb-live role="img" aria-label="一张小票，实时打印演示集群上发生的 LLM 调用，并按团队小计：team-b $1.96，team-a $0.85，team-c $0.17，合计 $2.98">
  <div class="hb-receipt__head">
    <strong>honeybeepf-llm</strong>
    <span>集群 demo · 节点 worker-1</span>
    <span class="hb-receipt__live"><i></i>实时打印中</span>
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
  <div class="hb-receipt__total"><span>合计</span><span data-hb-total>$2.98</span></div>
  <div class="hb-receipt__foot">应用代码改动：0 行<br>演示工作负载上的模拟调用</div>
</div>
</div>

</div>

## 如何做到 { #how-you-get-it }

<div class="hb-steps" markdown>

<div class="hb-step" markdown>
<div class="hb-step__art hb-step__art--code">$ helm install honeybeepf-llm \
    oci://ghcr.io/honeybee-studio/\
    charts/honeybeepf-llm

<span>DaemonSet ready: 1 agent per node</span></div>
<div class="hb-step__n">1</div>
<h3>安装一个 DaemonSet</h3>

每个节点一个 agent。你的应用、SDK 和 API 密钥保持不变。
</div>

<div class="hb-step" markdown>
<div class="hb-step__art hb-step__art--log"><span>pod</span>    team-b/support-bot
<span>host</span>   api.anthropic.com
<span>model</span>  claude-sonnet-4-5
<span>tokens</span> 1,840  <b>$0.011</b></div>
<div class="hb-step__n">2</div>
<h3>读取每一次 LLM 调用</h3>

每次调用都在应用把数据交给 TLS 的那一刻于内核中读取，并关联到发起它的 Pod。
</div>

<div class="hb-step" markdown>
<div class="hb-step__art hb-step__art--shot" markdown>![Grafana 面板：每个工作负载的估算支出，team-b support-bot $1.96，team-a chat-api $0.85，team-c summarizer $0.17](../images/grafana-cost-narrow.png)</div>
<div class="hb-step__n">3</div>
<h3>查看每个团队的成本</h3>

在 Prometheus 中按命名空间、Pod 和模型统计 token 与美元金额，并附带 [Grafana 仪表板](grafana/README.md)。
</div>

</div>

## 试一试 { #try-it }

=== "Helm"

    需要一个节点运行 Linux 5.10+ 的 Kubernetes 集群、`kubectl` 和 Helm 3。

    ```bash
    kubectl create namespace honeybeepf
    helm install honeybeepf-llm oci://ghcr.io/honeybee-studio/charts/honeybeepf-llm -n honeybeepf
    kubectl logs -n honeybeepf ds/honeybeepf-llm -f | grep 'LLM SUCCESS'
    ```

    如需 Grafana 成本仪表板，请加装随附的 Prometheus 和 OpenTelemetry Collector，参见[安装](install.md)。

=== "演示，无需 API 密钥"

    在任意集群（kind、k3d、minikube）中运行一个模拟服务商、三个团队命名空间中的 LLM 客户端以及 Grafana。

    ```bash
    git clone https://github.com/honeybee-studio/honeybeepf-llm.git && cd honeybeepf-llm
    examples/demo/demo.sh up
    kubectl -n monitoring port-forward svc/grafana 3000:80
    ```

    然后打开 <http://localhost:3000>，进入“LLM Cost Observability”仪表板。

## 为什么不用网关或 SDK？ { #why-not-a-gateway-or-an-sdk }

<div class="hb-compare" markdown>

| | 网关 / 代理 | SDK 插桩 | honeybeepf-llm（eBPF） |
|---|---|---|---|
| 应用改动 | 换用新的 base URL | 添加代码并重新部署 | 无 |
| 是否在请求路径中 | 是 | 在进程内 | 否：读取缓冲区副本 |
| 能看到的调用 | 仅经它路由的调用 | 仅已插桩的代码 | 节点上的所有 Pod |
| API 密钥 | 经过它传递 | 在应用进程中 | 从不记录（仅记录路径、模型和 token） |

</div>

## 工作原理 { #how-it-works }

![SSL_write/SSL_read 上的 uprobe 将明文复制到环形缓冲区；agent 解析 HTTP、匹配服务商、将 cgroup 映射到 Pod 并导出 OTLP 指标](../images/how-it-works.svg)

1. agent 在进程映射的每个 `libssl` 里给 `SSL_write` 和 `SSL_read` 挂载 uprobe；对 Go 可执行文件，则挂载到 `crypto/tls`。
2. uprobe 将明文缓冲区、pid 和 cgroup id 复制到 BPF 环形缓冲区。
3. 在用户空间，HTTP/1.1、HTTP/2 和 SSE 解析器重组每次调用，根据主机和路径识别服务商，并从响应中读取用量字段。
4. 根据 cgroup 找到进程所属的 Pod，结果以 OpenTelemetry 指标导出。

## 延伸阅读 { #read-next }

<div class="hb-index" markdown>

- [快速开始](quickstart.md)：安装 agent，几分钟内即可看到调用。
- [工作原理](how-it-works.md)：从 TLS 缓冲区到每个 Pod 的美元成本，逐步说明。
- [支持范围](providers.md)：服务商、运行时、协议和内核。
- [找出未被观测到的工作负载](guides/coverage.md)：一条查询即可列出。
- [添加服务商](guides/custom-providers.md)或[自定义价格](guides/prices.md)。
- [指标](metrics.md)：所有指标和标签，以及调用未被记录的原因。
- [预算与告警](alerts.md)：随 chart 提供的支出和数据质量规则。
- [故障排查](troubleshooting.md)和[常见问题](faq.md)。
- [安全评审](security-review.md)：内核钩子、权限、数据处理和已知不足。
- [开销基准测试](benchmark.md)：启用与不启用 agent 时的 CPU、内存和吞吐量。

</div>

!!! note ""
    本页译自英文原文。如有出入，以[英文原文](https://honeybee-studio.github.io/honeybeepf-llm/)为准。

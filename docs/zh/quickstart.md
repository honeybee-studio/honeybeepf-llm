<!-- translated from docs/en/quickstart.md, git blob 39a92ae8a6a4 -->

# 快速开始 { #quickstart }

!!! note ""
    本页译自英文原文。如有出入，以[英文原文](https://honeybee-studio.github.io/honeybeepf-llm/quickstart/)为准。

几分钟内即可看到集群中每个 Pod 的 LLM 调用。本页只安装 agent，并从它的日志中读取调用；[安装](install.md)一页还会加上 Prometheus、OpenTelemetry Collector 和 Grafana 成本仪表板。

## 准备工作 { #before-you-start }

- 一个 Kubernetes 集群，节点运行 Linux 5.10 或更高版本，或 RHEL 8.10、9.6、9.7 内核。
- `kubectl` 和 Helm 3。
- 通过 HTTPS 调用 LLM 服务商的 Pod，调用方可以是 Python、Node.js、Go、curl 或任何使用 OpenSSL 的程序。还没有 LLM 流量？请改为运行[演示](demo.md)。

## 1. 安装 agent { #1-install-the-agent }

```bash
kubectl create namespace honeybeepf
helm install honeybeepf-llm oci://ghcr.io/honeybee-studio/charts/honeybeepf-llm -n honeybeepf
```

该 chart 以 DaemonSet 形式在每个节点上运行一个 agent Pod。你的应用及其 SDK 和 API 密钥保持不变。

## 2. 查看调用 { #2-watch-the-calls }

```bash
kubectl logs -n honeybeepf ds/honeybeepf-llm -f | grep 'LLM SUCCESS'
```

每次 LLM 调用占一行，包含模型、token 数以及发起调用的 Pod：

```
LLM SUCCESS | PID: 1686847 | Model: claude-test | Transport: tls | Latency: 0.00s | Tokens: 140 (Prompt: 125, Compl: 15, Cache read: 100, Cache write: 0) | Pod: team-b/support-bot-589db898dd-rv9ts
```

`ds/honeybeepf-llm` 只跟踪一个 agent Pod。在有多个节点的集群上，每个节点的调用只出现在该节点自己的 agent Pod 中：`kubectl logs -n honeybeepf -l app.kubernetes.io/name=honeybeepf-llm --prefix -f` 可以跟踪所有 agent Pod。

没有输出？参见[故障排查](troubleshooting.md)。

## 3. 下一步 { #3-next }

- [将指标发送到 Prometheus 并打开成本仪表板](install.md)。
- [查看哪些工作负载能被观测到](guides/coverage.md)。
- [了解调用如何计价](cost.md)，以及[添加自定义价格](guides/prices.md)。

卸载方法：

```bash
helm uninstall honeybeepf-llm -n honeybeepf
kubectl delete namespace honeybeepf
```

<!-- translated from docs/install.md at 250d516 -->

# 安装 { #install }

!!! note ""
    本页译自英文原文。如有出入，以[英文原文](https://honeybee-studio.github.io/honeybeepf-llm/install/)为准。

[快速开始](quickstart.md)只单独安装 agent，并从它的日志中读取调用。本页会同时安装指标管道：agent 将 OpenTelemetry 指标发送到 Collector，Prometheus 抓取 Collector，[Grafana 仪表板](grafana/README.md)再从 Prometheus 读取数据。

## Agent、Collector 和 Prometheus { #agent-collector-and-prometheus }

需要一个节点运行 Linux 5.10+ 或 RHEL 8.10 / 9.6 / 9.7 内核的 Kubernetes 集群、`kubectl` 和 Helm 3。以下步骤安装最新版本 v0.5.0，并附带 Prometheus 和 OpenTelemetry Collector。chart 要从发布标签检出，因为 `main` 分支上的 chart 可能依赖比发布镜像更新的 agent。

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

实时查看 LLM 调用：

```bash
kubectl logs -n honeybeepf ds/honeybeepf-llm -f | grep 'LLM SUCCESS'
```

`ds/honeybeepf-llm` 只跟踪一个 agent Pod；在有多个节点的集群上，每个节点的调用只出现在该节点自己的 agent Pod 中。

```
2026-10-26T08:43:35.510264Z  INFO honeybeepf_llm::probes::builtin::llm::processor::background: LLM SUCCESS | PID: 1686847 | Model: claude-test | Transport: tls | Latency: 0.00s | Tokens: 140 (Prompt: 125, Compl: 15, Cache read: 100, Cache write: 0) | Pod: team-b/support-bot-589db898dd-rv9ts
```

随附的 Prometheus 是 `honeybeepf` 命名空间中的 `honeybeepf-llm-prometheus-server` Service（端口 80）；[指标参考](metrics.md)中有示例查询，[Grafana 仪表板](grafana/README.md)介绍了如何加载成本仪表板。

## 集群外部的收集器 { #a-collector-outside-the-cluster }

要将指标发送到集群外部的收集器，请使用 TLS；如果收集器要求认证，还需加上认证头：

```bash
kubectl -n honeybeepf create secret generic otel-ca --from-file=ca.crt=./collector-ca.crt   # only for a private CA
kubectl -n honeybeepf create secret generic otel-auth --from-literal=headers='authorization=Bearer <token>'
helm upgrade --install honeybeepf-llm charts/honeybeepf-llm -n honeybeepf --reuse-values \
  --set output.otlp.endpoint=https://otel.example.com:4317 \
  --set output.otlp.caSecret=otel-ca \
  --set output.otlp.headersSecret.name=otel-auth --set output.otlp.headersSecret.key=headers
```

设置了 `caSecret` 时，`https://` 端点用其中的 CA 校验，否则用系统根证书；`host:port` 和 `http://` 形式则保持明文。

## 权限与资源 { #permissions-and-resources }

agent 需要 `hostPID` 以及 `BPF`、`PERFMON`、`SYS_ADMIN`、`SYS_PTRACE`、`NET_ADMIN` 和 `SYS_RESOURCE` 权能，每个节点请求 50m CPU / 128Mi（限制为 1 CPU / 1Gi）；参见 [`values.yaml`](https://github.com/honeybee-studio/honeybeepf-llm/blob/main/charts/honeybeepf-llm/values.yaml)。

其中两项权能只在较旧的内核上需要。chart 默认授予这两项，这样一次安装就能适用于所有节点；如果所有节点都运行较新的内核，可以移除它们：

| 权能 | 节点运行以下内核时需要 | 原因 | 所有节点都运行以下内核时可移除 |
|---|---|---|---|
| `NET_ADMIN` | 6.9 之前的内核 | agent 在启动时会创建一个 socket map（即使关闭了明文捕获也是如此），而在这些内核上创建它需要 `NET_ADMIN` | 6.9 或更高版本 |
| `SYS_RESOURCE` | 5.11 之前的内核 | 这些内核将 BPF map 内存计入 memlock 限制，agent 会在启动时提高该限制 | 5.11 或更高版本 |

设置该列表会整体替换默认值，因此要保留另外四项。例如，所有节点都运行 6.9 或更高版本时：

```yaml
securityContext:
  capabilities:
    add: [BPF, PERFMON, SYS_ADMIN, SYS_PTRACE]
```

如果缺少节点内核所需的某项权能，agent 会在启动、创建 map 时报 `Operation not permitted` 并退出。

## 卸载 { #uninstall }

```bash
helm uninstall honeybeepf-llm honeybeepf-llm-otel-collector honeybeepf-llm-prometheus -n honeybeepf
kubectl delete namespace honeybeepf
```

安装某个版本之前，你可以[验证其签名](verify.md)；要运行基于本地检出代码构建的镜像，参见[从源码构建](build.md)。

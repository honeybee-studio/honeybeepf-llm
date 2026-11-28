<!-- translated from docs/faq.md at 250d516 -->

# 常见问题 { #faq }

!!! note ""
    本页译自英文原文。如有出入，以[英文原文](https://honeybee-studio.github.io/honeybeepf-llm/faq/)为准。

## 我需要修改应用吗？ { #do-i-have-to-change-my-apps }

不需要。应用继续用自己的 SDK 和密钥直接调用服务商。没有要修改的 base URL，没有要导入的库，也无需重新部署。参见[工作原理](how-it-works.md)。

## 它会进入请求路径或拖慢调用吗？ { #does-it-sit-in-the-request-path-or-slow-my-calls-down }

不会。agent 在节点上读取 TLS 缓冲区的副本；请求与以前完全一样发往服务商，没有任何环节等待 agent。它占用的 CPU 和内存，以及在负载下对吞吐量的影响，见[开销基准测试](benchmark.md)中的测量结果。

## 它会读取我的提示词和 API 密钥吗？ { #does-it-read-my-prompts-and-api-keys }

探针会把每次调用解密后的字节复制到节点上 agent 的内存中，任何读取 TLS 的工具都必须这样做。agent 从中解析出用量字段后即将其丢弃：提示词、响应、请求头和 API 密钥绝不会被导出，也不会写入日志。[安全评审](security-review.md#3-data-handling)逐个字段说明了哪些内容会被导出和记录。

## 数据有多准确？ { #how-accurate-are-the-numbers }

token 数取自服务商自己的统计：agent 读取每个响应的用量字段，包括缓存 token 和推理 token。成本是估算值：这些 token 乘以带日期的价格表，你可以用合同价格覆盖它（[成本与价格](cost.md)）。agent 看到但无法解析的调用会连同原因一起计数，而不是被静默丢弃。

## 它支持哪些服务商、语言和协议？ { #which-providers-languages-and-protocols-does-it-cover }

OpenAI、Anthropic、Gemini、Bedrock、Azure OpenAI、Vertex AI、Meta 以及许多兼容 OpenAI 的 API；支持 Python、Node.js、Go、curl 以及其他任何使用 OpenSSL 的程序；支持 HTTP/1.1 和 HTTP/2（包括流式传输），以及基于 WebSocket 的 OpenAI Realtime、Responses 和 Gemini Live。暂不支持：rustls、Java、其他 WebSocket API 和 HTTP/3。完整矩阵见[支持范围](providers.md)，[一个指标](guides/coverage.md)会列出集群中未被观测到的工作负载。

## 我们已经在运行 LiteLLM 之类的 LLM 网关，还需要这个吗？ { #we-already-run-an-llm-gateway-such-as-litellm-do-we-need-this }

网关只能看到经它路由的调用；agent 能看到从节点发出的每一次调用，包括绕过网关的调用，并指出每次调用来自哪个 Pod，且无需修改应用。如果开启了[按形态检测](guides/detect-by-shape.md)，集群内的网关会被记录两次（应用到网关、网关到服务商）；该页面说明了如何只计一次。

## 节点需要满足什么条件？ { #what-does-it-need-on-the-nodes }

Linux 5.10 或更高版本，或 RHEL 8.10、9.6 或 9.7 内核，需启用 BTF，架构为 x86-64 或 arm64。agent 不是特权容器：它只使用 `hostPID` 和少量权能运行，详见[安装](install.md#permissions-and-resources)和[安全评审](security-review.md#2-privileges)。

## 没有 Kubernetes 能用吗？ { #does-it-work-without-kubernetes }

可以，以二进制文件形式直接在 Linux 主机上运行，构建时不启用 `k8s` 特性；此时指标不带命名空间、Pod 或工作负载标签。参见[不使用 Kubernetes 运行](run-binary.md)。

## 在哪里报告 bug 或申请支持某个服务商？ { #where-do-i-report-a-bug-or-ask-for-a-provider }

请在 [GitHub Issues](https://github.com/honeybee-studio/honeybeepf-llm/issues) 中提出。

<!-- translated from docs/how-it-works.md at 250d516 -->

# 工作原理 { #how-it-works }

!!! note ""
    本页译自英文原文。如有出入，以[英文原文](https://honeybee-studio.github.io/honeybeepf-llm/how-it-works/)为准。

调用 LLM 服务商的应用会把明文请求交给它的 TLS 库，再从 TLS 库取回明文响应。honeybeepf-llm 就在这个环节，在节点上用 eBPF 读取这些字节的副本。应用、SDK 以及到服务商的网络路径都保持不变。

![SSL_write/SSL_read 上的 uprobe 将明文复制到环形缓冲区；agent 解析 HTTP、匹配服务商、将 cgroup 映射到 Pod 并导出 OTLP 指标](images/how-it-works.svg)

## 1. 找到 TLS 代码 { #1-find-the-tls-code }

agent 以 DaemonSet 方式运行，每个节点一个 Pod。它检查所在节点上的每个进程，找出每个进程的 TLS 代码在哪里：

| 进程 | agent 挂载到 |
|---|---|
| 映射了共享的 `libssl.so`（Python、发行版软件包中的 Node.js、curl） | 该库中的 `SSL_read`、`SSL_write` 及其 `_ex` 变体 |
| 可执行文件中内置了 OpenSSL（官方 Node.js 二进制文件） | 可执行文件中的同名函数 |
| 是 Go 1.20+ 可执行文件 | `crypto/tls` 的 `(*Conn).Write`、`Read` 以及 `Read` 的每个返回点，通过 Go 函数表定位，已剥离符号的二进制文件同样适用 |

之后启动的进程同样会被挂载：agent 在 `exec` 和打开 `libssl` 文件处设有探针，借此发现新进程。使用其他 TLS 栈（如 rustls 或 Java 的 JSSE）的程序目前还观测不到；[覆盖率指标](guides/coverage.md)会列出它们。

## 2. 复制明文 { #2-copy-the-plaintext }

每个探针都是一个 eBPF 程序，在函数被调用或返回时运行。它把明文缓冲区（读取最多 16 KB，写入最多 4 KB）、进程 ID 和 cgroup ID 复制到与 agent 共享的环形缓冲区中。这些程序只读不写，不会改动调用、数据或连接。

通过明文 HTTP 调用的集群内模型服务（如 Ollama 或 vLLM）没有可挂载的 TLS 库。对于你标记为 `plaintext` 的服务商，agent 改为在套接字处复制字节（参见[通过明文 HTTP 访问的自托管模型](guides/plaintext.md)）。

## 3. 把字节还原为调用 { #3-turn-bytes-into-calls }

在用户空间，agent 将每个连接的字节重组为 HTTP/1.1 或 HTTP/2 请求和响应，或 WebSocket 消息（Realtime、Responses、Gemini Live），对其解压（`gzip`、`br`、`zstd`、`deflate`），并跟踪服务器发送事件流直到结束。它根据请求的主机和路径识别服务商，并从响应的用量字段中读取 token 数：提示、补全、缓存读取与写入，以及推理 token。

agent 看到但无法解析的调用不会被静默丢弃：它会连同原因一起计入 `llm_responses_unparsed_total`。

## 4. 找到 Pod { #4-find-the-pod }

从进程的 cgroup 可以得到容器 ID。agent 监视（watch）本节点上的 Pod，据此将容器映射到所在的命名空间、Pod 和所属工作负载，例如 Deployment 或 CronJob。

## 5. 为调用计价 { #5-price-the-call }

agent 用响应中的模型名称查询带日期的内置价格表，再在其上叠加你自己的价格或折扣。调用的成本按服务商的计费方式拆分：输入、缓存读取、缓存写入和输出。没有价格的模型计入 `llm_unpriced_tokens_total`，绝不会记为 $0。

## 6. 导出 { #6-export }

agent 通过 OTLP 以 OpenTelemetry 指标导出 token、请求、延迟和成本，按命名空间、Pod、工作负载和模型打标签；并为每次调用记录一行日志，包含模型、Pod、延迟和 token 数（在 `debug` 级别还包含请求路径）。捕获的字节只保存在 agent 的内存中，解析后即被丢弃：提示词、响应、请求头和 API 密钥绝不会被导出，也不会写入日志。

[安全评审](security-review.md)列出了每个 eBPF 程序、agent 所需的权限，以及它能看到和看不到的内容。

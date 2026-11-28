<!-- translated from docs/en/install.md, git blob 0b2ef7c4e900 -->

# インストール { #install }

!!! note ""
    このページは英語の原文を翻訳したものです。内容が異なる場合は[英語の原文](https://honeybee-studio.github.io/honeybeepf-llm/install/)が正しい内容です。

[クイックスタート](quickstart.md)ではエージェントだけをインストールし、そのログで呼び出しを確認します。このページでは、メトリクスのパイプラインと合わせてインストールします。エージェントが OpenTelemetry メトリクスを Collector に送り、Prometheus がそれをスクレイプし、[Grafana ダッシュボード](grafana/README.md)が Prometheus から読み取ります。

## エージェント、Collector、Prometheus { #agent-collector-and-prometheus }

ノードで Linux 5.10 以降、または RHEL 8.10 / 9.6 / 9.7 のカーネルが動いている Kubernetes クラスター、`kubectl`、Helm 3 が必要です。以下の手順では、最新リリースの v0.5.0 を、同梱の Prometheus および OpenTelemetry Collector と一緒にインストールします。`main` のチャートはリリースイメージより新しいエージェントを前提にしている場合があるため、チャートはリリースタグでチェックアウトします。

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

LLM 呼び出しを発生と同時に確認します。

```bash
kubectl logs -n honeybeepf ds/honeybeepf-llm -f | grep 'LLM SUCCESS'
```

`ds/honeybeepf-llm` が追うのは 1 つのエージェント Pod だけです。複数のノードがあるクラスターでは、各ノードの呼び出しはそのノードのエージェント Pod に出力されます。

```
2026-10-26T08:43:35.510264Z  INFO honeybeepf_llm::probes::builtin::llm::processor::background: LLM SUCCESS | PID: 1686847 | Model: claude-test | Transport: tls | Latency: 0.00s | Tokens: 140 (Prompt: 125, Compl: 15, Cache read: 100, Cache write: 0) | Pod: team-b/support-bot-589db898dd-rv9ts
```

同梱の Prometheus は、`honeybeepf` Namespace 内の `honeybeepf-llm-prometheus-server` Service (ポート 80) です。クエリの例は[メトリクスリファレンス](metrics.md)に、コストダッシュボードの読み込み方は[Grafana ダッシュボード](grafana/README.md)にあります。

## クラスター外の Collector { #a-collector-outside-the-cluster }

クラスター外の Collector にメトリクスを送るには、TLS を使い、Collector が求める場合は認証ヘッダーも設定します。

```bash
kubectl -n honeybeepf create secret generic otel-ca --from-file=ca.crt=./collector-ca.crt   # only for a private CA
kubectl -n honeybeepf create secret generic otel-auth --from-literal=headers='authorization=Bearer <token>'
helm upgrade --install honeybeepf-llm charts/honeybeepf-llm -n honeybeepf --reuse-values \
  --set output.otlp.endpoint=https://otel.example.com:4317 \
  --set output.otlp.caSecret=otel-ca \
  --set output.otlp.headersSecret.name=otel-auth --set output.otlp.headersSecret.key=headers
```

`https://` のエンドポイントは、`caSecret` が設定されていればその CA で、設定されていなければシステムのルート証明書で検証されます。`host:port` と `http://` は平文のままです。

## 権限とリソース { #permissions-and-resources }

エージェントには `hostPID` と、`BPF`、`PERFMON`、`SYS_ADMIN`、`SYS_PTRACE`、`NET_ADMIN`、`SYS_RESOURCE` の各ケーパビリティが必要です。リソース要求はノードごとに 50m CPU / 128Mi (制限は 1 CPU / 1Gi) です。[`values.yaml`](https://github.com/honeybee-studio/honeybeepf-llm/blob/main/charts/honeybeepf-llm/values.yaml) を参照してください。

このうち 2 つのケーパビリティは、古いカーネルでのみ必要です。チャートは 1 回のインストールでどのノードでも動くようにこれらを付与していますが、すべてのノードのカーネルが新しければ削除できます。

| ケーパビリティ | 必要になるノードのカーネル | 理由 | 削除できる条件 (すべてのノードのカーネル) |
|---|---|---|---|
| `NET_ADMIN` | 6.9 より前のカーネル | エージェントは平文キャプチャーが無効でも起動時にソケットマップを作成し、これらのカーネルではそのために `NET_ADMIN` が必要です | 6.9 以降 |
| `SYS_RESOURCE` | 5.11 より前のカーネル | これらのカーネルは BPF マップのメモリーを memlock の上限に含めて数えるため、エージェントは起動時にこの上限を引き上げます | 5.11 以降 |

このリストは設定すると丸ごと置き換わるため、ほかの 4 つは必ず含めてください。たとえば、すべてのノードで 6.9 以降が動いている場合は次のようにします。

```yaml
securityContext:
  capabilities:
    add: [BPF, PERFMON, SYS_ADMIN, SYS_PTRACE]
```

ノードのカーネルが必要とするケーパビリティが欠けていると、エージェントは起動時のマップ作成中に `Operation not permitted` で終了します。

## アンインストール { #uninstall }

```bash
helm uninstall honeybeepf-llm honeybeepf-llm-otel-collector honeybeepf-llm-prometheus -n honeybeepf
kubectl delete namespace honeybeepf
```

リリースをインストールする前に、[署名を検証](verify.md)できます。手元でチェックアウトしたソースからビルドしたイメージを実行するには、[ソースからビルドする](build.md)を参照してください。

<!-- translated from docs/quickstart.md at 250d516 -->

# クイックスタート { #quickstart }

!!! note ""
    このページは英語の原文を翻訳したものです。内容が異なる場合は[英語の原文](https://honeybee-studio.github.io/honeybeepf-llm/quickstart/)が正しい内容です。

クラスター上のすべての Pod の LLM 呼び出しを、数分で確認できます。この手順ではエージェントだけをインストールし、そのログで呼び出しを確認します。[インストール](install.md)では、Prometheus、OpenTelemetry Collector、Grafana のコストダッシュボードも追加します。

## 始める前に { #before-you-start }

- ノードで Linux 5.10 以降、または RHEL 8.10、9.6、9.7 のカーネルが動いている Kubernetes クラスター。
- `kubectl` と Helm 3。
- Python、Node.js、Go、curl など、OpenSSL を使うプログラムから HTTPS で LLM プロバイダーを呼び出す Pod。LLM のトラフィックがまだない場合は、代わりに[デモ](demo.md)を実行してください。

## 1. エージェントをインストールする { #1-install-the-agent }

```bash
kubectl create namespace honeybeepf
helm install honeybeepf-llm oci://ghcr.io/honeybee-studio/charts/honeybeepf-llm -n honeybeepf
```

このチャートは、DaemonSet としてノードごとに 1 つのエージェント Pod を実行します。アプリやその SDK、API キーは変更不要です。

## 2. 呼び出しを確認する { #2-watch-the-calls }

```bash
kubectl logs -n honeybeepf ds/honeybeepf-llm -f | grep 'LLM SUCCESS'
```

LLM 呼び出し 1 件につき 1 行が出力され、モデル、トークン数、呼び出し元の Pod が含まれます。

```
LLM SUCCESS | PID: 1686847 | Model: claude-test | Transport: tls | Latency: 0.00s | Tokens: 140 (Prompt: 125, Compl: 15, Cache read: 100, Cache write: 0) | Pod: team-b/support-bot-589db898dd-rv9ts
```

`ds/honeybeepf-llm` が追うのは 1 つのエージェント Pod だけです。複数のノードがあるクラスターでは、各ノードの呼び出しはそのノードのエージェント Pod に出力されます。すべてを追うには `kubectl logs -n honeybeepf -l app.kubernetes.io/name=honeybeepf-llm --prefix -f` を使います。

行が出力されない場合は、[トラブルシューティング](troubleshooting.md)を参照してください。

## 3. 次のステップ { #3-next }

- [メトリクスを Prometheus に送り、コストダッシュボードを開く](install.md)。
- [どのワークロードを観測できているかを確認する](guides/coverage.md)。
- [呼び出しの価格の計算方法を確認し](cost.md)、[独自の価格を追加する](guides/prices.md)。

削除するには次のコマンドを実行します。

```bash
helm uninstall honeybeepf-llm -n honeybeepf
kubectl delete namespace honeybeepf
```

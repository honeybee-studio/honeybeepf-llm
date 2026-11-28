---
title: LLM の請求額をチーム別に
description: LLM の請求額をチーム別に分けます。Kubernetes の Namespace、Pod、モデルごとのトークン数と推定コストを eBPF で読み取ります。コードの変更、プロキシ、SDK は不要です。
hide:
  - navigation
  - toc
---

<!-- translated from docs/en/index.md, git blob 16eafce5ce85 -->

<div class="hb-hero" markdown>

<div class="hb-hero__text" markdown>

<h1>LLM の請求額を、<em>チーム別に。</em></h1>

<p class="hb-sub">プロバイダーから届くのは合計額だけです。honeybeepf-llm は、その金額をどの Namespace、Pod、モデルが使ったのかを、アプリに手を加えずに示します。</p>

[クイックスタート :material-arrow-right:](quickstart.md){ .md-button .md-button--primary }
[GitHub :fontawesome-brands-github:](https://github.com/honeybee-studio/honeybeepf-llm){ .md-button }

<p class="hb-meta">オープンソース、Apache-2.0 · Kubernetes 向けの eBPF DaemonSet · OpenTelemetry メトリクス</p>

</div>

<div class="hb-receipt-wrap">
<div class="hb-receipt" data-hb-live role="img" aria-label="デモクラスターの LLM 呼び出しを発生と同時に印字し、チームごとの小計を示すレシート: team-b $1.96、team-a $0.85、team-c $0.17、合計 $2.98">
  <div class="hb-receipt__head">
    <strong>honeybeepf-llm</strong>
    <span>クラスター demo · ノード worker-1</span>
    <span class="hb-receipt__live"><i></i>リアルタイムで印字中</span>
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
  <div class="hb-receipt__total"><span>合計</span><span data-hb-total>$2.98</span></div>
  <div class="hb-receipt__foot">変更したアプリケーションコード: 0 行<br>デモワークロードによる模擬的な呼び出し</div>
</div>
</div>

</div>

## 導入の流れ { #how-you-get-it }

<div class="hb-steps" markdown>

<div class="hb-step" markdown>
<div class="hb-step__art hb-step__art--code">$ helm install honeybeepf-llm \
    oci://ghcr.io/honeybee-studio/\
    charts/honeybeepf-llm

<span>DaemonSet の準備完了: ノードごとに 1 エージェント</span></div>
<div class="hb-step__n">1</div>
<h3>DaemonSet を 1 つインストール</h3>

ノードごとに 1 つのエージェントが動きます。アプリ、SDK、API キーはそのままです。
</div>

<div class="hb-step" markdown>
<div class="hb-step__art hb-step__art--log"><span>pod</span>    team-b/support-bot
<span>host</span>   api.anthropic.com
<span>model</span>  claude-sonnet-4-5
<span>tokens</span> 1,840  <b>$0.011</b></div>
<div class="hb-step__n">2</div>
<h3>すべての LLM 呼び出しを読み取る</h3>

各呼び出しは、アプリが TLS に渡す時点でカーネル内で読み取られ、呼び出し元の Pod と対応付けられます。
</div>

<div class="hb-step" markdown>
<div class="hb-step__art hb-step__art--shot" markdown>![Grafana パネル: ワークロードごとの推定支出。team-b support-bot $1.96、team-a chat-api $0.85、team-c summarizer $0.17](../images/grafana-cost-narrow.png)</div>
<div class="hb-step__n">3</div>
<h3>チームごとのコストを確認する</h3>

Namespace、Pod、モデルごとのトークン数と金額を Prometheus で確認できます。[Grafana ダッシュボード](grafana/README.md)も付属しています。
</div>

</div>

## 試してみる { #try-it }

=== "Helm"

    Linux 5.10 以降のノードを持つ Kubernetes クラスター、`kubectl`、Helm 3 が必要です。

    ```bash
    kubectl create namespace honeybeepf
    helm install honeybeepf-llm oci://ghcr.io/honeybee-studio/charts/honeybeepf-llm -n honeybeepf
    kubectl logs -n honeybeepf ds/honeybeepf-llm -f | grep 'LLM SUCCESS'
    ```

    Grafana のコストダッシュボードを使うには、同梱の Prometheus と OpenTelemetry Collector を追加します。[インストール](install.md)を参照してください。

=== "デモ (API キー不要)"

    モックのプロバイダー、3 つのチーム Namespace 内の LLM クライアント、Grafana を任意のクラスター (kind、k3d、minikube) で動かします。

    ```bash
    git clone https://github.com/honeybee-studio/honeybeepf-llm.git && cd honeybeepf-llm
    examples/demo/demo.sh up
    kubectl -n monitoring port-forward svc/grafana 3000:80
    ```

    その後、<http://localhost:3000> で「LLM Cost Observability」ダッシュボードを開きます。

## ゲートウェイや SDK ではない理由 { #why-not-a-gateway-or-an-sdk }

<div class="hb-compare" markdown>

| | ゲートウェイ / プロキシ | SDK による計装 | honeybeepf-llm (eBPF) |
|---|---|---|---|
| アプリの変更 | 新しいベース URL | コードの追加と再デプロイ | なし |
| リクエスト経路上にあるか | はい | プロセス内 | いいえ: バッファーのコピーを読む |
| 見える呼び出し | 経由した呼び出しのみ | 計装したコードのみ | ノード上のすべての Pod |
| API キー | ゲートウェイを通過する | アプリのプロセス内 | ログに記録しない (パス、モデル、トークン数のみ) |

</div>

## 仕組み { #how-it-works }

![SSL_write/SSL_read の uprobe が平文をリングバッファーにコピーし、エージェントが HTTP を解析してプロバイダーを判定し、cgroup を Pod に対応付けて OTLP メトリクスをエクスポートする](../images/how-it-works.svg)

1. エージェントは、プロセスがマップするすべての `libssl` の `SSL_write` と `SSL_read`、および Go 実行ファイルの `crypto/tls` に uprobe をアタッチします。
2. uprobe は、平文のバッファー、pid、cgroup ID を BPF リングバッファーにコピーします。
3. ユーザー空間では、HTTP/1.1、HTTP/2、SSE のパーサーが各呼び出しを再構成し、ホストとパスからプロバイダーを判定して、レスポンスの usage フィールドを読み取ります。
4. cgroup からプロセスを Pod に対応付け、結果を OpenTelemetry メトリクスとしてエクスポートします。

## 次に読む { #read-next }

<div class="hb-index" markdown>

- [クイックスタート](quickstart.md): エージェントをインストールし、数分で呼び出しを確認します。
- [仕組み](how-it-works.md): TLS バッファーから Pod ごとの金額までを順を追って説明します。
- [サポート対象](providers.md): プロバイダー、ランタイム、プロトコル、カーネル。
- [観測できていないワークロードを見つける](guides/coverage.md): 1 つのクエリで一覧にできます。
- [プロバイダーを追加する](guides/custom-providers.md)、または[独自の価格を設定する](guides/prices.md)。
- [メトリクス](metrics.md): すべてのメトリクスとラベル、および呼び出しが記録されなかった理由。
- [予算とアラート](alerts.md): チャートに同梱されている、支出とデータ品質のルール。
- [トラブルシューティング](troubleshooting.md)と[よくある質問](faq.md)。
- [セキュリティレビュー](security-review.md): カーネルフック、権限、データの扱い、既知の制限。
- [オーバーヘッドのベンチマーク](benchmark.md): エージェントの有無による CPU、メモリ、スループットの比較。

</div>

!!! note ""
    このページは英語の原文を翻訳したものです。内容が異なる場合は[英語の原文](https://honeybee-studio.github.io/honeybeepf-llm/)が正しい内容です。

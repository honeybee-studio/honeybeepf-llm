<!-- translated from docs/cost.md at 250d516 -->

# コストと価格 { #cost-and-prices }

!!! note ""
    このページは英語の原文を翻訳したものです。内容が異なる場合は[英語の原文](https://honeybee-studio.github.io/honeybeepf-llm/cost/)が正しい内容です。

記録された呼び出しにはすべて、USD 建ての推定コストが付きます。トークン数にモデルの単価を掛けた額です。

## 価格の参照元 { #where-prices-come-from }

- 日付付きの**組み込み価格表**。レスポンス内のモデル名で照合します。`claude-sonnet-4-5@20250929` のような日付付きの名前は、そのモデルに一致します。
- その上に重ねる**[独自の価格](guides/prices.md)**。価格表にないモデルや、契約による割引に使います。
- 価格のないモデルが **$0 として記録されることはありません**。そのトークンは代わりに `llm_unpriced_tokens_total` に計上されます。

**価格の選び方**

| | |
|---|---|
| 基準 | ホストではなくモデル名。Azure OpenAI、Bedrock、Vertex AI、リージョン別のエンドポイントは、そのプロバイダー自身の API と同じ価格で計算されるため、OpenAI のデータレジデンシー (+10%) のようなリージョン別の追加料金は加算されません |
| 価格 | 各プロバイダーが公開している定価。プロバイダーごとのエントリーに、価格ページと確認した日付が記載されています |
| 含まれないもの | Web 検索など、リクエスト単位のツール料金 |

合計を信頼する前に、コストを `llm_unpriced_tokens_total` と比べてください。

!!! note
    コストのメトリクスは、価格表が読み込まれた場合にのみ出力されます。組み込みの価格表が受け付けられなかった場合も、警告がログに出るだけで、エージェントはコストのメトリクスなしで動作を続けます。

## ワークロードごとの支出 { #spend-per-workload }

```promql
sum by (target_namespace, target_workload_name) (rate(honeybeepf_llm_llm_cost_usd_total[5m])) * 60
```

Namespace とワークロードごとの、1 分あたりの USD です。

## ゲートウェイを経由する呼び出し { #calls-through-a-gateway }

クラスター内のゲートウェイ (LiteLLM や社内プロキシ) を経由する呼び出しは、アプリからゲートウェイへ、ゲートウェイからプロバイダーへと 2 回行われます。エージェントが両方を記録する場合は、2 つのレコードを区別できるように、ゲートウェイを `gateway: true` 付きの[プロバイダー](guides/custom-providers.md)として登録します。

| レコード | ラベル | 用途 |
|---|---|---|
| アプリ → ゲートウェイ | アプリの Pod、`hop="gateway"` | チームやワークロードごとの支出 |
| ゲートウェイ → プロバイダー | ゲートウェイの Pod、`hop` なし | 総支出: プロバイダー自身のモデルと使用量 |

各ワークロードの時系列はそのワークロード自身の呼び出しなので、Namespace やワークロードごとの支出にはフィルターは不要で、ゲートウェイはワークロードの 1 つとして表示されます。すべてのワークロードを合計するときは、ゲートウェイのホップを除けば各呼び出しを 1 回だけ数えられます。

```promql
sum(increase(honeybeepf_llm_llm_cost_usd_total{hop!="gateway"}[1d]))
```

同梱のコストダッシュボードは、合計と、モデル別、モダリティ別、オペレーション別のパネルでこの除外を適用しています。

!!! warning
    `gateway: true` は、このエージェントがプロバイダーへの呼び出しを記録できるゲートウェイにだけ設定してください。つまり、クラスターのノード上で動き、TLS または `plaintext` エントリーでプロバイダーを呼び出すゲートウェイです。クラスター外のゲートウェイでは、記録されるのはそのレコードだけなので、上のフィルターを使うとそれが除外されてしまいます。

## コストのメトリクス { #the-cost-metrics }

| メトリクス | 数える対象 | 追加のラベル |
|---|---|---|
| `honeybeepf_llm_llm_cost_usd_total` | 各呼び出しのコスト (USD) | `cost_type`、`price_table_version` |
| `honeybeepf_llm_llm_cache_savings_ceiling_usd_total` | プロンプトキャッシュで節約できた可能性のある最大額 | `cache_price`、`price_table_version` |
| `honeybeepf_llm_llm_unpriced_tokens_total` | 価格を付けられなかった呼び出しのトークン数 | `reason`、`price_table_version` |

3 つとも呼び出しごとのラベル (`model`、`status`、`attempt`、`target_namespace`、`target_pod_name`、`target_workload_kind`、`target_workload_name`) を持ちますが、`transport` と `operation` は持ちません。USD のカウンターには単位がないため、エクスポーターは名前に単位を付加しません。

### `cost_type`: コストの内訳 { #cost_type-how-the-cost-is-split }

プロバイダーの請求と同じ分け方です。ゼロの項目は記録されないため、各呼び出しには実際に使った分の時系列だけができます。

| `cost_type` | 課金対象 |
|---|---|
| `input` | キャッシュから読み取られず、キャッシュにも書き込まれなかったプロンプトトークン |
| `cache_read` | プロバイダーのキャッシュから提供されたプロンプトトークン |
| `cache_write` | プロバイダーのキャッシュに書き込まれたプロンプトトークン |
| `output` | すべての補完トークン (推論トークンを含む) |

### `attempt`: 処理されたか、見送られたか { #attempt-served-or-declined }

| `attempt` | 呼び出しの状態 |
|---|---|
| `served` | プロバイダーが処理した |
| `declined` | サーバー側フォールバックで先に行われた試行のうち、プロバイダーが課金したうえで別の処理先に回したもの。トークン数とコストのみで、リクエスト数やレイテンシーは記録しない |

エラーとして記録された呼び出しには、コストは記録されません。

### `price_table_version` { #price_table_version }

組み込み価格表の `version` です。独自の価格がマージされている場合は `+custom-<hash>` が付加されます。価格が変わると新しい時系列として現れます。

### キャッシュによる節約の上限 { #cache-savings-ceiling }

呼び出しのキャッシュされなかった入力トークンを、入力価格ではなくモデルのキャッシュ読み取り価格で計算したものです。プロンプトキャッシュで節約できる額の上限を示します。価格が付いた呼び出しのうち embeddings 以外を対象に、値がゼロより大きい場合にのみ出力されます。

| `cache_price` | 意味 |
|---|---|
| `known` | モデルにキャッシュ読み取り価格がある |
| `assumed_free` | キャッシュ読み取り価格がないため、上限は入力コストの全額になる |

### 価格のないトークン { #unpriced-tokens }

| `reason` | 呼び出しに価格を付けられなかった理由 |
|---|---|
| `unknown_model` | モデル名がない、または価格表にない |
| `missing_price` | 呼び出しに必要な価格がない、または適用期限を過ぎている |
| `unsupported_modifier` | 呼び出しのサービスティア、速度、地域の修飾子に価格がない |
| `inconsistent_usage` | 報告されたキャッシュのトークン数が、それを含むはずのプロンプトの合計を超えている |

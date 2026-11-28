<!-- translated from docs/faq.md at 250d516 -->

# よくある質問 { #faq }

!!! note ""
    このページは英語の原文を翻訳したものです。内容が異なる場合は[英語の原文](https://honeybee-studio.github.io/honeybeepf-llm/faq/)が正しい内容です。

## アプリを変更する必要はありますか? { #do-i-have-to-change-my-apps }

いいえ。アプリはこれまでどおり、自身の SDK とキーでプロバイダーを直接呼び出します。変更するベース URL も、インポートするライブラリも、再デプロイするものもありません。[仕組み](how-it-works.md)を参照してください。

## リクエスト経路に割り込んだり、呼び出しが遅くなったりしますか? { #does-it-sit-in-the-request-path-or-slow-my-calls-down }

いいえ。エージェントはノード上で TLS バッファーのコピーを読み取ります。リクエストはこれまでとまったく同じようにプロバイダーへ送られ、エージェントを待つ処理はありません。エージェントが使う CPU とメモリー、および負荷時のスループットへの影響は、[オーバーヘッドのベンチマーク](benchmark.md)で測定しています。

## プロンプトや API キーを読み取りますか? { #does-it-read-my-prompts-and-api-keys }

TLS を読み取るツールである以上、プローブは各呼び出しの復号されたバイト列を、ノード上のエージェントのメモリーにコピーします。エージェントはそれを解析して usage フィールドを取り出し、その後破棄します。プロンプト、レスポンス、ヘッダー、API キーがエクスポートされたりログに記録されたりすることはありません。何がエクスポートされ、何がログに記録されるかは、[セキュリティレビュー](security-review.md#3-data-handling)にフィールド単位で記載しています。

## 数値はどのくらい正確ですか? { #how-accurate-are-the-numbers }

トークン数はプロバイダー自身が報告した値です。エージェントは各レスポンスの usage フィールドを読み取り、キャッシュと推論のトークンも含めます。コストは推定値で、これらのトークン数に日付付きの価格表を掛けたものです。価格表には契約価格を重ねられます ([コストと価格](cost.md))。エージェントが捉えたのに読み取れなかった呼び出しも、黙って破棄されることはなく、理由とともに計上されます。

## どのプロバイダー、言語、プロトコルに対応していますか? { #which-providers-languages-and-protocols-does-it-cover }

OpenAI、Anthropic、Gemini、Bedrock、Azure OpenAI、Vertex AI、Meta と、多くの OpenAI 互換 API に対応しています。Python、Node.js、Go、curl など OpenSSL を使うあらゆるプログラムからの呼び出しを、HTTP/1.1 と HTTP/2 (ストリーミングを含む) で、また OpenAI Realtime、Responses、Gemini Live は WebSocket で扱います。rustls、Java、その他の WebSocket API、HTTP/3 にはまだ対応していません。対応表の全体は[サポート対象](providers.md)にあり、クラスター内で観測できていないワークロードは[メトリクス](guides/coverage.md)で一覧できます。

## すでに LiteLLM などの LLM ゲートウェイを運用しています。これは必要ですか? { #we-already-run-an-llm-gateway-such-as-litellm-do-we-need-this }

ゲートウェイから見えるのは、そこを経由した呼び出しだけです。エージェントは、ゲートウェイを迂回する呼び出しも含めてノードから出るすべての呼び出しを捉え、アプリを変更せずに呼び出し元の Pod を特定します。[形状による検出](guides/detect-by-shape.md)を有効にすると、クラスター内のゲートウェイは 2 回記録されます (アプリからゲートウェイ、ゲートウェイからプロバイダー)。1 回だけ数える方法はそのページで説明しています。

## ノードには何が必要ですか? { #what-does-it-need-on-the-nodes }

x86-64 または arm64 で、BTF が有効な Linux 5.10 以降、または RHEL 8.10、9.6、9.7 のカーネルが必要です。エージェントは特権コンテナではありません。`hostPID` と限られたケーパビリティだけで動作します。詳細は[インストール](install.md#permissions-and-resources)と[セキュリティレビュー](security-review.md#2-privileges)で説明しています。

## Kubernetes なしでも動きますか? { #does-it-work-without-kubernetes }

はい。`k8s` フィーチャーなしでビルドしたバイナリーとして、Linux ホスト上で動きます。その場合、メトリクスには Namespace、Pod、ワークロードのラベルが付きません。[Kubernetes なしで実行する](run-binary.md)を参照してください。

## バグの報告やプロバイダーの追加の依頼はどこでできますか? { #where-do-i-report-a-bug-or-ask-for-a-provider }

[GitHub Issues](https://github.com/honeybee-studio/honeybeepf-llm/issues) で受け付けています。

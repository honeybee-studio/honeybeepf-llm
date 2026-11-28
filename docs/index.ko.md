---
title: 팀별로 나눈 LLM 청구서
description: 팀별로 나눈 LLM 청구서. eBPF로 읽은 Kubernetes 네임스페이스, 파드, 모델별 토큰과 예상 비용. 코드 변경, 프록시, SDK가 필요 없습니다.
hide:
  - navigation
  - toc
---
<!-- translated from docs/index.md at 250d516 -->

<div class="hb-hero" markdown>

<div class="hb-hero__text" markdown>

<h1>LLM 청구서를 <em>팀별로 나눕니다.</em></h1>

<p class="hb-sub">프로바이더는 합계 하나만 보냅니다. honeybeepf-llm은 앱을 건드리지 않고, 어느 네임스페이스와 파드, 모델이
그 비용을 썼는지 보여 줍니다.</p>

[빠른 시작 :material-arrow-right:](quickstart.md){ .md-button .md-button--primary }
[GitHub :fontawesome-brands-github:](https://github.com/honeybee-studio/honeybeepf-llm){ .md-button }

<p class="hb-meta">오픈 소스, Apache-2.0 · Kubernetes용 eBPF 데몬셋 · OpenTelemetry 메트릭</p>

</div>

<div class="hb-receipt-wrap">
<div class="hb-receipt" data-hb-live role="img" aria-label="데모 클러스터에서 일어나는 LLM 호출을 실시간으로 찍는 영수증과 팀별 소계: team-b $1.96, team-a $0.85, team-c $0.17, 합계 $2.98">
  <div class="hb-receipt__head">
    <strong>honeybeepf-llm</strong>
    <span>클러스터 demo · 노드 worker-1</span>
    <span class="hb-receipt__live"><i></i>실시간 출력 중</span>
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
  <div class="hb-receipt__total"><span>합계</span><span data-hb-total>$2.98</span></div>
  <div class="hb-receipt__foot">변경한 애플리케이션 코드: 0줄<br>데모 워크로드의 모의 호출</div>
</div>
</div>

</div>

## 이렇게 얻습니다 { #how-you-get-it }

<div class="hb-steps" markdown>

<div class="hb-step" markdown>
<div class="hb-step__art hb-step__art--code">$ helm install honeybeepf-llm \
    oci://ghcr.io/honeybee-studio/\
    charts/honeybeepf-llm

<span>DaemonSet ready: 1 agent per node</span></div>
<div class="hb-step__n">1</div>
<h3>데몬셋 하나 설치</h3>

노드마다 에이전트가 하나씩 실행됩니다. 앱, SDK, API 키는 그대로 둡니다.
</div>

<div class="hb-step" markdown>
<div class="hb-step__art hb-step__art--log"><span>pod</span>    team-b/support-bot
<span>host</span>   api.anthropic.com
<span>model</span>  claude-sonnet-4-5
<span>tokens</span> 1,840  <b>$0.011</b></div>
<div class="hb-step__n">2</div>
<h3>모든 LLM 호출을 읽습니다</h3>

앱이 호출을 TLS에 넘기는 지점에서 커널이 각 호출을 읽고, 호출한 파드와 연결합니다.
</div>

<div class="hb-step" markdown>
<div class="hb-step__art hb-step__art--shot"><img src="images/grafana-cost-narrow.png" alt="Grafana 패널: 워크로드별 예상 지출, team-b support-bot $1.96, team-a chat-api $0.85, team-c summarizer $0.17"></div>
<div class="hb-step__n">3</div>
<h3>팀별 비용 확인</h3>

네임스페이스, 파드, 모델별 토큰과 달러 금액을 Prometheus에서 봅니다. [Grafana 대시보드](grafana/README.md)도 포함되어 있습니다.
</div>

</div>

## 사용해 보기 { #try-it }

=== "Helm"

    Linux 5.10+ 노드로 구성된 Kubernetes 클러스터, `kubectl`, Helm 3이 필요합니다.

    ```bash
    kubectl create namespace honeybeepf
    helm install honeybeepf-llm oci://ghcr.io/honeybee-studio/charts/honeybeepf-llm -n honeybeepf
    kubectl logs -n honeybeepf ds/honeybeepf-llm -f | grep 'LLM SUCCESS'
    ```

    Grafana 비용 대시보드를 쓰려면 함께 제공되는 Prometheus와 OpenTelemetry Collector를 추가합니다. [설치](install.md)를 참고하세요.

=== "데모, API 키 불필요"

    어떤 클러스터(kind, k3d, minikube)에서든 모의 프로바이더, 세 팀 네임스페이스의 LLM 클라이언트, Grafana를 실행합니다.

    ```bash
    git clone https://github.com/honeybee-studio/honeybeepf-llm.git && cd honeybeepf-llm
    examples/demo/demo.sh up
    kubectl -n monitoring port-forward svc/grafana 3000:80
    ```

    그런 다음 <http://localhost:3000>에서 "LLM Cost Observability" 대시보드를 엽니다.

## 왜 게이트웨이나 SDK가 아닌가요? { #why-not-a-gateway-or-an-sdk }

<div class="hb-compare" markdown>

| | 게이트웨이 / 프록시 | SDK 계측 | honeybeepf-llm (eBPF) |
|---|---|---|---|
| 앱 변경 | 새 base URL | 코드 추가, 재배포 | 없음 |
| 요청 경로에 위치 | 예 | 프로세스 내부 | 아니요: 버퍼 사본을 읽음 |
| 보이는 호출 | 거쳐 가는 호출만 | 계측한 코드의 호출만 | 노드의 모든 파드 |
| API 키 | 게이트웨이를 지나감 | 앱 프로세스 안에 있음 | 로그에 남기지 않음(경로, 모델, 토큰만) |

</div>

## 동작 원리 { #how-it-works }

![SSL_write/SSL_read의 uprobe가 평문을 링 버퍼에 복사하고, 에이전트가 HTTP를 파싱해 프로바이더를 찾고 cgroup을 파드에 매핑한 뒤 OTLP 메트릭을 내보냄](images/how-it-works.svg)

1. 에이전트는 프로세스가 매핑한 모든 `libssl`의 `SSL_write`와 `SSL_read`에, 그리고 Go 실행 파일의 `crypto/tls`에
   uprobe를 붙입니다.
2. uprobe는 평문 버퍼, pid, cgroup id를 BPF 링 버퍼에 복사합니다.
3. 사용자 공간에서는 HTTP/1.1, HTTP/2, SSE 파서가 각 호출을 다시 조립하고, 호스트와 경로로 프로바이더를 고르고,
   응답에서 usage 필드를 읽습니다.
4. cgroup으로 프로세스를 해당 파드에 매핑하고, 결과를 OpenTelemetry 메트릭으로 내보냅니다.

## 다음 읽을거리 { #read-next }

<div class="hb-index" markdown>

- [빠른 시작](quickstart.md): 에이전트를 설치하고 몇 분 안에 호출을 확인합니다.
- [동작 원리](how-it-works.md): TLS 버퍼에서 파드별 달러 금액까지 단계별로 설명합니다.
- [지원 범위](providers.md): 프로바이더, 런타임, 프로토콜, 커널.
- [보이지 않는 워크로드 찾기](guides/coverage.md): 쿼리 하나로 목록을 얻습니다.
- [프로바이더 추가](guides/custom-providers.md) 또는 [직접 정한 가격 추가](guides/prices.md).
- [메트릭](metrics.md): 모든 메트릭과 레이블, 그리고 호출이 기록되지 않은 이유.
- [예산과 알림](alerts.md): 차트에 포함된 지출 규칙과 데이터 품질 규칙.
- [문제 해결](troubleshooting.md)과 [자주 묻는 질문](faq.md).
- [보안 검토](security-review.md): 커널 훅, 권한, 데이터 처리, 알려진 한계.
- [오버헤드 벤치마크](benchmark.md): 에이전트가 있을 때와 없을 때의 CPU, 메모리, 처리량.

</div>

!!! note ""
    영어 원문을 옮긴 페이지입니다. 내용이 다르면 [영어 원문](https://honeybee-studio.github.io/honeybeepf-llm/)이 맞습니다.

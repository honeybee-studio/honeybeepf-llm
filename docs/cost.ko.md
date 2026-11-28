<!-- translated from docs/cost.md at 250d516 -->

# 비용과 가격 { #cost-and-prices }

!!! note ""
    영어 원문을 옮긴 페이지입니다. 내용이 다르면 [영어 원문](https://honeybee-studio.github.io/honeybeepf-llm/cost/)이 맞습니다.

기록된 모든 호출에는 USD 단위의 예상 비용이 붙습니다. 토큰 수에 모델 가격을 곱한 값입니다.

## 가격의 출처 { #where-prices-come-from }

- 날짜가 붙은 **내장 가격표**로, 응답의 모델 이름으로 매칭합니다. `claude-sonnet-4-5@20250929`처럼 날짜가 붙은
  이름도 해당 모델에 매칭됩니다.
- 그 위에 적용하는 **[직접 정한 가격](guides/prices.md)**: 가격표에 없는 모델이나 계약 할인.
- 가격이 없는 모델은 **$0으로 기록되지 않습니다**. 대신 그 토큰은 `llm_unpriced_tokens_total`로 갑니다.

**가격을 고르는 방식**

| | |
|---|---|
| 기준 | 호스트가 아니라 모델 이름. Azure OpenAI, Bedrock, Vertex AI, 리전 엔드포인트도 프로바이더 자체 API 가격으로 계산하므로, OpenAI 데이터 레지던시(+10%) 같은 리전 할증은 더하지 않음 |
| 가격 | 프로바이더가 공개한 정가. 프로바이더 항목마다 가격 페이지와 확인한 날짜가 적혀 있음 |
| 포함하지 않음 | 웹 검색 같은 요청당 도구 요금 |

합계를 믿기 전에 비용을 `llm_unpriced_tokens_total`과 비교하세요.

!!! note
    비용 메트릭은 가격표가 로드되었을 때만 나타납니다. 내장 가격표가 거부되면 경고만 로그에 남기고, 에이전트는 비용
    메트릭 없이 실행됩니다.

## 워크로드별 지출 { #spend-per-workload }

```promql
sum by (target_namespace, target_workload_name) (rate(honeybeepf_llm_llm_cost_usd_total[5m])) * 60
```

네임스페이스와 워크로드별 분당 USD입니다.

## 게이트웨이를 거치는 호출 { #calls-through-a-gateway }

클러스터 안의 게이트웨이(LiteLLM, 사내 프록시)를 거치는 호출은 두 번 일어납니다. 앱이 게이트웨이를 호출하고,
게이트웨이가 프로바이더를 호출합니다. 에이전트가 둘 다 기록한다면, 두 기록을 구분할 수 있도록 게이트웨이를
[프로바이더](guides/custom-providers.md)로 등록하고 `gateway: true`를 지정합니다.

| 기록 | 레이블 | 용도 |
|---|---|---|
| 앱 → 게이트웨이 | 앱의 파드, `hop="gateway"` | 팀별 또는 워크로드별 지출 |
| 게이트웨이 → 프로바이더 | 게이트웨이의 파드, `hop` 없음 | 총 지출: 프로바이더 자체의 모델과 사용량 |

각 워크로드의 시리즈는 그 워크로드 자신의 호출이므로, 네임스페이스별이나 워크로드별 지출에는 필터가 필요 없고,
게이트웨이는 워크로드 하나로 더 나타납니다. 모든 워크로드에 걸친 합계는 게이트웨이 홉을 빼면 각 호출을 한 번씩만
셉니다.

```promql
sum(increase(honeybeepf_llm_llm_cost_usd_total{hop!="gateway"}[1d]))
```

함께 제공되는 비용 대시보드는 합계 패널과 모델별, 모달리티별, 작업별 패널에서 이렇게 계산합니다.

!!! warning
    `gateway: true`는 이 에이전트가 프로바이더 호출을 기록하는 게이트웨이에만 설정하세요. 즉 클러스터 노드에서
    실행되고, TLS 또는 `plaintext` 항목으로 프로바이더를 호출하는 게이트웨이입니다. 클러스터 밖의 게이트웨이라면 그
    기록이 유일한 기록이므로, 위 필터가 그 기록을 빼 버립니다.

## 비용 메트릭 { #the-cost-metrics }

| 메트릭 | 세는 대상 | 추가 레이블 |
|---|---|---|
| `honeybeepf_llm_llm_cost_usd_total` | 각 호출의 비용(USD) | `cost_type`, `price_table_version` |
| `honeybeepf_llm_llm_cache_savings_ceiling_usd_total` | 프롬프트 캐싱으로 절약할 수 있었던 최대 금액 | `cache_price`, `price_table_version` |
| `honeybeepf_llm_llm_unpriced_tokens_total` | 가격을 매길 수 없었던 호출의 토큰 | `reason`, `price_table_version` |

세 메트릭 모두 호출별 레이블(`model`, `status`, `attempt`, `target_namespace`, `target_pod_name`,
`target_workload_kind`, `target_workload_name`)을 갖지만, `transport`와 `operation`은 갖지 않습니다. USD 카운터에는
단위가 없으므로 익스포터가 이름에 단위를 덧붙이지 않습니다.

### `cost_type`: 비용을 나누는 방식 { #cost_type-how-the-cost-is-split }

프로바이더 청구서와 같은 방식입니다. 0인 항목은 추가하지 않으므로, 호출에는 실제로 쓴 시리즈만 있습니다.

| `cost_type` | 청구 대상 |
|---|---|
| `input` | 캐시에서 읽지도, 캐시에 쓰지도 않은 프롬프트 토큰 |
| `cache_read` | 프로바이더 캐시에서 제공된 프롬프트 토큰 |
| `cache_write` | 프로바이더 캐시에 쓴 프롬프트 토큰 |
| `output` | 추론 토큰을 포함한 모든 출력(completion) 토큰 |

### `attempt`: 처리됨 또는 거절됨 { #attempt-served-or-declined }

| `attempt` | 호출 |
|---|---|
| `served` | 프로바이더가 처리함 |
| `declined` | 프로바이더가 요금은 청구했지만 다른 곳으로 라우팅한, 앞선 서버 측 폴백 시도. 토큰과 비용만 있고 요청 수나 지연 시간은 없음 |

오류로 기록된 호출에는 비용을 기록하지 않습니다.

### `price_table_version` { #price_table_version }

내장 가격표의 `version`이며, 직접 정한 가격을 그 위에 병합하면 `+custom-<hash>`가 붙습니다. 가격이 바뀌면 새
시리즈로 나타납니다.

### 캐시 절감 상한 { #cache-savings-ceiling }

호출의 캐시되지 않은 입력 토큰을 입력 가격 대신 모델의 캐시 읽기 가격으로 계산한 값으로, 프롬프트 캐싱으로 절약할
수 있는 금액의 상한입니다. 임베딩이 아니면서 가격이 매겨진 호출에 대해서만, 그리고 0보다 클 때만 내보냅니다.

| `cache_price` | 의미 |
|---|---|
| `known` | 모델에 캐시 읽기 가격이 있음 |
| `assumed_free` | 캐시 읽기 가격이 없으므로 상한은 입력 비용 전체 |

### 가격 없는 토큰 { #unpriced-tokens }

| `reason` | 호출에 가격을 매길 수 없었던 이유 |
|---|---|
| `unknown_model` | 모델 이름이 없거나 가격표에 없음 |
| `missing_price` | 호출에 필요한 가격이 없거나 적용 마지막 날이 지남 |
| `unsupported_modifier` | 호출의 서비스 티어, 속도, 지역 수정자에 가격이 없음 |
| `inconsistent_usage` | 보고된 캐시 토큰 수가 그 수를 포함하는 프롬프트 합계보다 큼 |

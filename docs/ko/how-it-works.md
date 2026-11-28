<!-- translated from docs/en/how-it-works.md, git blob 4df014123bcb -->

# 동작 원리 { #how-it-works }

!!! note ""
    영어 원문을 옮긴 페이지입니다. 내용이 다르면 [영어 원문](https://honeybee-studio.github.io/honeybeepf-llm/how-it-works/)이 맞습니다.

LLM 프로바이더를 호출하는 앱은 요청을 평문으로 TLS 라이브러리에 넘기고, 응답도 TLS 라이브러리에서 평문으로
돌려받습니다. honeybeepf-llm은 노드에서 eBPF로 바로 그 지점의 바이트 사본을 읽습니다. 앱, 앱의 SDK, 프로바이더까지의
네트워크 경로는 그대로입니다.

![SSL_write/SSL_read의 uprobe가 평문을 링 버퍼에 복사하고, 에이전트가 HTTP를 파싱해 프로바이더를 찾고 cgroup을 파드에 매핑한 뒤 OTLP 메트릭을 내보냄](../images/how-it-works.svg)

## 1. TLS 코드 찾기 { #1-find-the-tls-code }

에이전트는 노드마다 파드 하나씩, 데몬셋으로 실행됩니다. 에이전트는 자기 노드의 모든 프로세스를 살펴보고, 각 프로세스가
TLS를 처리하는 위치를 찾습니다.

| 프로세스 | 에이전트가 붙는 위치 |
|---|---|
| 공유 `libssl.so`를 매핑함(Python, 배포판 패키지의 Node.js, curl) | 그 라이브러리의 `SSL_read`, `SSL_write`와 각각의 `_ex` 변형 |
| 실행 파일에 OpenSSL이 내장되어 있음(공식 Node.js 바이너리) | 실행 파일 안의 같은 함수 |
| Go 1.20+ 실행 파일임 | `crypto/tls` `(*Conn).Write`, `Read`, `Read`의 각 반환 지점. Go 함수 테이블에서 찾으며, 스트립된 바이너리도 포함 |

나중에 시작하는 프로세스에도 붙습니다. `exec`와 `libssl` 파일 열기에 건 프로브가 새 프로세스를 알려 줍니다. rustls나
Java의 JSSE처럼 다른 TLS 스택을 쓰는 프로그램은 아직 보이지 않으며,
[커버리지 메트릭](guides/coverage.md)이 그런 프로그램을 나열합니다.

## 2. 평문 복사 { #2-copy-the-plaintext }

각 프로브는 함수가 호출되거나 반환될 때 실행되는 eBPF 프로그램입니다. 프로브는 평문 버퍼(읽기는 최대 16 KB, 쓰기는
최대 4 KB), 프로세스 ID, cgroup ID를 에이전트와 공유하는 링 버퍼에 복사합니다. 이 프로그램들은 읽기만 합니다. 어떤
프로그램도 호출, 데이터, 연결을 바꾸지 않습니다.

Ollama나 vLLM처럼 클러스터 안에서 평문 HTTP로 호출하는 모델 서버에는 훅을 걸 TLS 라이브러리가 없습니다.
`plaintext`로 표시한 프로바이더의 경우, 에이전트는 대신 소켓에서 바이트를 복사합니다
([평문 HTTP로 제공하는 자체 호스팅 모델](guides/plaintext.md) 참고).

## 3. 바이트를 호출로 바꾸기 { #3-turn-bytes-into-calls }

사용자 공간에서 에이전트는 각 연결의 바이트를 HTTP/1.1 또는 HTTP/2 요청과 응답, 또는 WebSocket 메시지(Realtime,
Responses, Gemini Live)로 다시 조립하고, 압축을 풀고(`gzip`, `br`, `zstd`, `deflate`), SSE 스트림을
끝까지 따라갑니다. 요청의 호스트와 경로로 프로바이더를 고르고, 응답의 usage 필드에서 토큰 수를 읽습니다. 프롬프트,
출력(completion), 캐시 읽기와 쓰기, 추론 토큰입니다.

에이전트가 보았지만 읽지 못한 호출은 조용히 버려지지 않습니다. 이유와 함께 `llm_responses_unparsed_total`에
집계됩니다.

## 4. 파드 찾기 { #4-find-the-pod }

프로세스의 cgroup에서 컨테이너 ID를 얻습니다. 이 노드의 파드를 감시(watch)하여 컨테이너를 네임스페이스, 파드, 그리고
디플로이먼트나 크론잡 같은 소유 워크로드에 매핑합니다.

## 5. 호출에 가격 매기기 { #5-price-the-call }

응답의 모델 이름으로 날짜가 붙은 내장 가격표를 찾고, 그 위에 직접 정한 가격이나 할인을 적용합니다. 호출 비용은
프로바이더가 청구하는 방식대로 입력, 캐시 읽기, 캐시 쓰기, 출력으로 나뉩니다. 가격이 없는 모델은
`llm_unpriced_tokens_total`에 집계되며, $0으로 기록되지 않습니다.

## 6. 내보내기 { #6-export }

에이전트는 토큰, 요청, 지연 시간, 비용을 네임스페이스, 파드, 워크로드, 모델 레이블을 붙인 OpenTelemetry 메트릭으로
OTLP를 통해 내보내고, 호출마다 모델, 파드, 지연 시간, 토큰 수를 담은 로그를 한 줄 남깁니다(`debug`에서는 요청 경로도 남김).
캡처한 바이트는 에이전트 메모리에만 있다가 파싱이 끝나면 버려집니다. 프롬프트, 응답, 헤더, API 키는 내보내거나
로그에 남기지 않습니다.

[보안 검토](security-review.md)에는 모든 eBPF 프로그램, 에이전트에 필요한 권한, 에이전트가 볼 수 있는 것과 볼 수
없는 것이 정리되어 있습니다.

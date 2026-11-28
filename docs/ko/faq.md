<!-- translated from docs/en/faq.md at 250d516 -->

# 자주 묻는 질문 { #faq }

!!! note ""
    영어 원문을 옮긴 페이지입니다. 내용이 다르면 [영어 원문](https://honeybee-studio.github.io/honeybeepf-llm/faq/)이 맞습니다.

## 앱을 바꿔야 하나요? { #do-i-have-to-change-my-apps }

아니요. 앱은 자신의 SDK와 키로 계속 프로바이더를 직접 호출합니다. 바꿀 base URL도, 가져올 라이브러리도, 다시 배포할
것도 없습니다. [동작 원리](how-it-works.md)를 참고하세요.

## 요청 경로에 끼어들거나 호출을 느리게 하나요? { #does-it-sit-in-the-request-path-or-slow-my-calls-down }

아니요. 에이전트는 노드에서 TLS 버퍼의 사본을 읽습니다. 요청은 이전과 똑같이 프로바이더로 가고, 에이전트를 기다리는
것은 없습니다. 에이전트가 쓰는 CPU와 메모리, 부하 상황에서 처리량에 미치는 영향은
[오버헤드 벤치마크](benchmark.md)에서 측정했습니다.

## 프롬프트와 API 키를 읽나요? { #does-it-read-my-prompts-and-api-keys }

TLS를 읽는 모든 도구가 그렇듯, 프로브는 각 호출의 복호화된 바이트를 노드에 있는 에이전트 메모리로 복사합니다.
에이전트는 이 바이트에서 usage 필드를 파싱한 뒤 버립니다. 프롬프트, 응답, 헤더, API 키는 내보내거나 로그에 남기지
않습니다. 무엇을 내보내고 로그에 남기는지는 [보안 검토](security-review.md#3-data-handling)에 필드별로 나와 있습니다.

## 숫자는 얼마나 정확한가요? { #how-accurate-are-the-numbers }

토큰 수는 프로바이더가 보고한 값 그대로입니다. 에이전트는 캐시 토큰과 추론 토큰을 포함해 각 응답의 usage 필드를
읽습니다. 비용은 추정치입니다. 이 토큰 수에 날짜가 붙은 가격표를 곱한 값이며, 가격표 위에 계약 가격을 덮어쓸 수
있습니다([비용과 가격](cost.md)). 에이전트가 보았지만 읽지 못한 호출은 조용히 버리지 않고 이유와 함께 집계합니다.

## 어떤 프로바이더, 언어, 프로토콜을 지원하나요? { #which-providers-languages-and-protocols-does-it-cover }

OpenAI, Anthropic, Gemini, Bedrock, Azure OpenAI, Vertex AI, Meta와 여러 OpenAI 호환 API를 지원합니다. Python,
Node.js, Go, curl 등 OpenSSL을 쓰는 모든 프로그램에서, 스트리밍을 포함한 HTTP/1.1과 HTTP/2, 그리고 WebSocket을
통한 OpenAI Realtime, Responses, Gemini Live를 다룹니다. rustls, Java, 다른 WebSocket API, HTTP/3은 아직 지원하지
않습니다. 전체 지원 표는 [지원 범위](providers.md)에 있으며,
[메트릭 하나](guides/coverage.md)로 클러스터에서 보이지 않는 워크로드를 나열할 수 있습니다.

## 이미 LiteLLM 같은 LLM 게이트웨이를 운영하고 있습니다. 이것도 필요한가요? { #we-already-run-an-llm-gateway-such-as-litellm-do-we-need-this }

게이트웨이는 자신을 거쳐 가는 호출만 봅니다. 에이전트는 게이트웨이를 우회하는 호출까지 포함해 노드에서 나가는 모든
호출을 보고, 앱을 바꾸지 않고도 각 호출을 만든 파드를 알려 줍니다.
[형태 기반 감지](guides/detect-by-shape.md)를 켜면 클러스터 안의 게이트웨이를 거친 호출이 두 번 기록되고(앱에서 게이트웨이로,
게이트웨이에서 프로바이더로), 한 번만 세는 방법은 그 페이지에 있습니다.

## 노드에는 무엇이 필요한가요? { #what-does-it-need-on-the-nodes }

x86-64 또는 arm64에서 BTF를 갖춘 Linux 5.10 이상 또는 RHEL 8.10, 9.6, 9.7 커널이 필요합니다. 에이전트는 특권
모드로 실행되지 않습니다. `hostPID`와 몇 가지 capability만으로 실행되며, 이는 [설치](install.md#permissions-and-resources)와
[보안 검토](security-review.md#2-privileges)에서 설명합니다.

## Kubernetes 없이도 동작하나요? { #does-it-work-without-kubernetes }

네. `k8s` feature 없이 빌드한 바이너리로 Linux 호스트에서 실행할 수 있습니다. 이 경우 메트릭에는 네임스페이스, 파드,
워크로드 레이블이 붙지 않습니다. [Kubernetes 없이 실행](run-binary.md)을 참고하세요.

## 버그 신고나 프로바이더 요청은 어디에 하나요? { #where-do-i-report-a-bug-or-ask-for-a-provider }

[GitHub Issues](https://github.com/honeybee-studio/honeybeepf-llm/issues)에 남겨 주세요.

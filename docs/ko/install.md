<!-- translated from docs/en/install.md, git blob 0b2ef7c4e900 -->

# 설치 { #install }

!!! note ""
    영어 원문을 옮긴 페이지입니다. 내용이 다르면 [영어 원문](https://honeybee-studio.github.io/honeybeepf-llm/install/)이 맞습니다.

[빠른 시작](quickstart.md)은 에이전트만 설치하고 호출을 로그에서 읽습니다. 이 페이지는 메트릭 파이프라인과 함께
설치합니다. 에이전트가 OpenTelemetry 메트릭을 Collector로 보내고, Prometheus가 Collector를 스크레이프하며,
[Grafana 대시보드](grafana/README.md)는 Prometheus에서 데이터를 읽습니다.

## 에이전트, Collector, Prometheus { #agent-collector-and-prometheus }

노드가 Linux 5.10+ 또는 RHEL 8.10 / 9.6 / 9.7 커널을 실행하는 Kubernetes 클러스터, `kubectl`, Helm 3이 필요합니다.
아래 단계는 최신 릴리스인 v0.5.0을 함께 제공되는 Prometheus, OpenTelemetry Collector와 같이 설치합니다. `main`의
차트는 릴리스 이미지보다 새로운 에이전트를 전제할 수 있으므로, 차트는 릴리스 태그로 체크아웃합니다.

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

LLM 호출을 실시간으로 확인합니다.

```bash
kubectl logs -n honeybeepf ds/honeybeepf-llm -f | grep 'LLM SUCCESS'
```

`ds/honeybeepf-llm`은 에이전트 파드 하나만 따라갑니다. 노드가 여러 개인 클러스터에서는 각 노드의 호출이 그 노드의
에이전트 파드에 남습니다.

```
2026-10-26T08:43:35.510264Z  INFO honeybeepf_llm::probes::builtin::llm::processor::background: LLM SUCCESS | PID: 1686847 | Model: claude-test | Transport: tls | Latency: 0.00s | Tokens: 140 (Prompt: 125, Compl: 15, Cache read: 100, Cache write: 0) | Pod: team-b/support-bot-589db898dd-rv9ts
```

함께 제공되는 Prometheus는 `honeybeepf` 네임스페이스의 `honeybeepf-llm-prometheus-server` 서비스(포트 80)입니다.
[메트릭 레퍼런스](metrics.md)에 예제 쿼리가 있고, [Grafana 대시보드](grafana/README.md)에서 비용 대시보드를 불러오는
방법을 설명합니다.

## 클러스터 외부의 Collector { #a-collector-outside-the-cluster }

클러스터 외부의 Collector로 메트릭을 보내려면 TLS를 쓰고, Collector가 요구하면 인증 헤더도 지정합니다.

```bash
kubectl -n honeybeepf create secret generic otel-ca --from-file=ca.crt=./collector-ca.crt   # only for a private CA
kubectl -n honeybeepf create secret generic otel-auth --from-literal=headers='authorization=Bearer <token>'
helm upgrade --install honeybeepf-llm charts/honeybeepf-llm -n honeybeepf --reuse-values \
  --set output.otlp.endpoint=https://otel.example.com:4317 \
  --set output.otlp.caSecret=otel-ca \
  --set output.otlp.headersSecret.name=otel-auth --set output.otlp.headersSecret.key=headers
```

`https://` 엔드포인트는 `caSecret`이 설정되어 있으면 그 CA로, 아니면 시스템 루트 인증서로 검증합니다. `host:port`와
`http://`는 평문으로 유지됩니다.

## 권한과 리소스 { #permissions-and-resources }

에이전트에는 `hostPID`와 `BPF`, `PERFMON`, `SYS_ADMIN`, `SYS_PTRACE`, `NET_ADMIN`, `SYS_RESOURCE` capability가 필요하며, 노드마다 50m CPU / 128Mi를 요청합니다(제한 1 CPU / 1Gi).
[`values.yaml`](https://github.com/honeybee-studio/honeybeepf-llm/blob/main/charts/honeybeepf-llm/values.yaml)을 참고하세요.

이 중 두 기능은 오래된 커널에서만 필요합니다. 차트는 한 번 설치로 모든 노드에서 동작하도록 이 기능을 부여합니다.
모든 노드가 더 새로운 커널을 실행한다면 제거해도 됩니다.

| 기능 | 노드가 다음 커널을 실행할 때 필요 | 이유 | 모든 노드가 다음 커널을 실행하면 제거 가능 |
|---|---|---|---|
| `NET_ADMIN` | 6.9 이전 커널 | 에이전트는 평문 캡처를 꺼도 시작할 때 소켓 맵을 만들며, 이 커널은 여기에 `NET_ADMIN`을 요구합니다 | 6.9 이상 |
| `SYS_RESOURCE` | 5.11 이전 커널 | 이 커널은 BPF 맵 메모리를 memlock 제한에 포함하며, 에이전트는 시작할 때 이 제한을 올립니다 | 5.11 이상 |

목록을 설정하면 기존 목록을 대체하므로 나머지 네 개는 남겨 둡니다. 예를 들어 모든 노드가 6.9 이상을 실행한다면
다음과 같습니다.

```yaml
securityContext:
  capabilities:
    add: [BPF, PERFMON, SYS_ADMIN, SYS_PTRACE]
```

노드의 커널이 요구하는 기능이 빠져 있으면, 에이전트는 시작할 때 맵을 만들다가 `Operation not permitted`로
종료됩니다.

## 제거 { #uninstall }

```bash
helm uninstall honeybeepf-llm honeybeepf-llm-otel-collector honeybeepf-llm-prometheus -n honeybeepf
kubectl delete namespace honeybeepf
```

릴리스를 설치하기 전에 [서명을 검증](verify.md)할 수 있습니다. 직접 체크아웃한 소스로 빌드한 이미지를 실행하려면
[소스에서 빌드](build.md)를 참고하세요.

<!-- translated from docs/quickstart.md at 250d516 -->

# 빠른 시작 { #quickstart }

!!! note ""
    영어 원문을 옮긴 페이지입니다. 내용이 다르면 [영어 원문](https://honeybee-studio.github.io/honeybeepf-llm/quickstart/)이 맞습니다.

몇 분 안에 클러스터의 모든 파드가 하는 LLM 호출을 확인합니다. 이 페이지는 에이전트만 설치하고 호출을 에이전트 로그에서
읽습니다. [설치](install.md)에서는 Prometheus, OpenTelemetry Collector, Grafana 비용 대시보드를 추가합니다.

## 시작하기 전에 { #before-you-start }

- 노드가 Linux 5.10 이상 또는 RHEL 8.10, 9.6, 9.7 커널을 실행하는 Kubernetes 클러스터.
- `kubectl`과 Helm 3.
- Python, Node.js, Go, curl 또는 OpenSSL을 쓰는 다른 프로그램으로 HTTPS를 통해 LLM 프로바이더를 호출하는 파드. 아직
  LLM 트래픽이 없다면 대신 [데모](demo.md)를 실행하세요.

## 1. 에이전트 설치 { #1-install-the-agent }

```bash
kubectl create namespace honeybeepf
helm install honeybeepf-llm oci://ghcr.io/honeybee-studio/charts/honeybeepf-llm -n honeybeepf
```

차트는 데몬셋으로 노드마다 에이전트 파드를 하나씩 실행합니다. 앱과 앱의 SDK, API 키는 그대로 둡니다.

## 2. 호출 확인 { #2-watch-the-calls }

```bash
kubectl logs -n honeybeepf ds/honeybeepf-llm -f | grep 'LLM SUCCESS'
```

LLM 호출 하나가 한 줄로 나오며, 모델, 토큰 수, 호출한 파드가 함께 표시됩니다.

```
LLM SUCCESS | PID: 1686847 | Model: claude-test | Transport: tls | Latency: 0.00s | Tokens: 140 (Prompt: 125, Compl: 15, Cache read: 100, Cache write: 0) | Pod: team-b/support-bot-589db898dd-rv9ts
```

`ds/honeybeepf-llm`은 에이전트 파드 하나만 따라갑니다. 노드가 여러 개인 클러스터에서는 각 노드의 호출이 그 노드의
에이전트 파드에 남습니다. `kubectl logs -n honeybeepf -l app.kubernetes.io/name=honeybeepf-llm --prefix -f`로 모두 따라갈 수 있습니다.

줄이 나오지 않으면 [문제 해결](troubleshooting.md)을 참고하세요.

## 3. 다음 단계 { #3-next }

- [메트릭을 Prometheus로 보내고 비용 대시보드 열기](install.md).
- [어떤 워크로드가 보이는지 확인하기](guides/coverage.md).
- [호출에 가격을 매기는 방식 보기](cost.md), 그리고 [직접 정한 가격 추가하기](guides/prices.md).

제거하려면 다음을 실행합니다.

```bash
helm uninstall honeybeepf-llm -n honeybeepf
kubectl delete namespace honeybeepf
```

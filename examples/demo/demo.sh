#!/usr/bin/env bash
# Run honeybeepf-llm against a mock LLM provider on any Kubernetes cluster, with no API keys.
#   examples/demo/demo.sh up     mock provider, agent, Prometheus, collector, Grafana and three client teams
#   examples/demo/demo.sh down   remove all of it
# HB_DEMO_CHART=local installs charts/honeybeepf-llm from this checkout with the `latest` image built from main,
# instead of the release chart.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo=$(cd "$here/../.." && pwd)
NS=honeybeepf
DEMO_NS=llm-demo
TEAMS=(team-a team-b team-c)
HOSTS=(api.openai.com api.anthropic.com generativelanguage.googleapis.com)

down() {
  helm uninstall grafana -n monitoring --ignore-not-found >/dev/null 2>&1 || true
  for r in honeybeepf-llm honeybeepf-llm-otel-collector honeybeepf-llm-prometheus; do
    helm uninstall "$r" -n "$NS" --ignore-not-found >/dev/null 2>&1 || true
  done
  kubectl delete namespace "$DEMO_NS" "${TEAMS[@]}" monitoring "$NS" --ignore-not-found --wait=false
}

need() { command -v "$1" >/dev/null || { echo "demo: needs $1" >&2; exit 1; }; }

# A CA and a server certificate for the three provider hosts, so the clients verify TLS against the mock.
certs() {
  local dir=$1
  openssl req -x509 -newkey rsa:2048 -nodes -days 7 -subj /CN=honeybeepf-llm-demo-ca \
    -keyout "$dir/ca.key" -out "$dir/ca.crt" 2>/dev/null
  openssl req -newkey rsa:2048 -nodes -subj /CN=llm-demo-mock -keyout "$dir/tls.key" -out "$dir/tls.csr" 2>/dev/null
  printf 'subjectAltName=%s\n' "$(printf 'DNS:%s,' "${HOSTS[@]}" | sed 's/,$//')" >"$dir/san.ext"
  openssl x509 -req -in "$dir/tls.csr" -CA "$dir/ca.crt" -CAkey "$dir/ca.key" -CAcreateserial -days 7 \
    -extfile "$dir/san.ext" -out "$dir/tls.crt" 2>/dev/null
}

mock() {
  kubectl create namespace "$DEMO_NS" --dry-run=client -o yaml | kubectl apply -f - >/dev/null
  kubectl -n "$DEMO_NS" create secret tls mock-tls --cert="$tmp/tls.crt" --key="$tmp/tls.key" \
    --dry-run=client -o yaml | kubectl apply -f - >/dev/null
  kubectl -n "$DEMO_NS" create configmap mock --from-file="$here/mock.py" \
    --dry-run=client -o yaml | kubectl apply -f - >/dev/null
  kubectl apply -f - >/dev/null <<YAML
apiVersion: apps/v1
kind: Deployment
metadata: {name: mock-llm, namespace: $DEMO_NS}
spec:
  selector: {matchLabels: {app: mock-llm}}
  template:
    metadata: {labels: {app: mock-llm}}
    spec:
      containers:
      - name: mock
        image: python:3.12-slim
        command: [python3, -u, /mock/mock.py, /tls/tls.crt, /tls/tls.key, "8443"]
        ports: [{containerPort: 8443}]
        readinessProbe: {tcpSocket: {port: 8443}}
        volumeMounts: [{name: mock, mountPath: /mock}, {name: tls, mountPath: /tls}]
      volumes:
      - {name: mock, configMap: {name: mock}}
      - {name: tls, secret: {secretName: mock-tls}}
---
apiVersion: v1
kind: Service
metadata: {name: mock-llm, namespace: $DEMO_NS}
spec:
  selector: {app: mock-llm}
  ports: [{port: 443, targetPort: 8443}]
YAML
  kubectl -n "$DEMO_NS" rollout status deploy/mock-llm --timeout=300s >/dev/null
}

agent() {
  helm repo add prometheus-community https://prometheus-community.github.io/helm-charts >/dev/null
  helm repo add open-telemetry https://open-telemetry.github.io/opentelemetry-helm-charts >/dev/null
  helm dependency build "$repo/charts/honeybeepf-llm-prometheus" >/dev/null
  helm dependency build "$repo/charts/honeybeepf-llm-otel-collector" >/dev/null
  kubectl create namespace "$NS" --dry-run=client -o yaml | kubectl apply -f - >/dev/null
  helm upgrade --install honeybeepf-llm-prometheus "$repo/charts/honeybeepf-llm-prometheus" -n "$NS" >/dev/null
  helm upgrade --install honeybeepf-llm-otel-collector "$repo/charts/honeybeepf-llm-otel-collector" -n "$NS" >/dev/null
  local chart=(oci://ghcr.io/honeybee-studio/charts/honeybeepf-llm --version "$version")
  [ "${HB_DEMO_CHART:-release}" = local ] && chart=("$repo/charts/honeybeepf-llm" --set image.tag=latest)
  helm upgrade --install honeybeepf-llm "${chart[@]}" -n "$NS" \
    --set builtinProbes.llm.enabled=true \
    --set output.otlp.endpoint=honeybeepf-llm-otel-collector-opentelemetry-collector:4317 >/dev/null
  kubectl -n "$NS" rollout status ds/honeybeepf-llm --timeout=300s >/dev/null
}

grafana() {
  helm repo add grafana https://grafana.github.io/helm-charts >/dev/null
  kubectl create namespace monitoring --dry-run=client -o yaml | kubectl apply -f - >/dev/null
  kubectl -n monitoring create configmap honeybeepf-dashboards --from-file="$repo/docs/grafana/honeybeepf-cost.json" \
    --dry-run=client -o yaml | kubectl apply -f - >/dev/null
  kubectl -n monitoring label configmap honeybeepf-dashboards grafana_dashboard=1 --overwrite >/dev/null
  helm upgrade --install grafana grafana/grafana -n monitoring -f "$here/grafana-values.yaml" \
    --wait --timeout 300s >/dev/null
}

# One Deployment of LLM clients per team: Python with the system libssl, or the official Node.js image.
clients() {
  local ip
  ip=$(kubectl -n "$DEMO_NS" get service mock-llm -o jsonpath='{.spec.clusterIP}')
  local hosts
  hosts=$(printf '%s,' "${HOSTS[@]}" | sed 's/,$//')
  for ns in "${TEAMS[@]}"; do
    kubectl create namespace "$ns" --dry-run=client -o yaml | kubectl apply -f - >/dev/null
    kubectl -n "$ns" create configmap demo-client --from-file="$here/client.py" --from-file="$here/client.mjs" \
      --from-file="$tmp/ca.crt" --dry-run=client -o yaml | kubectl apply -f - >/dev/null
  done
  deploy team-a chat-api 2 python:3.12-slim '[python3, -u, /c/client.py, "openai-json,openai-sse,openai-cache", "4"]'
  deploy team-b support-bot 1 python:3.12-slim \
    '[python3, -u, /c/client.py, "anthropic-json,anthropic-cache,anthropic-fallback", "5"]'
  deploy team-c summarizer 1 node:24-slim '[node, /c/client.mjs, "6"]'
  for ns in "${TEAMS[@]}"; do kubectl -n "$ns" rollout status deploy --timeout=300s >/dev/null; done
}

deploy() {  # deploy <namespace> <name> <replicas> <image> <command as a YAML list>
  kubectl apply -f - >/dev/null <<YAML
apiVersion: apps/v1
kind: Deployment
metadata: {name: $2, namespace: $1}
spec:
  replicas: $3
  selector: {matchLabels: {app: $2}}
  template:
    metadata: {labels: {app: $2}}
    spec:
      terminationGracePeriodSeconds: 1
      hostAliases: [{ip: "$ip", hostnames: [$hosts]}]
      containers:
      - name: app
        image: $4
        command: $5
        env:
        - {name: SSL_CERT_FILE, value: /c/ca.crt}
        - {name: NODE_EXTRA_CA_CERTS, value: /c/ca.crt}
        volumeMounts: [{name: c, mountPath: /c}]
      volumes: [{name: c, configMap: {name: demo-client}}]
YAML
}

case "${1:-}" in
  down) down; echo "demo: removed"; exit 0 ;;
  up) ;;
  *) sed -n '2,6p' "$0" | sed 's/^# \{0,1\}//'; exit 2 ;;
esac

for tool in kubectl helm openssl; do need "$tool"; done
version=$(sed -nE 's/^version:[[:space:]]*"?([^"[:space:]]+)"?.*/\1/p' "$repo/charts/honeybeepf-llm/Chart.yaml")
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

certs "$tmp"
echo "demo: mock LLM provider";  mock
echo "demo: honeybeepf-llm ${HB_DEMO_CHART:-release} chart, Prometheus, OpenTelemetry Collector";  agent
echo "demo: Grafana";  grafana
echo "demo: LLM clients in ${TEAMS[*]}";  clients

cat <<EOF
demo: up. Calls show in the agent log within seconds and in Grafana after a scrape (about a minute):
  kubectl -n $NS logs ds/honeybeepf-llm -f | grep 'LLM SUCCESS'
  kubectl -n monitoring port-forward svc/grafana 3000:80   # then http://localhost:3000, "LLM Cost Observability"
Remove it with: $0 down
EOF

#!/usr/bin/env bash
# Per-kernel BPF compatibility test. /etc/hosts redirects the LLM provider
# domains to a local TLS mock so we never hit real APIs from CI; curl still
# sends the real Host header, which is what the agent's parser matches on.
set -euo pipefail

cd /host

uname -r
if ! mount | grep -q ' on /sys/fs/bpf type bpf'; then
  echo "::error::bpffs not mounted at /sys/fs/bpf — BPF probe load will fail"
  exit 1
fi

apt-get install -y --no-install-recommends curl ca-certificates openssl python3 >/dev/null 2>&1 || true

openssl req -x509 -newkey rsa:2048 -keyout /tmp/key.pem -out /tmp/cert.pem \
  -days 1 -nodes -subj "/CN=mock" 2>/dev/null

cat > /tmp/mock.py <<'PY'
import http.server, ssl, socketserver
class H(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        self.send_response(401); self.send_header('Content-Length','0'); self.end_headers()
    def log_message(self, *_): pass
ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
ctx.load_cert_chain('/tmp/cert.pem', '/tmp/key.pem')
srv = socketserver.TCPServer(('127.0.0.1', 443), H)
srv.socket = ctx.wrap_socket(srv.socket, server_side=True)
srv.serve_forever()
PY

python3 /tmp/mock.py >/dev/null 2>&1 &
MOCK_PID=$!

# wait until mock accepts TLS (max ~10s)
for _ in $(seq 1 20); do
  curl -sk --connect-timeout 1 -o /dev/null https://127.0.0.1 2>/dev/null && break
  sleep 0.5
done
curl -sk --connect-timeout 1 -o /dev/null https://127.0.0.1 || {
  echo "::error::mock TLS server failed to start"
  exit 1
}

cat >> /etc/hosts <<EOF
127.0.0.1 api.openai.com
127.0.0.1 api.anthropic.com
127.0.0.1 generativelanguage.googleapis.com
EOF

export BUILTIN_PROBES__LLM=true
export BUILTIN_PROBES__INTERVAL=2
export RUST_LOG=info

# Configure one plaintext provider so the sockops/sk_msg/sk_skb programs load and the cgroup-v2 gate
# runs. A throwaway kubeconfig lets the client build (no cluster in CI); 127.0.0.1 resolves via DNS.
cat > /tmp/kubeconfig.yaml <<'EOF'
apiVersion: v1
kind: Config
clusters: [{name: x, cluster: {server: "https://127.0.0.1:1"}}]
users: [{name: x, user: {}}]
contexts: [{name: x, context: {cluster: x, user: x}}]
current-context: x
EOF
export KUBECONFIG=/tmp/kubeconfig.yaml
export LLM_PROVIDERS_CONFIG='{"providers":[{"name":"vllm","hosts":["127.0.0.1"],"plaintext":{"port":18080}}]}'

# Run the agent with only the chart's capabilities in its bounding set, as the DaemonSet does, so a capability the
# agent needs but the chart does not grant fails here instead of on a user's node (CI itself runs as root).
CHART_CAPS=$(sed -n '/^securityContext:/,/^resources:/p' charts/honeybeepf-llm/values.yaml | tr -d '\r' \
  | sed -n 's/^ *- \([A-Z_]*\).*/\1/p' | grep -vx ALL)
echo "agent capabilities: ${CHART_CAPS//$'\n'/ }"
cat > /tmp/capexec.py <<'PY'
import ctypes, os, sys
# Capability numbers from linux/capability.h; a name the chart adds must be listed here.
NUMBERS = {"NET_ADMIN": 12, "SYS_PTRACE": 19, "SYS_ADMIN": 21, "PERFMON": 38, "BPF": 39}
PR_CAPBSET_DROP = 24
split = sys.argv.index("--")
keep = {NUMBERS[name] for name in sys.argv[1:split]}
last = int(open("/proc/sys/kernel/cap_last_cap").read())
prctl = ctypes.CDLL(None, use_errno=True).prctl
for cap in range(last + 1):
    if cap not in keep and prctl(PR_CAPBSET_DROP, cap, 0, 0, 0) != 0:
        sys.exit(f"drop capability {cap}: {os.strerror(ctypes.get_errno())}")
os.execv(sys.argv[split + 1], sys.argv[split + 1:])
PY
# shellcheck disable=SC2086 # one argument per capability
python3 /tmp/capexec.py $CHART_CAPS -- ./bin/honeybeepf-llm --verbose > /tmp/boot.log 2>&1 &
AGENT_PID=$!
# ~5s for BPF program load + verifier, ~3s for libssl scan / uprobe attach
sleep 8

for url in \
  'https://api.openai.com/v1/chat/completions' \
  'https://api.anthropic.com/v1/messages' \
  'https://generativelanguage.googleapis.com/v1beta/models'; do
  curl -sk --http1.1 --max-time 5 -X POST "$url" \
    -H 'Authorization: Bearer fake-test-key' \
    -H 'Content-Type: application/json' \
    -d '{"model":"test","messages":[{"role":"user","content":"hi"}]}' \
    -o /dev/null -w 'curl %{url_effective} -> %{http_code}\n' || true
done

sleep 3

# graceful then forceful — agent occasionally ignores SIGINT, don't hang `wait`
kill -INT "$AGENT_PID" 2>/dev/null || true
for _ in $(seq 1 10); do
  kill -0 "$AGENT_PID" 2>/dev/null || break
  sleep 0.3
done
kill -KILL "$AGENT_PID" 2>/dev/null || true
wait "$AGENT_PID" 2>/dev/null || true

kill "$MOCK_PID" 2>/dev/null || true

cat /tmp/boot.log

# A working kprobe fallback logs the fentry error it replaced; that line is not a failure.
if grep -v 'libssl open watch: kprobe' /tmp/boot.log | grep -qiE 'panicked|FATAL|verifier|failed to attach|failed to load|failed to create map|drop capability'; then
  echo "::error::BPF load/attach failure on $(uname -r)"
  exit 1
fi

grep -qE 'Attaching LLM \(SSL\) probes to detected library' /tmp/boot.log
grep -qE 'Active probe registered: llm' /tmp/boot.log
grep -qE '\[LLM\] Detected HTTP' /tmp/boot.log

# Plaintext capture must either enable (cgroup v2 + kernel 5.10+) or disable with a gate reason,
# never silently; a verifier rejection of its programs is already caught by the failure grep above.
grep -qE 'Plaintext capture (enabled|disabled)' /tmp/boot.log
if grep -qE 'Plaintext capture disabled' /tmp/boot.log; then
  grep -qE 'Plaintext capture disabled: (kernel older than 5\.10|no cgroup v2)' /tmp/boot.log
fi

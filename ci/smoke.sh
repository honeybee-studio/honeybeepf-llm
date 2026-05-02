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

./bin/honeybeepf-llm --verbose > /tmp/boot.log 2>&1 &
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

if grep -qiE 'panicked|FATAL|verifier|failed to attach|failed to load' /tmp/boot.log; then
  echo "::error::BPF load/attach failure on $(uname -r)"
  exit 1
fi

grep -qE 'Attaching LLM \(SSL\) probes' /tmp/boot.log
grep -qE 'Active probe registered: llm' /tmp/boot.log
grep -qE '\[LLM\] Detected HTTP' /tmp/boot.log

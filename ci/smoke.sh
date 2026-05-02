#!/usr/bin/env bash
# Per-kernel BPF compatibility test. Boots the agent, sends a few HTTPS
# requests through libssl, and asserts the BPF program loads, the uprobe
# attaches, and the agent dynamically discovers the curl process.
set -euo pipefail

cd /host

uname -r
mount | grep bpf || true

apt-get install -y --no-install-recommends curl ca-certificates >/dev/null 2>&1 || true

export BUILTIN_PROBES__LLM=true
export BUILTIN_PROBES__INTERVAL=2
export RUST_LOG=info

./bin/honeybeepf-llm --verbose > /tmp/boot.log 2>&1 &
AGENT_PID=$!
sleep 8

for url in \
  'https://api.openai.com/v1/chat/completions' \
  'https://api.anthropic.com/v1/messages' \
  'https://generativelanguage.googleapis.com/v1beta/models'; do
  curl -sk --http1.1 --max-time 10 -X POST "$url" \
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

cat /tmp/boot.log

if grep -qiE 'panicked|FATAL|verifier|failed to attach|failed to load' /tmp/boot.log; then
  echo "::error::BPF load/attach failure on $(uname -r)"
  exit 1
fi

grep -qE 'Attaching LLM \(SSL\) probes' /tmp/boot.log
grep -qE 'Active probe registered: llm' /tmp/boot.log
grep -qE '\[Re-discovery\] New SSL library found' /tmp/boot.log
grep -E '\[LLM\] Detected HTTP' /tmp/boot.log || true

#!/usr/bin/env bash
# End-to-end SSL trace test run inside vmtest. Sends fake POSTs to the
# three hardcoded LLM provider hosts; the agent's SSL_write hook should
# parse the request bytes and emit a "[LLM] Detected HTTP" log line.
set -euo pipefail

cd /host

apt-get install -y --no-install-recommends curl ca-certificates >/dev/null 2>&1 || true

export BUILTIN_PROBES__LLM=true
export BUILTIN_PROBES__INTERVAL=2
export RUST_LOG=info

./bin/honeybeepf-llm --verbose > /tmp/agent.log 2>&1 &
sleep 10

for url in \
  'https://api.openai.com/v1/chat/completions' \
  'https://api.anthropic.com/v1/messages' \
  'https://generativelanguage.googleapis.com/v1beta/models'; do
  curl -sk -X POST "$url" \
    -H 'Authorization: Bearer fake-test-key' \
    -H 'Content-Type: application/json' \
    -d '{"model":"test","messages":[{"role":"user","content":"hi"}]}' \
    -o /dev/null -w 'curl %{url_effective} -> %{http_code}\n' || true
done

sleep 5
pkill -INT honeybeepf-llm || true
sleep 1

echo '----- agent log -----'
cat /tmp/agent.log
echo '----- assertions -----'
grep -E 'Attaching LLM \(SSL\) probes' /tmp/agent.log
grep -E '\[LLM\] Detected HTTP' /tmp/agent.log

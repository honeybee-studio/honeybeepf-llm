#!/usr/bin/env bash
# Per-kernel BPF compatibility test. Boots the agent with the LLM probe
# enabled, lets it attach uprobes on libssl, and asserts the BPF program
# loads + probe attaches without verifier or runtime errors.
set -euo pipefail

cd /host

uname -r
mount | grep bpf || true

export BUILTIN_PROBES__LLM=true
export BUILTIN_PROBES__INTERVAL=2
export RUST_LOG=info

./bin/honeybeepf-llm --verbose > /tmp/boot.log 2>&1 &
sleep 8
pkill -INT honeybeepf-llm 2>/dev/null || true
wait 2>/dev/null || true

cat /tmp/boot.log

if grep -qiE 'panicked|FATAL|verifier|failed to attach|failed to load' /tmp/boot.log; then
  echo "::error::BPF load/attach failure on $(uname -r)"
  exit 1
fi

grep -qE 'Attaching LLM \(SSL\) probes' /tmp/boot.log
grep -qE 'Active probe registered: llm' /tmp/boot.log

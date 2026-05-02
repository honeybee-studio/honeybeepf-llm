#!/usr/bin/env bash
# Boot smoke test run inside vmtest. Verifies the agent starts and emits
# its expected startup log lines on the kernel under test.
set -euo pipefail

cd /host

uname -r
mount | grep bpf || true

export BUILTIN_PROBES__LLM=true
export BUILTIN_PROBES__INTERVAL=2
export RUST_LOG=info

timeout --preserve-status 5 ./bin/honeybeepf-llm --verbose > /tmp/boot.log 2>&1 || true

cat /tmp/boot.log
grep -qE 'Monitoring active|Parsed settings' /tmp/boot.log

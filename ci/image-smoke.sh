#!/usr/bin/env bash
# Starts the agent image and fails unless its startup BPF programs load, the libssl open watch included.
# The kernel matrix builds its own agent with another toolchain; this checks the binary the image ships, since
# the verifier's work on a program depends on the compiler that built it.
# Usage: ci/image-smoke.sh <image>   (needs a Docker daemon that can run privileged containers)
set -euo pipefail

image=$1
name=honeybeepf-llm-image-smoke
log=$(mktemp)
trap 'docker rm -f "$name" >/dev/null 2>&1 || true; rm -f "$log"' EXIT

# The same host paths the chart mounts, except the cgroup tree: plaintext capture is off here.
docker run -d --name "$name" --privileged \
  -v /sys/kernel/tracing:/sys/kernel/tracing -v /sys/kernel/debug:/sys/kernel/debug \
  -e BUILTIN_PROBES__LLM=true -e RUST_LOG=info "$image" >/dev/null
for _ in $(seq 1 60); do
  docker logs "$name" >"$log" 2>&1
  grep -q 'Dynamic discovery active' "$log" && break
  docker inspect -f '{{.State.Running}}' "$name" | grep -q true || break
  sleep 1
done
docker logs "$name" >"$log" 2>&1
cat "$log"

grep -q 'Dynamic discovery active' "$log" || { echo "::error::agent did not finish starting"; exit 1; }
if ! grep -q 'libssl open watch: fentry' "$log"; then
  echo "::error::libssl open watch did not load as fentry on $(uname -r)"
  exit 1
fi
if grep -qiE 'panicked|FATAL|verifier|failed to attach|failed to load' "$log"; then
  echo "::error::BPF load/attach failure on $(uname -r)"
  exit 1
fi
echo "image smoke: ok on $(uname -r)"

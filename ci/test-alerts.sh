#!/usr/bin/env bash
# Tests the chart's alert rules: promtool checks the rules and their annotation templates, runs the unit tests in
# tests/alerts against rules rendered from tests/alerts/values.yaml, and fails when the copy in the bundled
# Prometheus chart differs from the chart rendered with its defaults.
# Usage: ci/test-alerts.sh   (needs helm, and yq and promtool or Docker to run them)
set -euo pipefail

promtool_image='prom/prometheus:v3.15.1@sha256:fa3302279710be2f21a8301ec17a8b4f2301fd829bd113839b3b6b02b6f191a2'
yq_image='mikefarah/yq:4.44.3'

root=$(git rev-parse --show-toplevel)
cd "$root"
work=$(mktemp -d)
chmod 755 "$work" # the promtool container runs as nobody
trap 'rm -rf "$work"' EXIT

run_promtool() {
  if command -v promtool >/dev/null; then
    (cd "$work" && promtool "$@")
  else
    docker run --rm -v "$work:/w" -w /w --entrypoint promtool "$promtool_image" "$@"
  fi
}

run_yq() {
  if command -v yq >/dev/null; then
    yq "$@"
  else
    docker run --rm -i "$yq_image" "$@"
  fi
}

render() {
  helm template alerts charts/honeybeepf-llm --set alerts.prometheusRule.enabled=true \
    --show-only templates/prometheusrule.yaml "$@"
}

render -f tests/alerts/values.yaml > "$work/rule.yaml"
run_yq '{"groups": .spec.groups}' < "$work/rule.yaml" > "$work/full.yml"
# The unit tests compare labels only; the annotation templates are checked by `check rules` on full.yml.
run_yq '{"groups": .spec.groups} | del(.groups[].rules[].annotations)' < "$work/rule.yaml" > "$work/rules.yml"
cp tests/alerts/rules.test.yml "$work/"

run_promtool check rules --lint-fatal full.yml
run_promtool test rules rules.test.yml

render | run_yq -o=json -I=0 '.spec.groups' > "$work/chart.json"
run_yq -o=json -I=0 '.prometheus.serverFiles."alerting_rules.yml".groups' \
  < charts/honeybeepf-llm-prometheus/values.yaml > "$work/bundled.json"
if ! cmp -s "$work/chart.json" "$work/bundled.json"; then
  echo "::error::charts/honeybeepf-llm-prometheus/values.yaml has different alert rules from the chart's defaults;" \
    "copy them from: helm template x charts/honeybeepf-llm --set alerts.prometheusRule.enabled=true"
  exit 1
fi
echo "alert rules: ok"

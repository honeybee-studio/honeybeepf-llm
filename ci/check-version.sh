#!/usr/bin/env bash
# The agent crate, the chart and the release tag name one version, so a chart release runs the image built with it.
# Usage: ci/check-version.sh [repo-dir [tag]]   prints the version when they agree
set -euo pipefail

root=${1:-.}
has_tag=$(($# >= 2))
tag=${2:-}
cargo_toml=honeybeepf-llm/honeybeepf-llm/Cargo.toml
chart_yaml=charts/honeybeepf-llm/Chart.yaml

# Only the [package] table: dependency tables carry version keys too.
crate=$(awk '/^\[/ { pkg = ($0 == "[package]") }
  pkg && /^version[[:space:]]*=/ { if (match($0, /=[[:space:]]*"[^"]*"/)) { v = substr($0, RSTART, RLENGTH); sub(/^=[[:space:]]*"/, "", v); sub(/"$/, "", v); print v }; exit }' \
  "$root/$cargo_toml")
chart=$(sed -nE 's/^version:[[:space:]]*"?([^"[:space:]]*)"?[[:space:]]*$/\1/p' "$root/$chart_yaml")
app=$(sed -nE 's/^appVersion:[[:space:]]*"?([^"[:space:]]*)"?[[:space:]]*$/\1/p' "$root/$chart_yaml")

failed=0
fail() {
  echo "::error file=$1::$2" >&2
  failed=1
}

[[ $crate =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]] ||
  fail "$cargo_toml" "[package] version '$crate' is not X.Y.Z"
[[ $chart == "$crate" ]] || fail "$chart_yaml" "version '$chart' differs from the agent crate's $crate"
[[ $app == "$crate" ]] || fail "$chart_yaml" "appVersion '$app' differs from the agent crate's $crate"
# A given but empty tag is a caller bug, not "no tag".
if ((has_tag)) && [[ $tag != "v$crate" ]]; then
  echo "::error::tag $tag is not v$crate (the version in $cargo_toml)" >&2
  failed=1
fi

if ((failed)); then
  exit 1
fi
echo "$crate"

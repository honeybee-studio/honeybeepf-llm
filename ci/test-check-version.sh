#!/usr/bin/env bash
# Runs check-version.sh against throwaway trees, one per case.
set -euo pipefail

check="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/check-version.sh"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
failed=0

# An agent crate and chart that both say 0.1.0.
setup() {
  root=$(mktemp -d "$tmp/root.XXXXXX")
  mkdir -p "$root/honeybeepf-llm/honeybeepf-llm" "$root/charts/honeybeepf-llm"
  cat >"$root/honeybeepf-llm/honeybeepf-llm/Cargo.toml" <<'EOF'
[package]
name = "honeybeepf-llm"
version = "0.1.0"
edition.workspace = true

[dependencies]
tokio = { version = "1.47", features = ["full"] }

[build-dependencies]
version = "9.9.9"
EOF
  cat >"$root/charts/honeybeepf-llm/Chart.yaml" <<'EOF'
apiVersion: v2
name: honeybeepf-llm
type: application
version: 0.1.0
appVersion: "0.1.0"
EOF
}

cargo_version() { sed -i -E "0,/^version = .*/s//version = \"$1\"/" "$root/honeybeepf-llm/honeybeepf-llm/Cargo.toml"; }
chart_field() { sed -i -E "s/^$1:.*/$1: $2/" "$root/charts/honeybeepf-llm/Chart.yaml"; }

# expect <case> <pass|fail> <text the output must contain> [tag]
expect() {
  local got=pass
  bash "$check" "$root" "${@:4}" >"$tmp/out" 2>&1 || got=fail
  if [[ $got == "$2" && $(<"$tmp/out") == *"$3"* ]]; then
    echo "ok - $1"
  else
    echo "not ok - $1 (got $got)"
    cat "$tmp/out"
    failed=1
  fi
}

setup
expect "agent and chart agree, no tag" pass "0.1.0"

setup
expect "matching tag" pass "0.1.0" v0.1.0

setup
chart_field appVersion 0.1.0
expect "unquoted appVersion" pass "0.1.0" v0.1.0

setup
expect "tag names another version" fail "tag v0.2.0 is not v0.1.0" v0.2.0

setup
expect "tag without v" fail "tag 0.1.0 is not v0.1.0" 0.1.0

setup
expect "pre-release tag" fail "tag v0.1.0-rc.1 is not v0.1.0" v0.1.0-rc.1

setup
chart_field version 0.1.1
expect "chart version differs" fail "file=charts/honeybeepf-llm/Chart.yaml::version '0.1.1' differs from the agent crate's 0.1.0"

setup
chart_field appVersion '"0.2.0"'
expect "appVersion differs" fail "file=charts/honeybeepf-llm/Chart.yaml::appVersion '0.2.0' differs from the agent crate's 0.1.0"

setup
cargo_version 0.2.0
expect "crate bumped alone" fail "version '0.1.0' differs from the agent crate's 0.2.0" v0.2.0

setup
cargo_version 0.2.0-dev
chart_field version 0.2.0-dev
chart_field appVersion 0.2.0-dev
expect "pre-release crate version" fail "file=honeybeepf-llm/honeybeepf-llm/Cargo.toml::[package] version '0.2.0-dev' is not X.Y.Z"

setup
sed -i '0,/^version = /{/^version = /d}' "$root/honeybeepf-llm/honeybeepf-llm/Cargo.toml"
expect "dependency versions are not the crate version" fail "[package] version '' is not X.Y.Z"

setup
cargo_version 01.2.3
chart_field version 01.2.3
chart_field appVersion 01.2.3
expect "leading zero" fail "[package] version '01.2.3' is not X.Y.Z"

setup
printf 'dependencies:\n  - name: x\n    version: 9.9.9\n' >>"$root/charts/honeybeepf-llm/Chart.yaml"
expect "chart dependency versions are not the chart version" pass "0.1.0" v0.1.0

setup
chart_field appVersion '0.1.0 # pinned'
expect "trailing comment is not read as a version" fail "appVersion '' differs"

setup
expect "empty tag argument" fail "tag  is not v0.1.0" ""

setup
sed -i "s/^version = .*/version = '0.1.0'/" "$root/honeybeepf-llm/honeybeepf-llm/Cargo.toml"
expect "single-quoted crate version" fail "[package] version '' is not X.Y.Z"

exit "$failed"

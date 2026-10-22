#!/usr/bin/env bash
# Binary files can't be reviewed in a PR diff (xz-utils hid its backdoor in test fixtures), so each one
# lives under testdata/, is pinned by sha256 in its directory's README.md, and only test code may read it.
# Usage: ci/check-binary-provenance.sh [repo-dir]   (bash 4.4+)
set -euo pipefail

root=$(git -C "${1:-.}" rev-parse --show-toplevel)

# Binaries that need no README row: exact paths, one line per file, no patterns.
allowlist=(
  'docs/images/architecture.png'
)

# Build and deploy inputs: none of these may mention testdata or any binary file name.
build_paths=(
  ':(glob)**/build.rs'
  ':(glob)**/Cargo.toml'
  ':(glob)**/.cargo/**'
  ':(glob)**/Makefile'
  ':(glob)**/*.mk'
  ':(glob)**/xtask/**'
  ':(glob)**/Dockerfile*'
  ':(glob)**/*.dockerfile'
  ':(glob)**/docker-compose*'
  ':(glob)**/compose.y*ml'
  ':(glob)**/rust-toolchain*'
  ':(glob).github/workflows/**'
  ':(glob).github/actions/**'
  ':(glob)charts/**'
  ':(glob)ci/**'
  ':(glob)infra_module/**'
  ':(exclude)ci/check-binary-provenance.sh'
  ':(exclude)ci/test-check-binary-provenance.sh'
)

mod_re='^[[:space:]]*(pub(\([^)]*\))?[[:space:]]+)?mod[[:space:]]+[[:alnum:]_]+[[:space:]]*\{'
cfg_test_re='^[[:space:]]*#\[cfg\(test\)\]'

failed=0

# Prints a GitHub annotation; property and message escaping follows the workflow-command spec.
error() {
  local file=$1 msg=$2
  file=${file//%/%25}
  file=${file//$'\r'/%0D}
  file=${file//$'\n'/%0A}
  file=${file//:/%3A}
  file=${file//,/%2C}
  msg=${msg//%/%25}
  msg=${msg//$'\r'/%0D}
  msg=${msg//$'\n'/%0A}
  echo "::error file=$file${3:+,line=$3}::$msg"
  failed=1
}

trim() {
  local s=$1
  s=${s#"${s%%[![:space:]]*}"}
  s=${s%"${s##*[![:space:]]}"}
  printf '%s' "$s"
}

# git's heuristic only sees NULs in the first 8000 bytes, so also check for any NUL and for non-UTF-8.
is_binary() {
  local eol=$1 file=$root/$2
  [[ -f $file && ! -L $file ]] || return 1
  [[ $eol == *-text* ]] && return 0
  (($(tr -d '\000' <"$file" | wc -c) != $(wc -c <"$file"))) && return 0
  ! iconv -f UTF-8 -t UTF-8 "$file" >/dev/null 2>&1
}

allowed() {
  local listed
  for listed in "${allowlist[@]}"; do
    [[ $1 == "$listed" ]] && return 0
  done
  return 1
}

strip_ticks() {
  local s
  s=$(trim "$1")
  s=${s#\`}
  printf '%s' "${s%\`}"
}

# Prints the sha256 of every "| <name> | <sha256> | <command> |" row in the given README.
listed_hashes() {
  local readme=$1 name=$2 line sha
  local -a cells
  [[ -f $readme ]] || return 0
  while IFS= read -r line || [[ -n $line ]]; do
    [[ $line == \|* ]] || continue
    IFS='|' read -r -a cells <<<"$line"
    ((${#cells[@]} >= 4)) || continue
    sha=$(strip_ticks "${cells[2]}")
    if [[ $(strip_ticks "${cells[1]}") == "$name" && $sha =~ ^[0-9a-fA-F]{64}$ && -n $(trim "${cells[3]}") ]]; then
      echo "${sha,,}"
    fi
  done <"$readme"
}

# Reports each line of a file that mentions a needle. With rust=1, lines inside a
# `#[cfg(test)] mod … {` (until the `}` at its indent) or a `#![cfg(test)]` file are fine.
scan() {
  local file=$1 rust=$2 line lc needle n=0 pending=0 in_test=0 indent=''
  while IFS= read -r line || [[ -n $line ]]; do
    n=$((n + 1))
    if ((rust)); then
      if ((in_test)); then
        [[ $line =~ ^"$indent"\}[[:space:]]*$ ]] && in_test=0
        continue
      fi
      [[ $line =~ ^#!\[cfg\(test\)\] ]] && return 0
      if [[ $line =~ $cfg_test_re ]]; then
        indent=${line%%[![:space:]]*}
        line=${line#*cfg(test)\]}
        pending=1
      elif ((pending)); then
        indent=${line%%[![:space:]]*}
      fi
      if ((pending)) && [[ $line =~ $mod_re ]]; then
        pending=0
        [[ $line =~ \}[[:space:]]*$ ]] || in_test=1
        continue
      fi
      [[ $line =~ ^[[:space:]]*(#\[|//|$) ]] || pending=0
    fi
    lc=${line,,}
    for needle in "${needles[@]}"; do
      if [[ $lc == *"$needle"* ]]; then
        error "$file" "${label[rust]} mentions '$needle': $(trim "$line")" "$n"
        break
      fi
    done
  done <"$root/$file"
}
label=('build/deploy file' 'non-test Rust code')

while IFS= read -r -d '' entry; do
  if [[ $entry == 160000* ]]; then
    error "${entry#*$'\t'}" "submodules are not allowed; their files skip this check"
  fi
done < <(git -C "$root" ls-files -s -z)
wait "$!"

# A: every binary sits under testdata/ and is pinned in its directory's README.md.
binaries=0
needles=(testdata)
while IFS= read -r -d '' entry; do
  eol=${entry%%$'\t'*}
  path=${entry#*$'\t'}
  is_binary "$eol" "$path" || continue
  binaries=$((binaries + 1))
  name=${path##*/}
  needles+=("${name,,}")
  if allowed "$path"; then
    echo "allowlisted binary: $path"
    continue
  fi
  if [[ $path != testdata/* && $path != */testdata/* ]]; then
    error "$path" "binary file is outside a testdata/ directory"
  fi
  if [[ $path == */* ]]; then dir=${path%/*}; else dir=.; fi
  actual=$(sha256sum <"$root/$path")
  actual=${actual%% *}
  mapfile -t listed < <(listed_hashes "$root/$dir/README.md" "$name")
  if ((${#listed[@]} == 0)); then
    error "$path" "binary file has no '| $name | <sha256> | <command> |' row in $dir/README.md"
    continue
  fi
  for sha in "${listed[@]}"; do
    if [[ $sha != "$actual" ]]; then
      error "$path" "sha256 is $actual but $dir/README.md lists $sha"
      continue 2
    fi
  done
  echo "pinned binary: $path"
done < <(git -C "$root" ls-files --eol -z)
wait "$!"

grep_args=(grep -l -z -i -I -F)
for needle in "${needles[@]}"; do grep_args+=(-e "$needle"); done

# B1: build and deploy files never mention testdata or a binary file name.
declare -A build_file=()
while IFS= read -r -d '' file; do
  build_file[$file]=1
  scan "$file" 0
done < <(git -C "$root" "${grep_args[@]}" -- "${build_paths[@]}")
wait "$!" || [[ $? == 1 ]]

# B2: other Rust code mentions them only in test code; Cargo integration tests and benches are test code.
while IFS= read -r -d '' file; do
  [[ -z ${build_file[$file]:-} ]] || continue
  case $file in
    src/* | */src/*) ;;
    tests/* | */tests/* | benches/* | */benches/*) continue ;;
  esac
  scan "$file" 1
done < <(git -C "$root" "${grep_args[@]}" -- '*.rs')
wait "$!" || [[ $? == 1 ]]

if ((failed)); then
  echo "binary provenance check failed; see CONTRIBUTING.md (Binary Files)"
  exit 1
fi
echo "binary provenance check passed ($binaries binary files)"

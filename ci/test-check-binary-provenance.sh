#!/usr/bin/env bash
# Runs check-binary-provenance.sh against throwaway git repos, one per case.
set -euo pipefail

check="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/check-binary-provenance.sh"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
failed=0

# A crate whose one fixture is pinned and read only from its test module.
setup() {
  repo=$(mktemp -d "$tmp/repo.XXXXXX")
  git -C "$repo" init -q
  mkdir -p "$repo/crate/src/testdata"
  printf 'fix\0ture' >"$repo/crate/src/testdata/data.bin"
  local sha
  sha=$(sha256sum <"$repo/crate/src/testdata/data.bin")
  # shellcheck disable=SC2016 # the backticks are Markdown
  printf '| file | sha256 | command |\n|---|---|---|\n| data.bin | %s | `make-fixture > data.bin` |\n' \
    "${sha%% *}" >"$repo/crate/src/testdata/README.md"
  printf '[package]\nname = "crate"\n' >"$repo/crate/Cargo.toml"
  printf 'fn main() {}\n' >"$repo/crate/build.rs"
  cat >"$repo/crate/src/lib.rs" <<'EOF'
pub fn f() {}

#[cfg(test)]
mod tests {
    const DATA: &[u8] = include_bytes!("testdata/data.bin");
}
EOF
}

# expect <case> <pass|fail> [text the output must contain]
expect() {
  local got=pass
  git -C "$repo" add -Af
  bash "$check" "$repo" >"$tmp/out" 2>&1 || got=fail
  if [[ $got == "$2" && $(<"$tmp/out") == *"${3:-}"* ]]; then
    echo "ok - $1"
  else
    echo "not ok - $1 (got $got)"
    cat "$tmp/out"
    failed=1
  fi
}

lib() { cat >"$repo/crate/src/lib.rs"; }

setup
expect "pinned fixture read from cfg(test)" pass "pinned binary: crate/src/testdata/data.bin"

setup
rm "$repo/crate/src/testdata/README.md"
expect "missing README" fail "file=crate/src/testdata/data.bin::binary file has no"

setup
printf 'changed\0' >"$repo/crate/src/testdata/data.bin"
expect "sha256 mismatch" fail "file=crate/src/testdata/data.bin::sha256 is"

setup
printf '\xff\xfe no NUL byte\n' >"$repo/crate/src/testdata/other.bin"
expect "non-UTF-8 file without NUL is binary" fail "file=crate/src/testdata/other.bin"

setup
head -c 9000 /dev/zero | tr '\0' a >"$repo/crate/src/testdata/late.txt"
printf '\0' >>"$repo/crate/src/testdata/late.txt"
expect "NUL after git's 8000-byte window is binary" fail "file=crate/src/testdata/late.txt"

setup
mv "$repo/crate/src/testdata/data.bin" "$repo/crate/src/data.bin"
mv "$repo/crate/src/testdata/README.md" "$repo/crate/src/README.md"
expect "pinned binary outside testdata" fail "file=crate/src/data.bin::binary file is outside"

setup
printf 'fn main() { let _ = include_bytes!("src/testdata/data.bin"); }\n' >"$repo/crate/build.rs"
expect "build.rs reads testdata" fail "file=crate/build.rs,line=1"

setup
printf 'fn main() { let _ = include_bytes!(concat!("src/test", "data/data.bin")); }\n' >"$repo/crate/build.rs"
expect "build.rs reads a fixture by split path" fail "file=crate/build.rs,line=1::build/deploy file mentions 'data.bin'"

setup
mkdir -p "$repo/.github/workflows"
printf 'steps:\n  - run: cp crate/src/testdata/data.bin /tmp\n' >"$repo/.github/workflows/ci.yml"
expect "workflow reads testdata" fail "file=.github/workflows/ci.yml,line=2"

setup
lib <<'EOF'
pub const DATA: &[u8] = include_bytes!("testdata/data.bin");

#[cfg(test)]
mod tests {}
EOF
expect "include_bytes outside cfg(test)" fail "file=crate/src/lib.rs,line=1"

setup
lib <<'EOF'
#[cfg(test)]
mod tests {
    fn t() {}
}

pub const DATA: &[u8] = include_bytes!("testdata/data.bin");
EOF
expect "item after the test module" fail "file=crate/src/lib.rs,line=6"

setup
lib <<'EOF'
mod a {
    #[cfg(test)]
    mod t {}
}
pub static X: &[u8] = include_bytes!("testdata/data.bin");
EOF
expect "one-line test module in another module" fail "file=crate/src/lib.rs,line=5"

setup
lib <<'EOF'
#[cfg(not(test))]
mod real {
    pub const DATA: &[u8] = include_bytes!("testdata/data.bin");
}
EOF
expect "cfg(not(test)) module is not a test module" fail "file=crate/src/lib.rs,line=3"

setup
printf 'pub mod tests;\n' | lib
printf 'pub const D: &[u8] = include_bytes!("testdata/data.bin");\n' >"$repo/crate/src/tests.rs"
expect "tests.rs without cfg(test) is shipped code" fail "file=crate/src/tests.rs,line=1"

setup
printf '#![cfg(test)]\nconst D: &[u8] = include_bytes!("testdata/data.bin");\n' >"$repo/crate/src/café.rs"
expect "#![cfg(test)] file, non-ASCII path" pass

setup
mkdir -p "$repo/crate/tests"
printf 'const D: &[u8] = include_bytes!("../src/testdata/data.bin");\n' >"$repo/crate/tests/it.rs"
expect "integration test reads testdata" pass

setup
mkdir -p "$repo/docs/images"
printf '\x89PNG\r\n\x1a\n\0' >"$repo/docs/images/architecture.png"
expect "allowlisted doc image" pass "allowlisted binary: docs/images/architecture.png"

setup
mkdir -p "$repo/docs/images"
printf '\x89PNG\r\n\x1a\n\0' >"$repo/docs/images/logo.png"
expect "image not on the allowlist" fail "file=docs/images/logo.png"

setup
mkdir -p "$repo/docs/images"
printf '\x89PNG\r\n\x1a\n\0' >"$repo/docs/images/architecture.png"
printf 'COPY docs/images/architecture.png /app/\n' >"$repo/Dockerfile"
expect "Dockerfile reads an allowlisted image" fail "file=Dockerfile,line=1"

setup
mkdir -p "$repo/vendor/sub"
git -C "$repo" update-index --add --cacheinfo 160000,0123456789abcdef0123456789abcdef01234567,vendor/sub
expect "submodule" fail "file=vendor/sub::submodules are not allowed"

exit "$failed"

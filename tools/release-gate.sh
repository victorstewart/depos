#!/usr/bin/env bash
# Copyright 2026 Victor Stewart
# SPDX-License-Identifier: Apache-2.0

set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
smoke_build_dir="$repo_root/build/release-gate-smoke"
depos_executable="$repo_root/target/debug/depos"

require_tracked_path() {
  git -C "$repo_root" ls-files --error-unmatch -- "$1" >/dev/null 2>&1 || {
    echo "release-gate: required tracked path is missing from git index: $1" >&2
    exit 1
  }
}

rm -rf "$smoke_build_dir"

cleanup() {
  rm -rf "$smoke_build_dir"
}
trap cleanup EXIT

require_tracked_path ".depos.cmake"
require_tracked_path "cmake/depos.cmake"
require_tracked_path "depofiles/local/local_itoa/main/main.DepoFile"
require_tracked_path "tests/smoke/fixtures/local/bitsery/release/5.2.3/main.DepoFile"
require_tracked_path "tests/smoke/fixtures/local/itoa/release/main/main.DepoFile"
require_tracked_path "tests/smoke/fixtures/local/zlib/release/1.3.2/main.DepoFile"

cd "$repo_root/cli"
cargo test -j"$(nproc)"
cargo package --allow-dirty --locked --no-verify
cargo build --locked --bin depos -j"$(nproc)"

cmake \
  --fresh \
  -S "$repo_root/tests/smoke" \
  -B "$smoke_build_dir" \
  -G Ninja \
  -DDEPOS_EXECUTABLE="$depos_executable" \
  -DDEPOS_ROOT="$smoke_build_dir/depos-root" \
  -DDEPOS_SMOKE_STYLE=all
cmake --build "$smoke_build_dir" -j"$(nproc)"
ctest --test-dir "$smoke_build_dir" --output-on-failure -j"$(nproc)"

"$repo_root/tools/regenerate-local-depofiles.sh" --check

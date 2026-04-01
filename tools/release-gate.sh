#!/usr/bin/env bash
# Copyright 2026 Victor Stewart
# SPDX-License-Identifier: Apache-2.0

set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
smoke_build_dir="$repo_root/build/release-gate-smoke"
depos_root="/root/.depos"

rm -rf "$smoke_build_dir"

cleanup() {
  rm -rf "$smoke_build_dir"
}
trap cleanup EXIT

cd "$repo_root/cli"
cargo test -j"$(nproc)"
cargo package --allow-dirty --no-verify
cargo run -- sync --depos-root "$depos_root" --manifest "$repo_root/tests/smoke/manifest.cmake"

cmake -S "$repo_root/tests/smoke" -B "$smoke_build_dir" -G Ninja -DDEPOS_ROOT="$depos_root"
cmake --build "$smoke_build_dir" -j"$(nproc)"
ctest --test-dir "$smoke_build_dir" --output-on-failure -j"$(nproc)"

"$repo_root/tools/regenerate-local-depofiles.sh" --check

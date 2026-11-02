#!/usr/bin/env bash
set -Eeuo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"

if [[ "$(uname -s)" != Linux ]]
then
  echo "this probe requires a Linux guest" >&2
  exit 2
fi

mount_type="$(stat -f -c %T "${repo_root}")"
case "${mount_type}" in
  fuse|fuseblk|virtiofs) ;;
  *)
    echo "expected a FUSE/virtiofs-mounted Depos checkout, found ${mount_type}" >&2
    exit 2
    ;;
esac

target_dir="$(mktemp -d /tmp/depos-rootless-fuse-fallback.XXXXXX)"
trap 'rm -rf "${target_dir}"' EXIT

cd "${repo_root}"
DEPOS_LINUX_ISOLATION=auto CARGO_TARGET_DIR="${target_dir}" \
  cargo test --locked --offline --test integration \
    rootless_fuse_mount_system_build_falls_back -- --ignored --exact --nocapture

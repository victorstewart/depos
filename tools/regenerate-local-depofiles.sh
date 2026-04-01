#!/usr/bin/env bash
# Copyright 2026 Victor Stewart
# SPDX-License-Identifier: Apache-2.0

set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
schema_root="${SCHEMA_ROOT:-/root/nametag/libraries/schemas}"
local_root="${LOCAL_ROOT:-$repo_root/depofiles/local}"
check_only=0

if [[ "${1:-}" == "--check" ]]; then
  check_only=1
elif [[ $# -gt 0 ]]; then
  printf 'usage: %s [--check]\n' "$0" >&2
  exit 1
fi

if [[ ! -d "$schema_root" ]]; then
  printf 'schema root does not exist: %s\n' "$schema_root" >&2
  exit 1
fi

status=0
while IFS= read -r schema_file; do
  name="$(awk '$1 == "NAME" { print $2; exit }' "$schema_file")"
  version="$(awk '$1 == "VERSION" { print $2; exit }' "$schema_file")"
  if [[ -z "$name" || -z "$version" ]]; then
    printf 'failed to extract NAME/VERSION from %s\n' "$schema_file" >&2
    exit 1
  fi

  for namespace in nametag parsecheck; do
    namespace_root="$local_root/$name/$namespace"
    destination="$namespace_root/$version/main.DepoFile"
    if [[ $check_only -eq 1 ]]; then
      if [[ ! -f "$destination" ]] || ! cmp -s "$schema_file" "$destination"; then
        printf 'out of date: %s\n' "$destination" >&2
        status=1
      fi
      continue
    fi

    rm -rf "$namespace_root"
    mkdir -p "$(dirname -- "$destination")"
    cp "$schema_file" "$destination"
  done
done < <(find "$schema_root" -mindepth 2 -maxdepth 2 -name 'main.DepoFile' | sort)

exit "$status"

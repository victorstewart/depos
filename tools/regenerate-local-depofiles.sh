#!/usr/bin/env bash
# Copyright 2026 Victor Stewart
# SPDX-License-Identifier: Apache-2.0

set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
local_root="${LOCAL_ROOT:-$repo_root/depofiles/local}"
check_only=0

if [[ -n "${SCHEMA_ROOT:-}" ]]; then
  schema_root="$SCHEMA_ROOT"
elif [[ -d "$repo_root/.run/legacy-schema-extract/libraries/schemas" ]]; then
  schema_root="$repo_root/.run/legacy-schema-extract/libraries/schemas"
else
  schema_root=""
fi

if [[ "${1:-}" == "--check" ]]; then
  check_only=1
elif [[ $# -gt 0 ]]; then
  printf 'usage: %s [--check]\n' "$0" >&2
  exit 1
fi

if [[ -z "$schema_root" || ! -d "$schema_root" ]]; then
  source_namespace=""
  for candidate in nametag parsecheck; do
    if find "$local_root" -path "*/$candidate/*/main.DepoFile" -print -quit | grep -q .; then
      source_namespace="$candidate"
      break
    fi
  done
  if [[ -z "$source_namespace" ]]; then
    printf 'schema root does not exist and no local %s/%s depofiles were found under %s\n' \
      "nametag" "parsecheck" "$local_root" >&2
    exit 1
  fi
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
done < <(
  if [[ -n "$schema_root" && -d "$schema_root" ]]; then
    find "$schema_root" -mindepth 2 -maxdepth 2 -name 'main.DepoFile' | sort
  else
    find "$local_root" -path "*/$source_namespace/*/main.DepoFile" | sort
  fi
)

exit "$status"

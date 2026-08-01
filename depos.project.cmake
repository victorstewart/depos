# Copyright 2026 Victor Stewart
# SPDX-License-Identifier: Apache-2.0

if (NOT DEFINED DEPOS_BOOTSTRAP_VERSION OR DEPOS_BOOTSTRAP_VERSION STREQUAL "")
  # A source checkout bootstraps with the last published release, never itself.
  set(
    DEPOS_BOOTSTRAP_VERSION
    "0.5.4"
    CACHE STRING
    "Pinned depos version used by this project"
    FORCE
  )
endif()

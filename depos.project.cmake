# Copyright 2026 Victor Stewart
# SPDX-License-Identifier: Apache-2.0

if (NOT DEFINED DEPOS_BOOTSTRAP_VERSION OR DEPOS_BOOTSTRAP_VERSION STREQUAL "")
  set(
    DEPOS_BOOTSTRAP_VERSION
    "0.4.0"
    CACHE STRING
    "Pinned depos version used by this project"
    FORCE
  )
endif()

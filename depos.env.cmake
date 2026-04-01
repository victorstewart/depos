# Copyright 2026 Victor Stewart
# SPDX-License-Identifier: Apache-2.0

if (NOT DEFINED DEPOS_ROOT OR DEPOS_ROOT STREQUAL "")
  set(DEPOS_ROOT "$ENV{HOME}/.depos" CACHE PATH "Depo root" FORCE)
endif()

if (NOT DEFINED DEPOS_SYSTEM_LIBS OR DEPOS_SYSTEM_LIBS STREQUAL "")
  set(DEPOS_SYSTEM_LIBS "NEVER" CACHE STRING "Default system library policy for generated registries" FORCE)
endif()

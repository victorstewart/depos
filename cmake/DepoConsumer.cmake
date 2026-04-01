# Copyright 2026 Victor Stewart
# SPDX-License-Identifier: Apache-2.0

include_guard(GLOBAL)
include("${CMAKE_CURRENT_LIST_DIR}/DepoConfig.cmake")

function(depos_use)
  set(options)
  set(oneValueArgs MANIFEST REGISTRY_DIR)
  cmake_parse_arguments(DEPOS_USE "${options}" "${oneValueArgs}" "" ${ARGN})

  if ("${DEPOS_USE_MANIFEST}" STREQUAL "" AND "${DEPOS_USE_REGISTRY_DIR}" STREQUAL "")
    message(FATAL_ERROR "depos_use requires MANIFEST or REGISTRY_DIR.")
  endif()

  if (NOT "${DEPOS_USE_REGISTRY_DIR}" STREQUAL "")
    set(_depos_registry_dir "${DEPOS_USE_REGISTRY_DIR}")
  else()
    depos_registry_dir(_depos_registry_dir "${DEPOS_USE_MANIFEST}")
  endif()

  set(_depos_targets_file "${_depos_registry_dir}/targets.cmake")
  if (NOT EXISTS "${_depos_targets_file}")
    message(FATAL_ERROR "Depo registry file is missing: ${_depos_targets_file}. Run `depos sync` first.")
  endif()

  include("${_depos_targets_file}")
endfunction()

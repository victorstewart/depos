# Copyright 2026 Victor Stewart
# SPDX-License-Identifier: Apache-2.0

include_guard(GLOBAL)

function(_depos_repo_root out_var)
  get_filename_component(_depos_root "${CMAKE_CURRENT_LIST_DIR}/.." ABSOLUTE)
  set(${out_var} "${_depos_root}" PARENT_SCOPE)
endfunction()

function(depos_default_root out_var)
  _depos_repo_root(_depos_root)
  if (EXISTS "${_depos_root}/depos.env.cmake")
    include("${_depos_root}/depos.env.cmake" OPTIONAL)
  endif()

  if (DEFINED DEPOS_ROOT AND NOT "${DEPOS_ROOT}" STREQUAL "")
    set(_depos_runtime_root "${DEPOS_ROOT}")
  elseif (DEFINED ENV{DEPOS_ROOT} AND NOT "$ENV{DEPOS_ROOT}" STREQUAL "")
    set(_depos_runtime_root "$ENV{DEPOS_ROOT}")
  elseif (DEFINED ENV{HOME} AND NOT "$ENV{HOME}" STREQUAL "")
    set(_depos_runtime_root "$ENV{HOME}/.depos")
  else()
    message(FATAL_ERROR "Unable to determine DEPOS_ROOT.")
  endif()

  set(${out_var} "${_depos_runtime_root}" PARENT_SCOPE)
endfunction()

function(depos_default_system_libs out_var)
  _depos_repo_root(_depos_root)
  if (EXISTS "${_depos_root}/depos.env.cmake")
    include("${_depos_root}/depos.env.cmake" OPTIONAL)
  endif()

  if (DEFINED DEPOS_SYSTEM_LIBS AND NOT "${DEPOS_SYSTEM_LIBS}" STREQUAL "")
    set(${out_var} "${DEPOS_SYSTEM_LIBS}" PARENT_SCOPE)
  elseif (DEFINED ENV{DEPOS_SYSTEM_LIBS} AND NOT "$ENV{DEPOS_SYSTEM_LIBS}" STREQUAL "")
    set(${out_var} "$ENV{DEPOS_SYSTEM_LIBS}" PARENT_SCOPE)
  else()
    set(${out_var} "NEVER" PARENT_SCOPE)
  endif()
endfunction()

function(depos_default_variant out_var)
  string(TOLOWER "${CMAKE_SYSTEM_PROCESSOR}" _depos_arch)
  if (_depos_arch STREQUAL "")
    string(TOLOWER "${CMAKE_HOST_SYSTEM_PROCESSOR}" _depos_arch)
  endif()
  if (_depos_arch STREQUAL "")
    execute_process(
      COMMAND uname -m
      OUTPUT_VARIABLE _depos_arch
      OUTPUT_STRIP_TRAILING_WHITESPACE
      ERROR_QUIET
    )
    string(TOLOWER "${_depos_arch}" _depos_arch)
  endif()
  if (_depos_arch STREQUAL "")
    set(_depos_arch "unknown")
  endif()
  if (_depos_arch STREQUAL "amd64")
    set(_depos_arch "x86_64")
  endif()
  if (_depos_arch STREQUAL "arm64")
    set(_depos_arch "aarch64")
  endif()
  set(${out_var} "${_depos_arch}-${_depos_arch}_v1" PARENT_SCOPE)
endfunction()

function(depos_manifest_profile out_var manifest_path)
  file(SHA256 "${manifest_path}" _depos_hash)
  string(SUBSTRING "${_depos_hash}" 0 16 _depos_short_hash)
  set(${out_var} "manifest-${_depos_short_hash}" PARENT_SCOPE)
endfunction()

function(depos_registry_dir out_var manifest_path)
  depos_default_root(_depos_root)
  depos_default_variant(_depos_variant)
  depos_manifest_profile(_depos_profile "${manifest_path}")
  set(${out_var} "${_depos_root}/registry/${_depos_variant}/${_depos_profile}" PARENT_SCOPE)
endfunction()

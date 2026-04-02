# Copyright 2026 Victor Stewart
# SPDX-License-Identifier: Apache-2.0

cmake_minimum_required(VERSION 3.21)
project(depos_smoke LANGUAGES CXX)

if (NOT DEFINED DEPOS_ROOT OR DEPOS_ROOT STREQUAL "")
  set(DEPOS_ROOT "${CMAKE_BINARY_DIR}/depos-root" CACHE PATH "Smoke-test depos root" FORCE)
endif()

file(REMOVE_RECURSE "${DEPOS_ROOT}")
file(MAKE_DIRECTORY "${DEPOS_ROOT}/depofiles")
file(COPY "${CMAKE_CURRENT_LIST_DIR}/fixtures/local" DESTINATION "${DEPOS_ROOT}/depofiles")
if (WIN32)
  set(_depos_zlib_depofile "${DEPOS_ROOT}/depofiles/local/zlib/release/1.3.2/main.DepoFile")
  file(READ "${_depos_zlib_depofile}" _depos_zlib_depofile_contents)
  string(
    REPLACE
    "TARGET zlib::zlib STATIC lib/libz.a INTERFACE include"
    "TARGET zlib::zlib STATIC lib/zs.lib INTERFACE include"
    _depos_zlib_depofile_contents
    "${_depos_zlib_depofile_contents}"
  )
  file(WRITE "${_depos_zlib_depofile}" "${_depos_zlib_depofile_contents}")
endif()

include("${CMAKE_CURRENT_LIST_DIR}/../../.depos.cmake")
set(DEPOS_SMOKE_STYLE "all" CACHE STRING "Smoke style: all, explicit, or files")
if (NOT DEFINED DEPOS_DEPOFILES_DIR OR DEPOS_DEPOFILES_DIR STREQUAL "")
  set(DEPOS_DEPOFILES_DIR "${DEPOS_ROOT}/depofiles/local" CACHE PATH "Smoke depofile directory" FORCE)
endif()

if (DEPOS_SMOKE_STYLE STREQUAL "all")
  depos_depend_all()
elseif (DEPOS_SMOKE_STYLE STREQUAL "explicit")
  depos_depend(bitsery)
  depos_depend(itoa)
  depos_depend(zlib VERSION 1.3.2)
elseif (DEPOS_SMOKE_STYLE STREQUAL "files")
  depos_depend(
    FILES
    "${DEPOS_DEPOFILES_DIR}/bitsery/release/5.2.3/main.DepoFile"
    "${DEPOS_DEPOFILES_DIR}/itoa/release/main/main.DepoFile"
    "${DEPOS_DEPOFILES_DIR}/zlib/release/1.3.2/main.DepoFile"
  )
else()
  message(FATAL_ERROR "Unsupported DEPOS_SMOKE_STYLE: ${DEPOS_SMOKE_STYLE}")
endif()

add_executable(depos-smoke main.cpp)
if (DEPOS_SMOKE_STYLE STREQUAL "all")
  depos_link_all(depos-smoke)
else()
  depos_link(depos-smoke bitsery itoa zlib)
endif()
target_compile_features(depos-smoke PRIVATE cxx_std_20)

enable_testing()
add_test(NAME depos-smoke COMMAND depos-smoke)

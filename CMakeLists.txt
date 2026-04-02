# Copyright 2026 Victor Stewart
# SPDX-License-Identifier: Apache-2.0

cmake_minimum_required(VERSION 3.21)
project(depos LANGUAGES NONE)

include(CTest)
include("${CMAKE_CURRENT_SOURCE_DIR}/cmake/DepoConfig.cmake")

find_program(CARGO_EXECUTABLE cargo REQUIRED)

set(DEPOS_CLI_SOURCE_DIR "${CMAKE_CURRENT_SOURCE_DIR}/cli")
set(DEPOS_CLI_TARGET_DIR "${CMAKE_CURRENT_BINARY_DIR}/cargo")
set(DEPOS_CLI_OUTPUT_DIR "${CMAKE_CURRENT_BINARY_DIR}/bin")
set(DEPOS_CLI_OUTPUT_BINARY "${DEPOS_CLI_OUTPUT_DIR}/depos${CMAKE_EXECUTABLE_SUFFIX}")
set(DEPOS_CLI_BUILT_BINARY "${DEPOS_CLI_TARGET_DIR}/debug/depos${CMAKE_EXECUTABLE_SUFFIX}")

file(GLOB_RECURSE DEPOS_CLI_SOURCES
  "${DEPOS_CLI_SOURCE_DIR}/src/*.rs"
  "${DEPOS_CLI_SOURCE_DIR}/Cargo.toml"
  "${DEPOS_CLI_SOURCE_DIR}/Cargo.lock"
)

add_custom_command(
  OUTPUT "${DEPOS_CLI_OUTPUT_BINARY}"
  COMMAND "${CMAKE_COMMAND}" -E make_directory "${DEPOS_CLI_OUTPUT_DIR}"
  COMMAND "${CMAKE_COMMAND}" -E env
          "CARGO_TARGET_DIR=${DEPOS_CLI_TARGET_DIR}"
          "${CARGO_EXECUTABLE}" build --locked --manifest-path "${DEPOS_CLI_SOURCE_DIR}/Cargo.toml" --bin depos
  COMMAND "${CMAKE_COMMAND}" -E copy_if_different
          "${DEPOS_CLI_BUILT_BINARY}"
          "${DEPOS_CLI_OUTPUT_BINARY}"
  DEPENDS ${DEPOS_CLI_SOURCES}
  WORKING_DIRECTORY "${DEPOS_CLI_SOURCE_DIR}"
  VERBATIM
)

add_custom_target(depos ALL DEPENDS "${DEPOS_CLI_OUTPUT_BINARY}")

if (BUILD_TESTING)
  set(DEPOS_TEST_TOOLS_DIR "${CMAKE_CURRENT_BINARY_DIR}/test-tools")
  set(DEPOS_TEST_FAKE_CARGO_DIR "${DEPOS_TEST_TOOLS_DIR}/fake-cargo")
  file(MAKE_DIRECTORY "${DEPOS_TEST_FAKE_CARGO_DIR}")
  if (WIN32)
    set(DEPOS_TEST_FAKE_CARGO "${DEPOS_TEST_FAKE_CARGO_DIR}/cargo.cmd")
    file(WRITE "${DEPOS_TEST_FAKE_CARGO}" [=[
@echo off
setlocal EnableExtensions EnableDelayedExpansion

set "root="
set "version="
set "crate="

:loop
if "%~1"=="" goto after
if "%~1"=="install" shift & goto loop
if "%~1"=="--locked" shift & goto loop
if "%~1"=="--root" (
  set "root=%~2"
  shift
  shift
  goto loop
)
if "%~1"=="--version" (
  set "version=%~2"
  shift
  shift
  goto loop
)
if "%~1"=="-j" (
  shift
  shift
  goto loop
)
if "%~1"=="depos" (
  set "crate=%~1"
  shift
  goto loop
)
echo unexpected cargo argument: %~1 1>&2
exit /b 1

:after
if "%root%"=="" exit /b 1
if not "%version%"=="0.4.0" exit /b 1
if not "%crate%"=="depos" exit /b 1
if not exist "%root%\\bin" mkdir "%root%\\bin"
copy /Y "]=] "${DEPOS_CLI_OUTPUT_BINARY}" [=[" "%root%\\bin\\depos.exe" >NUL
]=])
  else()
    set(DEPOS_TEST_FAKE_CARGO "${DEPOS_TEST_FAKE_CARGO_DIR}/cargo")
    file(WRITE "${DEPOS_TEST_FAKE_CARGO}" [=[
#!/usr/bin/env bash
set -euo pipefail

if [[ "${1:-}" != "install" ]]; then
  echo "unexpected cargo invocation: $*" >&2
  exit 1
fi

root=""
version=""
crate=""
while (($#)); do
  case "$1" in
    install|--locked)
      shift
      ;;
    --root)
      root="$2"
      shift 2
      ;;
    --version)
      version="$2"
      shift 2
      ;;
    -j)
      shift 2
      ;;
    depos)
      crate="$1"
      shift
      ;;
    *)
      echo "unexpected cargo argument: $1" >&2
      exit 1
      ;;
  esac
done

test -n "$root"
test "$version" = "0.4.0"
test "$crate" = "depos"
mkdir -p "$root/bin"
cp ]=] "${DEPOS_CLI_OUTPUT_BINARY}" [=[ "$root/bin/depos"
]=])
    file(CHMOD "${DEPOS_TEST_FAKE_CARGO}" PERMISSIONS OWNER_READ OWNER_WRITE OWNER_EXECUTE GROUP_READ GROUP_EXECUTE WORLD_READ WORLD_EXECUTE)
  endif()

  set(DEPOS_SMOKE_SOURCE_DIR "${CMAKE_CURRENT_SOURCE_DIR}/tests/smoke")

  set(DEPOS_SMOKE_EXPLICIT_BINARY_DIR "${CMAKE_CURRENT_BINARY_DIR}/tests/smoke-explicit")
  add_test(
    NAME depos-cmake-explicit-configure
    COMMAND
      "${CMAKE_COMMAND}"
      --fresh
      -S "${DEPOS_SMOKE_SOURCE_DIR}"
      -B "${DEPOS_SMOKE_EXPLICIT_BINARY_DIR}"
      -DDEPOS_EXECUTABLE=${DEPOS_CLI_OUTPUT_BINARY}
      -DDEPOS_ROOT=${DEPOS_SMOKE_EXPLICIT_BINARY_DIR}/depos-root
      -DDEPOS_SMOKE_STYLE=explicit
  )
  add_test(
    NAME depos-cmake-explicit-build
    COMMAND "${CMAKE_COMMAND}" --build "${DEPOS_SMOKE_EXPLICIT_BINARY_DIR}" --parallel
  )
  set_tests_properties(depos-cmake-explicit-build PROPERTIES DEPENDS depos-cmake-explicit-configure)
  add_test(
    NAME depos-cmake-explicit-run
    COMMAND
      "${CMAKE_CTEST_COMMAND}"
      --test-dir "${DEPOS_SMOKE_EXPLICIT_BINARY_DIR}"
      --output-on-failure
      --no-tests=error
  )
  set_tests_properties(depos-cmake-explicit-run PROPERTIES DEPENDS depos-cmake-explicit-build)

  set(DEPOS_SMOKE_BOOTSTRAP_BINARY_DIR "${CMAKE_CURRENT_BINARY_DIR}/tests/smoke-bootstrap")
  add_test(
    NAME depos-cmake-bootstrap-configure
    COMMAND
      "${CMAKE_COMMAND}"
      --fresh
      -S "${DEPOS_SMOKE_SOURCE_DIR}"
      -B "${DEPOS_SMOKE_BOOTSTRAP_BINARY_DIR}"
      -DDEPOS_SMOKE_STYLE=all
      -DDEPOS_BOOTSTRAP_DIR=${DEPOS_SMOKE_BOOTSTRAP_BINARY_DIR}/.depos
      -DDEPOS_ALLOW_SYSTEM_EXECUTABLE=OFF
      -DCMAKE_PROGRAM_PATH=${DEPOS_TEST_FAKE_CARGO_DIR}
  )
  add_test(
    NAME depos-cmake-bootstrap-build
    COMMAND "${CMAKE_COMMAND}" --build "${DEPOS_SMOKE_BOOTSTRAP_BINARY_DIR}" --parallel
  )
  set_tests_properties(depos-cmake-bootstrap-build PROPERTIES DEPENDS depos-cmake-bootstrap-configure)
  add_test(
    NAME depos-cmake-bootstrap-run
    COMMAND
      "${CMAKE_CTEST_COMMAND}"
      --test-dir "${DEPOS_SMOKE_BOOTSTRAP_BINARY_DIR}"
      --output-on-failure
      --no-tests=error
  )
  set_tests_properties(depos-cmake-bootstrap-run PROPERTIES DEPENDS depos-cmake-bootstrap-build)

  set(DEPOS_SMOKE_INVALID_ALL_FILE_BINARY_DIR "${CMAKE_CURRENT_BINARY_DIR}/tests/smoke-invalid-all-file")
  set(DEPOS_SMOKE_INVALID_ALL_FILE "${DEPOS_SMOKE_SOURCE_DIR}/fixtures/local/zlib/release/1.3.2/main.DepoFile")
  add_test(
    NAME depos-cmake-depend-all-file-rejected
    COMMAND
      "${CMAKE_COMMAND}"
      --fresh
      -S "${DEPOS_SMOKE_SOURCE_DIR}"
      -B "${DEPOS_SMOKE_INVALID_ALL_FILE_BINARY_DIR}"
      -DDEPOS_EXECUTABLE=${DEPOS_CLI_OUTPUT_BINARY}
      -DDEPOS_ROOT=${DEPOS_SMOKE_INVALID_ALL_FILE_BINARY_DIR}/depos-root
      -DDEPOS_SMOKE_STYLE=all
      -DDEPOS_DEPOFILES_DIR=${DEPOS_SMOKE_INVALID_ALL_FILE}
      -DDEPOS_ALLOW_SYSTEM_EXECUTABLE=OFF
      -DDEPOS_ALLOW_CARGO_BOOTSTRAP=OFF
  )
  set_tests_properties(
    depos-cmake-depend-all-file-rejected
    PROPERTIES
      WILL_FAIL TRUE
      PASS_REGULAR_EXPRESSION "depos_depend_all expects a depofiles directory path"
  )
endif()

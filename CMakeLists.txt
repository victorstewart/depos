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
set(DEPOS_CLI_OUTPUT_BINARY "${DEPOS_CLI_OUTPUT_DIR}/depos")

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
          "${DEPOS_CLI_TARGET_DIR}/debug/depos"
          "${DEPOS_CLI_OUTPUT_BINARY}"
  DEPENDS ${DEPOS_CLI_SOURCES}
  WORKING_DIRECTORY "${DEPOS_CLI_SOURCE_DIR}"
  VERBATIM
)

add_custom_target(depos ALL DEPENDS "${DEPOS_CLI_OUTPUT_BINARY}")

if (BUILD_TESTING)
  add_test(
    NAME depos-smoke-sync
    COMMAND "${DEPOS_CLI_OUTPUT_BINARY}" sync
            --depos-root "${CMAKE_CURRENT_SOURCE_DIR}"
            --manifest "${CMAKE_CURRENT_SOURCE_DIR}/manifests/example.cmake"
  )
endif()

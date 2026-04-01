# Copyright 2026 Victor Stewart
# SPDX-License-Identifier: Apache-2.0

cmake_minimum_required(VERSION 3.21)
project(depos_smoke LANGUAGES CXX)

include("/root/depos/cmake/DepoConsumer.cmake")
depos_use(MANIFEST "${CMAKE_CURRENT_SOURCE_DIR}/manifest.cmake")

add_executable(depos-smoke main.cpp)
target_link_libraries(depos-smoke PRIVATE bitsery::bitsery itoa::itoa zlib::zlib)
target_compile_features(depos-smoke PRIVATE cxx_std_20)

enable_testing()
add_test(NAME depos-smoke COMMAND depos-smoke)

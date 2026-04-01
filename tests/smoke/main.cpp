// Copyright 2026 Victor Stewart
// SPDX-License-Identifier: Apache-2.0

#include <cstdint>
#include <bitsery/bitsery.h>
#include <itoa/jeaiii_to_text.h>
#include <zlib.h>

int main() {
  const char* version = zlibVersion();
  return (version != nullptr && version[0] != '\0') ? 0 : 1;
}

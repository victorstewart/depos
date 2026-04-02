# depos

`depos` installs the `depos` executable.

`depos` is an external dependency builder and repository manager for C and C++ projects. It is intended to be used as a tool binary, not primarily as a Rust library dependency. It reuses unchanged local materializations and cached sources so repeated syncs do not rebuild everything from scratch. Changing the registered `DepoFile`, the resolved source, or a local dependency forces rematerialization.

## Install

```bash
cargo install depos --version 0.4.0
```

Primary distribution is GitHub release binaries. `cargo install depos --version 0.4.0` is the convenience path.

Preferred resolution order for consuming projects:

1. default project-local `cargo install depos --version 0.4.0`
2. explicit override such as `DEPOS_EXECUTABLE`
3. optional system `depos` on `PATH` if enabled by the project

## Root

`depos` uses one root, `depos_root`. It defaults to `~/.depos` on Unix and `%USERPROFILE%\.depos` on Windows.

That root holds:

- registered `DepoFile`s
- the materialized package store
- generated CMake registries

## Quick start

```bash
depos register --file /path/to/main.DepoFile --namespace release
depos sync --manifest /path/to/deps.cmake
```

Minimal CMake style:

```cmake
include("/path/to/.depos.cmake")
depos_depend_all()
depos_link_all(app)
```

Explicit CMake style:

```cmake
include("/path/to/.depos.cmake")
depos_depend(bitsery)
depos_depend(itoa)
depos_depend(zlib VERSION 1.3.2)
depos_link(app bitsery itoa zlib)
```

`depos_depend(...)` and `depos_depend_all(...)` queue requests during configure and `.depos.cmake` syncs them once, lazily, on the first `depos_link(...)` or `depos_link_all(...)` that needs the registry. Imported targets from queued requests are not guaranteed to exist until that first `depos_link*` call or `depos_use(MANIFEST ...)` performs the sync. `depos_link(...)` and `depos_link_all(...)` default to `PUBLIC`; pass `PRIVATE` immediately after the target name if you want to stop propagation:

```cmake
depos_depend(
  FILES
  "${CMAKE_CURRENT_SOURCE_DIR}/third_party/depofiles/zlib.DepoFile"
  "${CMAKE_CURRENT_SOURCE_DIR}/third_party/depofiles/openssl.DepoFile"
)
depos_link(app PRIVATE zlib)
```

For the full CMake contract, including `FILE`/`FILES`, `depos_depend_all(...)`, project-local defaults, and source-tree consumption details, see [README.md](/root/depos/README.md).

Libraries that ship with `depos` support two consumer modes:

- source-tree consumption: ship top-level `.depos.cmake` plus public top-level `depofiles/`, and builders can configure the library directly without touching `depos` themselves
- published-depofile consumption: a downstream project can depend on the library package by name or point at the library's published `DepoFile`, and the library's dependency graph cascades to the final binary

```cmake
depos_depend(cascade_lib VERSION 1.0.0)
depos_link(app cascade_lib)
```

During configure, `.depos.cmake` emits `depos:` status lines while it bootstraps the tool, queues dependency requests, and performs the one lazy registry sync before first use so dependency work does not look stalled. For the full integration contract and examples, see [README.md](/root/depos/README.md).

If you do not want CMake to bootstrap `depos 0.4.0` locally, tell builders to install it ahead of time:

```bash
cargo install depos --version 0.4.0
```

## Current scope

Package build execution works natively on Linux, macOS, and Windows.

Current backend contract:

- Linux: `BUILD_ROOT SYSTEM`, `BUILD_ROOT SCRATCH`, `BUILD_ROOT OCI <ref>`, `TOOLCHAIN ROOTFS`, and foreign-architecture OCI execution
- macOS: `BUILD_ROOT SYSTEM` only in this pass
- Windows: `BUILD_ROOT SYSTEM` only in this pass

On macOS and Windows, `depos` rejects the Linux-only runtime features with clear errors:

- `BUILD_ROOT SCRATCH`
- `BUILD_ROOT OCI <ref>`
- `TOOLCHAIN ROOTFS`
- `BUILD_ARCH != TARGET_ARCH`

The OpenSSL example below is Linux-oriented. On macOS and Windows, keep `BUILD_ROOT SYSTEM` and use host-native build commands for that platform instead of Linux-only isolation directives.

Example `DepoFile`:

```text
NAME openssl
VERSION 3.4.1
SOURCE URL https://github.com/openssl/openssl/archive/refs/tags/openssl-3.4.1.tar.gz
BUILD_SYSTEM AUTOCONF
AUTOCONF_CONFIGURE ./Configure "linux-${DEPO_TARGET_ARCH}" --prefix="${DEPO_PREFIX}" --libdir=lib --openssldir="${DEPO_PREFIX}/ssl" no-quic no-tests no-docs no-shared
AUTOCONF_BUILD make -j$(nproc) libcrypto.a libssl.a
AUTOCONF_INSTALL make install_dev DESTDIR=
TARGET openssl INTERFACE include
TARGET openssl::crypto STATIC lib/libcrypto.a
TARGET openssl::ssl STATIC lib/libssl.a
LINK openssl openssl::crypto openssl::ssl
```

See the repository README for the full usage contract and examples.

## License

Apache-2.0

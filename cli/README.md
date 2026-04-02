# depos

`depos` installs the `depos` executable.

`depos` is an external dependency builder and repository manager for C and C++ projects. It is intended to be used as a tool binary, not primarily as a Rust library dependency.

## Install

```bash
cargo install depos --version 0.3.0
```

Primary distribution is GitHub release binaries. `cargo install depos --version 0.3.0` is the convenience path.

Preferred resolution order for consuming projects:

1. default project-local `cargo install depos --version 0.3.0`
2. explicit override such as `DEPOS_EXECUTABLE`
3. optional system `depos` on `PATH` if enabled by the project

## Root

`depos` uses one root, `depos_root`. It defaults to `~/.depos`.

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

`depos_depend(...)` can also take a single `DepoFile` path, and `depos_depend_all(...)` can take a depofiles directory path only. `depos_link(...)` and `depos_link_all(...)` default to `PUBLIC`; pass `PRIVATE` immediately after the target name if you want to stop propagation:

```cmake
depos_link(app PRIVATE zlib)
```

Libraries that ship with `depos` support two consumer modes:

- source-tree consumption: ship top-level `.depos.cmake` plus public top-level `depofiles/`, and builders can configure the library directly without touching `depos` themselves
- published-depofile consumption: a downstream project can depend on the library package by name or point at the library's published `DepoFile`, and the library's dependency graph cascades to the final binary

```cmake
depos_depend(cascade_lib VERSION 1.0.0)
depos_link(app cascade_lib)
```

During configure, `.depos.cmake` emits `depos:` status lines while it bootstraps the tool, registers local `DepoFile`s, and syncs the registry so dependency work does not look stalled. Library maintainers should install `.depos.cmake` at the top of the repo before publishing and keep dependency `DepoFile`s in the public top-level `depofiles/` directory beside it so consumer builds can include the hidden helper there, self-bootstrap, and just work.

If you do not want CMake to bootstrap `depos 0.3.0` locally, tell builders to install it ahead of time:

```bash
cargo install depos --version 0.3.0
```

## Current scope

Package build execution is Linux-only today.

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

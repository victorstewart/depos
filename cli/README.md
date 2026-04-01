# depos-cli

`depos-cli` installs the `depos` executable.

`depos` is an external dependency builder and repository manager for C and C++ projects. It is intended to be used as a tool binary, not primarily as a Rust library dependency.

## Install

```bash
cargo install depos-cli
```

Primary distribution is GitHub release binaries. `cargo install depos-cli` is the convenience path.

Preferred resolution order for consuming projects:

1. explicit override such as `DEPOS_EXECUTABLE`
2. compatible `depos` on `PATH`
3. pinned project-local binary download

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

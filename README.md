# depos

`depos` is an external dependency builder and repository manager for C and C++ projects.

You describe a dependency once in a small line-oriented `DepoFile`. `depos` fetches the source, builds it in an isolated Linux runtime, publishes the result into a versioned repository, and generates a CMake registry that consumer projects can import from a manifest.

## Distribution contract

`depos` is meant to be used as a tool binary.

Preferred resolution order for consumer projects:

1. explicit override such as `DEPOS_EXECUTABLE`
2. compatible `depos` on `PATH`
3. pinned project-local binary download

Primary distribution is GitHub release binaries. `cargo install depos-cli` is a convenience path.

## Root

`depos` uses one root, `depos_root`. It defaults to `~/.depos`.

Registered recipes, materialized packages, and generated registries all live under that same root:

```text
depos_root/
  depofiles/local/<name>/<namespace>/<version>/main.DepoFile
  store/<variant>/<name>/<namespace>/<version>/...
  registry/<variant>/<manifest-profile>/...
```

## Quick start

Register a recipe:

```bash
depos register --file /path/to/main.DepoFile --namespace release
```

Sync a manifest:

```bash
depos sync --manifest /path/to/deps.cmake
```

Project-local roots:

```bash
depos register --depos-root "$PWD/.deps/depos" --file /path/to/main.DepoFile --namespace release
depos sync --depos-root "$PWD/.deps/depos" --manifest /path/to/deps.cmake
```

CMake consumer:

```cmake
include("/path/to/depos/cmake/DepoConsumer.cmake")
# Set DEPOS_ROOT if you are using a non-default root.
# set(DEPOS_ROOT "${CMAKE_SOURCE_DIR}/.deps/depos")
depos_use(MANIFEST "${CMAKE_SOURCE_DIR}/deps.cmake")
target_link_libraries(app PRIVATE zlib::zlib)
```

Manifest example:

```cmake
depos_require(zlib VERSION 1.3.2)
depos_require(openssl VERSION 3.4.1)
```

## Example `DepoFile`s

Simple package:

```text
NAME openssl
VERSION 3.4.1
SYSTEM_LIBS ALLOW
SOURCE URL https://github.com/openssl/openssl/archive/refs/tags/openssl-3.4.1.tar.gz
BUILD_SYSTEM AUTOCONF
AUTOCONF_CONFIGURE ./Configure "linux-${DEPO_TARGET_ARCH}" --prefix="${DEPO_PREFIX}" --libdir=lib --openssldir=/usr/local/ssl no-quic no-tests no-docs no-shared
AUTOCONF_BUILD make -j$(nproc) libcrypto.a libssl.a
AUTOCONF_INSTALL make install_dev DESTDIR=
TARGET openssl INTERFACE include
TARGET openssl::crypto STATIC lib/libcrypto.a
TARGET openssl::ssl STATIC lib/libssl.a
LINK openssl openssl::crypto openssl::ssl
```

A genuinely strong use of `BUILD_ROOT OCI` is when the package you need to publish belongs to a different deployment contract than the host machine.

The clearest example is cross-target output: your CI runners are `x86_64`, but you need to publish an `aarch64` package that will be consumed on Ubuntu 22.04. Building directly on the host is the wrong contract. The host distro, host filesystem layout, and host architecture are not the thing you are trying to reproduce.

OCI mode gives you a pinned Ubuntu rootfs for the build environment, and `depos` can then mount only the host-side cross toolchain you intentionally trust. That is useful for a real reason, not a cosmetic one:

- the recipe runs against the target distro rootfs instead of whatever Linux userspace the CI runner happens to have
- the package lands in the `aarch64` store variant, not the host one
- you do not have to provision every laptop or CI worker as an Ubuntu `aarch64` machine just to build the dependency

Build an Ubuntu 22.04 `aarch64` OpenSSL package from `x86_64` CI:

```text
NAME openssl_ubuntu_2204_aarch64
VERSION 3.4.1
SYSTEM_LIBS NEVER

# Assume the host already provides an aarch64 GNU cross toolchain.
# The build runs inside Ubuntu 22.04; the host only contributes the mounted toolchain.
BUILD_ROOT OCI docker.io/library/ubuntu:22.04
TOOLCHAIN ROOTFS
BUILD_ARCH x86_64
TARGET_ARCH aarch64
TOOLCHAIN_INPUT /usr/bin
TOOLCHAIN_INPUT /usr/lib
TOOLCHAIN_INPUT /lib
TOOLCHAIN_INPUT /lib64

SOURCE URL https://github.com/openssl/openssl/archive/refs/tags/openssl-3.4.1.tar.gz

BUILD_SYSTEM AUTOCONF
AUTOCONF_CONFIGURE env CC=aarch64-linux-gnu-gcc AR=aarch64-linux-gnu-ar RANLIB=aarch64-linux-gnu-ranlib STRIP=aarch64-linux-gnu-strip ./Configure linux-aarch64 --prefix="${DEPO_PREFIX}" --libdir=lib --openssldir=/usr/local/ssl no-quic no-tests no-docs no-shared
AUTOCONF_BUILD make -j$(nproc) libcrypto.a libssl.a
AUTOCONF_INSTALL make install_dev DESTDIR=

TARGET openssl INTERFACE include
TARGET openssl::crypto STATIC lib/libcrypto.a
TARGET openssl::ssl STATIC lib/libssl.a
LINK openssl openssl::crypto openssl::ssl
```

## Current scope

Implemented today:

- Linux-only runtime behavior
- CMake, Meson, Autoconf, Cargo, and Manual recipe families
- `BUILD_ROOT SYSTEM`
- `BUILD_ROOT SCRATCH`
- `BUILD_ROOT OCI <ref>`
- foreign-architecture OCI execution via staged `qemu-*-static`

`DepoFile`s are trusted inputs. `depos` is not a hostile-code sandbox.

## License

Apache-2.0

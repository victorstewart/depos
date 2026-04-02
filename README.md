# depos

`depos` is an external dependency builder and repository manager for C and C++ projects.

You describe a dependency once in a small line-oriented `DepoFile`. `depos` fetches the source, builds it in an isolated Linux runtime, publishes the result into a versioned repository, and generates a CMake registry that consumer projects can import from a manifest.

## Distribution contract

`depos` is meant to be used as a tool binary.

Preferred resolution order for consumer projects:

1. default project-local `cargo install depos --version 0.3.0`
2. explicit override such as `DEPOS_EXECUTABLE`
3. optional system `depos` on `PATH` if you opt into it

Primary distribution is GitHub release binaries. `cargo install depos --version 0.3.0` is the convenience path and the same version `.depos.cmake` bootstraps locally by default.

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
include("/path/to/.depos.cmake")
depos_depend_all()
depos_link_all(app)
```

```cmake
include("/path/to/.depos.cmake")
depos_depend(bitsery)
depos_depend(itoa)
depos_depend(zlib VERSION 1.3.2)
depos_link(app bitsery itoa zlib)
```

`depos_depend_all()` scans the public top-level `depofiles/` directory next to `.depos.cmake` by default. `depos_link_all(<target>)` links every known primary target from those `DepoFile`s. `depos_link(<target> ...)` links specific package names or imported target names. Both link helpers default to `PUBLIC`; pass `PRIVATE` immediately after the target name if you want to stop propagation.

`depos_depend(...)` can take a single `DepoFile` path, and `depos_depend_all(...)` can take a depofiles directory path only:

```cmake
depos_depend("${CMAKE_CURRENT_SOURCE_DIR}/depofiles/zlib.DepoFile")
depos_depend_all("${CMAKE_CURRENT_SOURCE_DIR}/third_party/depofiles")
```

If you want non-propagating linkage on one target, make it explicit:

```cmake
depos_link(app PRIVATE zlib)
```

Libraries that ship with `depos` support two clean consumer modes:

- source-tree consumption: ship top-level `.depos.cmake` plus public top-level `depofiles/`, and anyone building the library's own CMake can let that helper bootstrap and resolve everything without touching `depos` directly
- published-depofile consumption: a downstream project can depend on the library package by name or point at the library's published `DepoFile`, and the library's `DEPENDS` graph cascades through to the final binary

```cmake
depos_depend(cascade_lib VERSION 1.0.0)
depos_link(app cascade_lib)
```

```cmake
depos_depend("${CMAKE_CURRENT_SOURCE_DIR}/third_party/cascade_lib.DepoFile")
depos_link(app cascade_lib)
```

During configure, `.depos.cmake` emits `depos:` status lines while it bootstraps the tool, registers local `DepoFile`s, and syncs the registry so dependency work does not look stalled.

By default `.depos.cmake` bootstraps `depos 0.3.0` into a hidden top-level `.depos/` directory next to the helper, keeps the local registry under that same hidden root, and records the selected mode in `.depos/.state.cmake`. Library maintainers should put `.depos.cmake` at the top of the repo before publishing the library and keep dependency `DepoFile`s in the public top-level `depofiles/` directory beside it so consumer builds can self-bootstrap from that location and just work.

If you want a shared system install instead, install the tool yourself and point CMake at it explicitly:

```bash
cargo install depos --version 0.3.0
```

Then set `DEPOS_EXECUTABLE` and, if you want a shared registry/root, `DEPOS_ROOT`.

Generated manifest example:

```cmake
depos_require(zlib VERSION 1.3.2)
depos_require(openssl VERSION 3.4.1)
```

## Why Isolated Build Roots Matter

The strongest utility `depos` provides is not just downloading dependencies. It is the ability to build packages in an isolated root so the build cannot silently reach into the host machine's `/usr` and `/lib` trees unless you deliberately expose them.

The clean mental model is:

- `BUILD_ROOT SYSTEM` is the convenience mode. The host filesystem is still available to the build.
- `BUILD_ROOT SCRATCH` is the minimal hermetic mode. You must declare the toolchain inputs you want mounted into the build root.
- `BUILD_ROOT OCI ...` gives you a curated hermetic root filesystem for the same reason.

In practice, this is how you prevent "it built on my machine because it found some random system OpenSSL/zlib/libcurl" from becoming part of the build story.

## Example: Ambient OpenSSL Leakage

Suppose an upstream dependency contains this in its own build scripts:

```cmake
find_package(OpenSSL REQUIRED)
target_link_libraries(upstream PRIVATE OpenSSL::SSL OpenSSL::Crypto)
```

And suppose your Depo graph already declares a specific OpenSSL package version that the rest of the program is supposed to use.

With `BUILD_ROOT SYSTEM`, that upstream package can still accidentally resolve the host copy from `/usr/lib` or `/usr/local`, because those locations exist in the build environment. Even if that was not your intent, the build may appear to work.

With `BUILD_ROOT SCRATCH`, the same package only sees:

- the fetched source tree
- the Depo dependency roots
- the explicit `TOOLCHAIN_INPUT` mounts you declared

If OpenSSL is not provided through Depo or an explicit toolchain/sysroot input, the build fails instead of silently drifting onto the host system copy. That failure is useful. It turns an implicit host dependency into an explicit packaging problem you can fix.

A minimal scratch-oriented package shape looks like this:

```text
NAME example_tls_dep
VERSION 1.0.0
BUILD_ROOT SCRATCH
TOOLCHAIN_INPUT /bin/sh
TOOLCHAIN_INPUT /usr/bin/install
TOOLCHAIN_INPUT /usr/lib
TOOLCHAIN_INPUT /lib
TOOLCHAIN_INPUT /lib64
DEPENDS openssl VERSION 3.4.1
SOURCE GIT https://example.com/example_tls_dep.git 0123456789abcdef0123456789abcdef01234567
BUILD_SYSTEM MANUAL
MANUAL_INSTALL_SH <<'EOF'
install -D "${DEPO_SOURCE_DIR}/include/example_tls_dep.h" \
  "${DEPO_PREFIX}/include/example_tls_dep/example_tls_dep.h"
EOF
TARGET example_tls_dep::example_tls_dep INTERFACE include
```

The important property is not the exact package above. The important property is that every host path visible inside the build is deliberate.

Using isolated build roots gives you three practical wins:

- reproducibility: a package cannot succeed just because your workstation happens to have extra libraries installed
- conflict detection: missing or conflicting subdependencies fail early instead of being "fixed" by ambient host libraries
- packaging discipline: if a dependency needs a library, the right answer is to declare it in a DepoFile and propagate it through exported targets

## Example `DepoFile`s

Simple package:

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
AUTOCONF_CONFIGURE env CC=aarch64-linux-gnu-gcc AR=aarch64-linux-gnu-ar RANLIB=aarch64-linux-gnu-ranlib STRIP=aarch64-linux-gnu-strip ./Configure linux-aarch64 --prefix="${DEPO_PREFIX}" --libdir=lib --openssldir="${DEPO_PREFIX}/ssl" no-quic no-tests no-docs no-shared
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

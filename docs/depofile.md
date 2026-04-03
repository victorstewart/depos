# DepoFile Reference

`DepoFile`s are line-oriented dependency recipes. They describe identity, source, build behavior, exported targets, and optional isolation requirements.

## Smallest Useful Shape

```text
NAME itoa
VERSION main
SOURCE GIT https://github.com/jeaiii/itoa.git main
TARGET itoa::itoa INTERFACE include
```

That is enough for a header-only dependency: name, version, source, and exported include path.

## Core Fields

- `NAME <name>`: package name
- `VERSION <version>`: package version label
- `SOURCE GIT <url> <ref>`: Git source with branch, tag, or commit reference
- `SOURCE URL <url>`: archive source
- `SOURCE_SUBDIR <path>`: build from a subdirectory inside the fetched source tree
- `DEPENDS <name> VERSION <version>`: consume another package

## Build Systems

`depos` supports these recipe families today:

- `BUILD_SYSTEM CMAKE`
- `BUILD_SYSTEM MESON`
- `BUILD_SYSTEM AUTOCONF`
- `BUILD_SYSTEM CARGO`
- `BUILD_SYSTEM MANUAL`

Each family has matching directives for configure, build, and install behavior. A few common examples:

- `CMAKE_DEFINE BUILD_SHARED_LIBS=OFF`
- `AUTOCONF_CONFIGURE ...`
- `CARGO_BUILD cargo build --release ...`
- `MANUAL_INSTALL ...`

## Exported Targets

`TARGET` lines define what downstream builds can import.

Header-only export:

```text
TARGET itoa::itoa INTERFACE include
```

Static library export:

```text
TARGET zlib::zlib STATIC lib/libz.a INTERFACE include
```

Multi-target export:

```text
TARGET openssl INTERFACE include
TARGET openssl::crypto STATIC lib/libcrypto.a
TARGET openssl::ssl STATIC lib/libssl.a
LINK openssl openssl::crypto openssl::ssl
```

## Isolation And Platform Controls

- `BUILD_ROOT SYSTEM`: build against the host environment
- `BUILD_ROOT SCRATCH`: minimal hermetic Linux root
- `BUILD_ROOT OCI <image>`: pinned Linux rootfs
- `TOOLCHAIN ROOTFS`: Linux-only rootfs toolchain mode
- `BUILD_ARCH` and `TARGET_ARCH`: build/target split for advanced Linux flows

On macOS and Windows, `depos` keeps native `BUILD_ROOT SYSTEM` on the portable host backend. [EXPERIMENTAL] `BUILD_ROOT OCI <image>` now routes through a local Linux provider instead of being rejected outright. `depos` still explicitly rejects:

- `BUILD_ROOT SCRATCH`
- `TOOLCHAIN ROOTFS` without `BUILD_ROOT OCI`
- `BUILD_ARCH != TARGET_ARCH` without `BUILD_ROOT OCI`

Provider selection stays in runtime configuration rather than the recipe:

- `DEPOS_LINUX_PROVIDER=auto`
- `DEPOS_LINUX_PROVIDER=wsl2`
- `DEPOS_LINUX_PROVIDER=mac-local`
- `DEPOS_LINUX_PROVIDER_ROOT=/absolute/linux/path`

`DepoFile`s are trusted inputs. `depos` is not a hostile-code sandbox.

## Real Examples In This Repo

- [itoa](../depofiles/local/itoa/nametag/main/main.DepoFile)
- [bitsery](../depofiles/local/bitsery/nametag/5.2.3/main.DepoFile)
- [zlib](../depofiles/local/zlib/nametag/1.3.2/main.DepoFile)
- [openssl](../depofiles/local/openssl/nametag/3.4.1/main.DepoFile)

For build-root behavior and the ambient-host-leakage motivation, see [build-roots.md](build-roots.md).

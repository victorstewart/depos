# Build Roots And Platform Contract

The sharpest thing `depos` provides is not just dependency download. It is control over what a dependency build can see.

Dependencies should not silently succeed just because the host machine happened to have a library lying around in `/usr`, `/lib`, `/usr/local`, or some preconfigured SDK path.

## Build Root Modes

- `BUILD_ROOT SYSTEM`: convenience mode; the host environment is visible
- `BUILD_ROOT SCRATCH`: minimal hermetic Linux mode; only declared inputs are visible
- `BUILD_ROOT OCI <image>`: pinned Linux rootfs for distro-specific or cross-target builds

The goal is simple: stop ambient machine state from choosing your dependency graph.

## Why This Matters

Suppose an upstream package does this:

```cmake
find_package(OpenSSL REQUIRED)
target_link_libraries(upstream PRIVATE OpenSSL::SSL OpenSSL::Crypto)
```

With `BUILD_ROOT SYSTEM`, that package can accidentally find a host OpenSSL from `/usr` or `/usr/local` and appear to work.

With `BUILD_ROOT SCRATCH`, it only sees:

- the fetched source tree
- declared Depo dependency roots
- explicit toolchain inputs you mounted into the build root

If OpenSSL was not declared, the build fails instead of silently drifting onto the host copy. That failure is useful.

## Platform Contract

Implemented today:

- Linux native execution with `BUILD_ROOT SYSTEM`, `BUILD_ROOT SCRATCH`, and `BUILD_ROOT OCI <ref>`
- Linux `TOOLCHAIN ROOTFS`
- Linux foreign-architecture OCI execution via staged `qemu-*-static`
- macOS native execution with `BUILD_ROOT SYSTEM`
- Windows native execution with `BUILD_ROOT SYSTEM`

[EXPERIMENTAL] On macOS and Windows, selecting `BUILD_ROOT OCI <ref>` now routes the package through a local Linux provider instead of trying to emulate Linux isolation in the host-native backend:

- Windows: WSL2
- macOS: a direct Apple Virtualization helper and Linux guest

Runtime selection happens outside the `DepoFile`:

- `DEPOS_LINUX_PROVIDER=auto` is the default
- `DEPOS_LINUX_PROVIDER=wsl2` is the explicit Windows provider
- `DEPOS_LINUX_PROVIDER=mac-local` is the explicit macOS provider
- `DEPOS_LINUX_PROVIDER_ROOT=/absolute/linux/path` overrides the provider-side runtime root
- `DEPOS_WSL_DISTRO=<name>` picks the WSL distro for Windows
- `DEPOS_APPLE_VIRTUALIZATION_HELPER=/absolute/path/to/helper` points macOS at the direct helper
- `DEPOS_APPLE_VIRTUALIZATION_VM=<name>` overrides the default macOS VM name

On macOS and Windows, `depos` still explicitly rejects:

- `BUILD_ROOT SCRATCH`
- `TOOLCHAIN ROOTFS` without `BUILD_ROOT OCI <ref>`
- `BUILD_ARCH != TARGET_ARCH` without `BUILD_ROOT OCI <ref>`

That restriction should stay visible in the docs and READMEs so people do not assume Linux runtime semantics exist everywhere or that the local Linux-provider path is already shipping-grade.

## Choosing A Mode

- choose `BUILD_ROOT SYSTEM` when convenience matters more than strict isolation
- choose `BUILD_ROOT SCRATCH` on Linux when you want minimal, deliberate visibility into host state
- choose `BUILD_ROOT OCI <image>` on Linux when you need a pinned distro rootfs or cross-target packaging contract

For recipe syntax, see [depofile.md](depofile.md). For working examples, see [examples.md](examples.md).

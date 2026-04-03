# depos CLI

`depos` is the CLI behind `.depos.cmake`.

Install it directly with:

```bash
cargo install depos --version 0.4.0
```

Use the CLI when you want to register recipes, sync manifests, or inspect repository state outside of the CMake helper flow.

## Core Commands

```bash
depos register --file /path/to/main.DepoFile --namespace release
depos sync --manifest /path/to/deps.cmake
depos status
```

Use a project-local root if you do not want the default shared root:

```bash
depos register --depos-root "$PWD/.deps/depos" --file /path/to/main.DepoFile --namespace release
depos sync --depos-root "$PWD/.deps/depos" --manifest /path/to/deps.cmake
```

## Current Scope

- Linux: `BUILD_ROOT SYSTEM`, `BUILD_ROOT SCRATCH`, `BUILD_ROOT OCI <ref>`, `TOOLCHAIN ROOTFS`, and foreign-architecture OCI execution
- macOS: native `BUILD_ROOT SYSTEM` plus [EXPERIMENTAL] `BUILD_ROOT OCI <ref>` through a local Linux provider
- Windows: native `BUILD_ROOT SYSTEM` plus [EXPERIMENTAL] `BUILD_ROOT OCI <ref>` through a local Linux provider

On macOS and Windows, `depos` still explicitly rejects:

- `BUILD_ROOT SCRATCH`
- `TOOLCHAIN ROOTFS` without `BUILD_ROOT OCI <ref>`
- `BUILD_ARCH != TARGET_ARCH` without `BUILD_ROOT OCI <ref>`

[EXPERIMENTAL] On macOS and Windows, selecting `BUILD_ROOT OCI <ref>` routes the package build through a local Linux provider instead of the host-native portable backend:

- Windows: WSL2
- macOS: a direct Apple Virtualization helper and Linux guest

Runtime selection is explicit at the bootstrap layer, not in `DepoFile` syntax:

- `DEPOS_LINUX_PROVIDER=auto` selects the host-appropriate provider
- `DEPOS_LINUX_PROVIDER=wsl2` forces the Windows WSL2 path
- `DEPOS_LINUX_PROVIDER=mac-local` forces the macOS Apple-Virtualization-backed path
- `DEPOS_LINUX_PROVIDER_ROOT=/absolute/linux/path` overrides the provider-side runtime root
- `DEPOS_WSL_DISTRO=<name>` selects the WSL distro when using `wsl2`; without it, Windows auto mode prefers `Ubuntu-24.04` and installs it lazily if no provider distro exists
- `DEPOS_APPLE_VIRTUALIZATION_HELPER=/absolute/path/to/helper` points `depos` at the direct macOS helper
- `DEPOS_APPLE_VIRTUALIZATION_VM=<name>` overrides the default macOS VM name

The provider runtime root now keeps `provider-metadata.env` plus versioned bootstrap state and caches so you can inspect the Linux-side runtime directly.

## Docs

- [Landing page](../README.md)
- [Getting started](../docs/getting-started.md)
- [DepoFile reference](../docs/depofile.md)
- [CMake integration](../docs/cmake.md)
- [Build roots and platform contract](../docs/build-roots.md)
- [CLI reference](../docs/cli.md)
- [Examples](../docs/examples.md)

## License

Apache-2.0

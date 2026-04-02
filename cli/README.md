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
- macOS: native `BUILD_ROOT SYSTEM` only in this pass
- Windows: native `BUILD_ROOT SYSTEM` only in this pass

On macOS and Windows, `depos` explicitly rejects:

- `BUILD_ROOT SCRATCH`
- `BUILD_ROOT OCI <ref>`
- `TOOLCHAIN ROOTFS`
- `BUILD_ARCH != TARGET_ARCH`

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

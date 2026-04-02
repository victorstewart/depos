# depos

**Dockerfiles for build dependencies.**

Build dependencies from declared inputs, not ambient machine state.

Write a tiny `DepoFile`. Build dependencies in isolated roots. Reuse versioned outputs from CMake.

```text
NAME itoa
VERSION main
SOURCE GIT https://github.com/jeaiii/itoa.git main
TARGET itoa::itoa INTERFACE include
```

A `DepoFile` is the dependency equivalent of a Dockerfile:
where source comes from, how it builds, and what it exports.

`depos` fetches the source, builds it in the right root, stores the result as a versioned package, and exposes imported CMake targets.

Copy [`.depos.cmake`](.depos.cmake) into your repo root, add a `DepoFile`, then:

```cmake
include(".depos.cmake")
depos_depend(itoa)
depos_link(app itoa)
```

`.depos.cmake` is the low-friction path. By default it bootstraps `depos 0.4.0` locally on first use.

Why this exists: stop letting `/usr`, `/lib`, `/usr/local`, or some random SDK decide whether your dependency graph works.

## Why People Use It

- Tiny dependency recipes
- Build dependencies from declared inputs instead of ambient machine state
- Versioned, cached package outputs
- CMake-friendly imported targets
- Linux isolated build roots when host leakage matters
- Native macOS and Windows `BUILD_ROOT SYSTEM` support

## Choose A Root

- `BUILD_ROOT SYSTEM` for convenience
- `BUILD_ROOT SCRATCH` for minimal hermetic Linux builds
- `BUILD_ROOT OCI <image>` for pinned Linux distro roots and cross-target packaging
- On macOS and Windows, `depos` supports native `BUILD_ROOT SYSTEM` only in this pass
- On macOS and Windows, `depos` explicitly rejects `BUILD_ROOT SCRATCH`, `BUILD_ROOT OCI`, `TOOLCHAIN ROOTFS`, and `BUILD_ARCH != TARGET_ARCH`

## Learn More

- [Getting started](docs/getting-started.md)
- [DepoFile reference](docs/depofile.md)
- [CMake integration](docs/cmake.md)
- [Build roots and platform contract](docs/build-roots.md)
- [CLI reference](docs/cli.md)
- [Examples](docs/examples.md)

## License

Apache-2.0

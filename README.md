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

`.depos.cmake` is the low-friction path. By default it bootstraps `depos 0.5.0` locally on first use.

If you publish a library for others to consume through `depos`, ship one detached top-level
published `DepoFile` outside your source archive. That published `DepoFile` should point at
the release source tarball, list transparent `DEPENDS`, and let `depos` automatically cascade
the dependency `depofiles/` tree from inside the fetched source during the same resolution flow.
That detached published `DepoFile` is the correct library export surface for `depos`
consumption.

Why this exists: stop letting `/usr`, `/lib`, `/usr/local`, or some random SDK decide whether your dependency graph works.

## Why People Use It

- Tiny dependency recipes
- Build dependencies from declared inputs instead of ambient machine state
- Versioned, cached package outputs
- CMake-friendly imported targets
- Linux isolated build roots when host leakage matters
- Native macOS and Windows `BUILD_ROOT SYSTEM` support
- [EXPERIMENTAL] macOS and Windows `BUILD_ROOT OCI <image>` support through a local Linux provider

## Choose A Root

- `BUILD_ROOT SYSTEM` for convenience
- `BUILD_ROOT SCRATCH` for minimal hermetic Linux builds
- `BUILD_ROOT OCI <image>` for pinned Linux distro roots and cross-target packaging
- On macOS and Windows, native `BUILD_ROOT SYSTEM` stays on the portable host backend
- [EXPERIMENTAL] On macOS and Windows, selecting `BUILD_ROOT OCI <image>` routes through a local Linux provider instead of the host-native portable backend
- [EXPERIMENTAL] On Windows, auto provider mode prefers a local `Ubuntu-24.04` WSL distro and installs it lazily if needed
- On macOS and Windows, `depos` still rejects `BUILD_ROOT SCRATCH`
- On macOS and Windows, `TOOLCHAIN ROOTFS` and `BUILD_ARCH != TARGET_ARCH` still require `BUILD_ROOT OCI <image>`

## Learn More

- [Getting started](docs/getting-started.md)
- [DepoFile reference](docs/depofile.md)
- [CMake integration](docs/cmake.md)
- [Build roots and platform contract](docs/build-roots.md)
- [CLI reference](docs/cli.md)
- [Examples](docs/examples.md)

## License

Apache-2.0

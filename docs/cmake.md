# CMake Integration

The normal CMake flow is:

```cmake
include(".depos.cmake")
depos_depend(itoa)
depos_link(app itoa)
```

`.depos.cmake` is designed so consumers do not have to lead with manual `depos register` / `depos sync` calls.

## Requesting Dependencies

Queue one package by name:

```cmake
depos_depend(itoa)
depos_depend(zlib VERSION 1.3.2)
```

Queue one explicit `DepoFile` path:

```cmake
depos_depend("${CMAKE_CURRENT_SOURCE_DIR}/third_party/depofiles/zlib.DepoFile")
```

Batch multiple explicit `DepoFile` paths:

```cmake
depos_depend(
  FILES
  "${CMAKE_CURRENT_SOURCE_DIR}/third_party/depofiles/zlib.DepoFile"
  "${CMAKE_CURRENT_SOURCE_DIR}/third_party/depofiles/openssl.DepoFile"
)
```

Queue every `DepoFile` under a depofiles directory:

```cmake
depos_depend_all()
depos_depend_all("${CMAKE_CURRENT_SOURCE_DIR}/third_party/depofiles")
```

By default `depos_depend_all()` scans the public top-level `depofiles/` directory next to `.depos.cmake`.

## Linking Targets

Link specific packages or imported targets:

```cmake
depos_link(app itoa zlib)
```

Link every known primary target from queued recipes:

```cmake
depos_link_all(app)
```

`depos_link(...)` and `depos_link_all(...)` default to `PUBLIC`. Use `PRIVATE` immediately after the target name if you want to stop propagation:

```cmake
depos_link(app PRIVATE zlib)
```

## Lazy Sync Behavior

- `depos_depend(...)` and `depos_depend_all(...)` queue requests during configure
- `.depos.cmake` syncs them once, lazily, on the first `depos_link(...)`, `depos_link_all(...)`, or `depos_use(...)` that needs the registry
- imported targets from queued requests are not guaranteed to exist before that first sync point

## Consumer Modes

Source-tree consumption:

- ship top-level `.depos.cmake`
- ship public top-level `depofiles/`
- let consumer CMake bootstrap and resolve the graph directly from source

Published-depofile consumption:

```cmake
depos_depend(cascade_lib VERSION 1.0.0)
depos_link(app cascade_lib)
```

or:

```cmake
depos_depend("${CMAKE_CURRENT_SOURCE_DIR}/third_party/cascade_lib.DepoFile")
depos_link(app cascade_lib)
```

The intended published shape is one detached top-level `cascade_lib.DepoFile` outside the
source archive. That published `DepoFile` points at the release tarball and lists transparent
`DEPENDS`. The fetched source archive can then carry `depofiles/` for the library's own
dependencies, which `depos` now discovers and cascades during the same resolution flow.

## Project Defaults

If you want repo-local defaults without hardcoding them in `CMakeLists.txt`, add `depos.project.cmake` next to `.depos.cmake`:

```cmake
set(DEPOS_BOOTSTRAP_VERSION "0.5.0" CACHE STRING "Pinned depos version used by this project" FORCE)
```

Useful knobs:

- `DEPOS_EXECUTABLE`
- `DEPOS_ROOT`
- `DEPOS_BOOTSTRAP_VERSION`
- `DEPOS_BOOTSTRAP_DIR`
- `DEPOS_ALLOW_SYSTEM_EXECUTABLE`

For install/bootstrap details, see [getting-started.md](getting-started.md). For runtime semantics, see [build-roots.md](build-roots.md).

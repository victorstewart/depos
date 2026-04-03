# Getting Started

`depos` has two normal entry points:

- let `.depos.cmake` bootstrap it locally on first use
- install the CLI yourself and point CMake at it explicitly

The shortest path is the first one, and it is the path the landing page assumes.

## Fastest Path

Copy [`.depos.cmake`](../.depos.cmake) into the top of your repo, add a `DepoFile`, and consume it from CMake.

```text
your-project/
  .depos.cmake
  depofiles/itoa/main.DepoFile
  CMakeLists.txt
```

`depos_depend_all()` scans `depofiles/` recursively, so your own repo does not need to copy this repo's internal example layout.

Example `DepoFile`:

```text
NAME itoa
VERSION main
SOURCE GIT https://github.com/jeaiii/itoa.git main
TARGET itoa::itoa INTERFACE include
```

Example `CMakeLists.txt`:

```cmake
include(".depos.cmake")
add_executable(app main.cpp)
depos_depend(itoa)
depos_link(app itoa)
```

If you are shipping a source tree with multiple public recipes, the all-in form is:

```cmake
include(".depos.cmake")
depos_depend_all()
depos_link_all(app)
```

## Publishing Your Own Library

The recommended export pattern is:

- publish one detached top-level `your_lib.DepoFile`
- keep that published `DepoFile` outside `depofiles/` and outside the source archive
- have that published `DepoFile` point at the release source archive with `SOURCE URL ...`
- keep the source archive's `depofiles/` tree for the library's dependency recipes only

That lets consumers download only the published `DepoFile`. `depos` fetches the source
archive, discovers embedded dependency `DepoFile`s from the fetched source tree, and resolves
the full graph in one flow.

## Bootstrap Behavior

By default `.depos.cmake` bootstraps `depos 0.4.0` into a hidden top-level `.depos/` directory next to the helper. It keeps the local registry and bootstrap state there too.

If you do not want local bootstrap, install `depos` yourself:

```bash
cargo install depos --version 0.4.0
```

Then point CMake at it with `DEPOS_EXECUTABLE`. If you want a shared root instead of the project-local `.depos/`, set `DEPOS_ROOT` too.

## What Happens On First Use

- `depos_depend(...)` queues requests during configure
- `.depos.cmake` syncs once, lazily, on the first `depos_link(...)`, `depos_link_all(...)`, or `depos_use(...)` that needs the registry
- unchanged sources and package outputs are reused instead of rebuilt

## Next Docs

- [DepoFile reference](depofile.md)
- [CMake integration](cmake.md)
- [Build roots and platform contract](build-roots.md)
- [CLI reference](cli.md)
- [Examples](examples.md)

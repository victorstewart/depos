# CLI Reference

`depos` is a tool binary. Use it directly when you want to register recipes, sync manifests, or inspect state outside of the CMake helper flow.

## Install

```bash
cargo install depos --version 0.4.0
```

If you are using `.depos.cmake`, you can also let it bootstrap `depos 0.4.0` locally on first use instead of preinstalling it.

## Core Commands

Register a recipe:

```bash
depos register --file /path/to/main.DepoFile --namespace release
```

Sync a manifest:

```bash
depos sync --manifest /path/to/deps.cmake
```

Use a project-local root:

```bash
depos register --depos-root "$PWD/.deps/depos" --file /path/to/main.DepoFile --namespace release
depos sync --depos-root "$PWD/.deps/depos" --manifest /path/to/deps.cmake
```

Inspect state:

```bash
depos status
depos status --refresh
depos registry-dir --manifest /path/to/deps.cmake
```

## Root Layout

`depos` uses one root. It defaults to `~/.depos` on Unix and `%USERPROFILE%\\.depos` on Windows.

```text
depos_root/
  depofiles/local/<name>/<namespace>/<version>/main.DepoFile
  store/<variant>/<name>/<namespace>/<version>/...
  registry/<variant>/<manifest-profile>/...
```

That root holds:

- registered `DepoFile`s
- materialized package outputs
- generated CMake registries

## Resolution Order In CMake

When `.depos.cmake` resolves the tool, the order is:

1. project-local bootstrap into `.depos/`
2. explicit override such as `DEPOS_EXECUTABLE`
3. optional system `depos` on `PATH` if the project opts into it

For the landing-page flow, start with [README.md](../README.md). For CMake behavior, see [cmake.md](cmake.md).

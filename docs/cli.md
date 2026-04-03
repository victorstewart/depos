# CLI Reference

`depos` is a tool binary. Use it directly when you want to register recipes, sync manifests, or inspect state outside of the CMake helper flow.

## Install

```bash
cargo install depos --version 0.5.0
```

If you are using `.depos.cmake`, you can also let it bootstrap `depos 0.5.0` locally on first use instead of preinstalling it.

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
  depofiles/.embedded/<name>/<namespace>/<version>/main.DepoFile
  store/<variant>/<name>/<namespace>/<version>/...
  registry/<variant>/<manifest-profile>/...
```

That root holds:

- registered `DepoFile`s
- generated embedded dependency `DepoFile`s discovered from fetched source trees
- materialized package outputs
- generated CMake registries

## Linux Provider Runtime Knobs

On macOS and Windows, `BUILD_ROOT OCI <image>` enters the local Linux-provider path. Runtime selection stays outside the `DepoFile` surface:

- `DEPOS_LINUX_PROVIDER=auto` selects the host-appropriate provider
- `DEPOS_LINUX_PROVIDER=wsl2` forces the Windows WSL2 provider
- `DEPOS_LINUX_PROVIDER=mac-local` forces the macOS direct-helper path
- `DEPOS_LINUX_PROVIDER_ROOT=/absolute/linux/path` overrides the provider-side runtime root
- `DEPOS_WSL_DISTRO=<name>` selects the WSL distro when `wsl2` is active; otherwise Windows auto mode prefers `Ubuntu-24.04` and installs it lazily if needed
- `DEPOS_APPLE_VIRTUALIZATION_HELPER=/absolute/path/to/helper` points macOS at the direct helper executable
- `DEPOS_APPLE_VIRTUALIZATION_VM=<name>` overrides the default macOS VM name

The provider runtime root keeps `provider-metadata.env` plus versioned bootstrap state and caches so the Linux-side runtime can be inspected directly.

On macOS and Windows, `depos` still rejects Linux-only advanced requests without `BUILD_ROOT OCI <image>`.

## Resolution Order In CMake

When `.depos.cmake` resolves the tool, the order is:

1. project-local bootstrap into `.depos/`
2. explicit override such as `DEPOS_EXECUTABLE`
3. optional system `depos` on `PATH` if the project opts into it

For the landing-page flow, start with [README.md](../README.md). For CMake behavior, see [cmake.md](cmake.md).

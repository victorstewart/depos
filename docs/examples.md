# Examples

This repo already contains real `DepoFile`s under `depofiles/local/`. Start with the smallest ones first.

## Small And Readable

- [itoa](../depofiles/local/itoa/nametag/main/main.DepoFile): tiny header-only example
- [bitsery](../depofiles/local/bitsery/nametag/5.2.3/main.DepoFile): another compact C++ library recipe

## Common Native Libraries

- [zlib](../depofiles/local/zlib/nametag/1.3.2/main.DepoFile)
- [openssl](../depofiles/local/openssl/nametag/3.4.1/main.DepoFile)
- [zstd](../depofiles/local/zstd/nametag/1.5.7/main.DepoFile)
- [libcurl](../depofiles/local/libcurl/nametag/8.8.0/main.DepoFile)

## Larger Examples

- [flatbuffers](../depofiles/local/flatbuffers/nametag/22.9.29/main.DepoFile)
- [simdjson](../depofiles/local/simdjson/nametag/4.2.4/main.DepoFile)
- [libevent](../depofiles/local/libevent/nametag/2.1.12/main.DepoFile)

## Notes

- `nametag` and `parsecheck` are namespaces, not different syntax
- some examples are intentionally Linux-oriented because they demonstrate Linux-only isolation modes
- on macOS and Windows, stick to native `BUILD_ROOT SYSTEM` recipes in this pass

For the platform contract, see [build-roots.md](build-roots.md).

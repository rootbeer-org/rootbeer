#!/bin/sh
# Runs `rb drv` in the pinned Linux builder image, to exercise linux-v1 from a
# Mac without CI. rb is compiled in a rust container. The store lives in a
# volume per architecture.
#
#   scripts/linux.sh build zstd
#   RB_PLATFORM=linux/amd64 scripts/linux.sh build zlib
#   RB_CATALOG=../pdr scripts/linux.sh show zlib
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
catalog=$(cd "${RB_CATALOG:-$root/../pdr}" && pwd)
platform=${RB_PLATFORM:-linux/arm64}
arch=${platform#linux/}

docker run --rm --platform "$platform" \
    --volume "$root:/src:ro" \
    --volume "rb-cargo-$arch:/usr/local/cargo/registry" \
    --volume "rb-target-$arch:/target" \
    --env CARGO_TARGET_DIR=/target \
    rust:1-bookworm \
    cargo build --quiet --locked --manifest-path /src/Cargo.toml --package rootbeer-cli

# bubblewrap needs to create namespaces and mount its own /proc. The image
# can't contain its own hash, so the sandbox learns it from RB_HOST_SYSTEM.
builder="$catalog/.github/builder"
image=$(docker build --quiet --platform "$platform" "$builder")
hash=$(shasum -a 256 < "$builder/Dockerfile")
exec docker run --rm --init --platform "$platform" \
    --env "RB_HOST_SYSTEM=builder-sha256:${hash%% *}" \
    --security-opt seccomp=unconfined \
    --security-opt apparmor=unconfined \
    --security-opt systempaths=unconfined \
    --volume "rb-store-$arch:/opt/rb" \
    --volume "rb-target-$arch:/target:ro" \
    --volume "$catalog:/catalog:ro" \
    --workdir /catalog \
    "$image" /target/debug/rb drv "$@"

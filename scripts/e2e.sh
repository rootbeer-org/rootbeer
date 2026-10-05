#!/bin/sh
# End to end test of the CI flow in the Linux builder. It builds a package,
# exports its closure, publishes it to a staging namespace on a local registry,
# promotes it to a store namespace, then installs it by key into one empty store
# and imports the export into another. Both must match the build exactly.
#
#   scripts/e2e.sh [package]
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
package=${1:-zstd}
linux="$root/scripts/linux.sh"
url=http://localhost:5050
# Fresh namespaces, so a rerun publishes and promotes instead of skipping.
run=rootbeer-e2e/$(date +%s)-$$
staging=$run/staging
store=$run/store

# Port 5000 belongs to AirPlay Receiver on macOS.
if ! docker container inspect rb-registry > /dev/null 2>&1; then
    docker run --detach --rm --name rb-registry --publish 5050:5000 registry:2 > /dev/null
fi

artifacts=$(mktemp -d)
installed="rb-e2e-installed-$$"
imported="rb-e2e-imported-$$"
trap 'rm -rf "$artifacts"; docker volume rm "$installed" "$imported" > /dev/null 2>&1 || true' EXIT

# Not piped straight into tail, which would hide a failed build.
built=$("$linux" build "$package")
entry=$(basename "$(printf '%s\n' "$built" | tail -n 1)")
key=${entry%%-*}
name=${package%@*}

export RB_ARTIFACTS="$artifacts"
"$linux" export "$package" /artifacts
"$linux" publish /artifacts --registry "$url" --namespace "$staging" --allow-http

# Promotion only talks to the registry, so it runs on this machine.
cargo run --quiet --manifest-path "$root/Cargo.toml" --package rootbeer-cli -- \
    drv promote "$name" "$key" --registry "$url" --from "$staging" --to "$store" --allow-http \
    --unattested

# Everything is in store now, so publishing again must push nothing.
again=$("$linux" publish /artifacts --registry "$url" --namespace "$staging-again" \
    --skip "$store" --allow-http 2>&1)
if printf '%s\n' "$again" | grep -v '^skipped '; then
    echo "publishing with --skip $store still pushed" >&2
    exit 1
fi

RB_STORE_VOLUME=$installed "$linux" install "$name" "$key" \
    --registry "$url" --namespace "$store" --allow-http
RB_STORE_VOLUME=$imported "$linux" import /artifacts

# Every tree, as tar with times and owners normalized, must be identical.
# Each tar must succeed, so two missing trees can't compare equal.
platform=${RB_PLATFORM:-linux/arm64}
docker run --rm --platform "$platform" \
    --volume "rb-store-${platform#linux/}:/built:ro" \
    --volume "$installed:/installed:ro" \
    --volume "$imported:/imported:ro" \
    ubuntu:24.04 sh -ec "
        for store in built installed imported; do
            tar -C /\$store/store/$entry --sort=name --mtime=@0 --owner=0 --group=0 \
                --numeric-owner -cf /tmp/\$store.tar .
        done
        cmp /tmp/built.tar /tmp/installed.tar
        cmp /tmp/built.tar /tmp/imported.tar
    "

echo "$entry went through staging, store, and an export, and matches the build"

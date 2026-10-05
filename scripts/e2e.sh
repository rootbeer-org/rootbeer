#!/bin/sh
# End to end test of the store and cache in the Linux builder. It builds a
# package, pushes its closure to a local registry, installs it by key into an
# empty store, and compares the installed tree with the built one.
#
#   scripts/e2e.sh [package]
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
package=${1:-zstd}
linux="$root/scripts/linux.sh"
registry="--registry http://localhost:5050 --namespace rootbeer-e2e/store --allow-http"

# Port 5000 belongs to AirPlay Receiver on macOS.
if ! docker container inspect rb-registry > /dev/null 2>&1; then
    docker run --detach --rm --name rb-registry --publish 5050:5000 registry:2 > /dev/null
fi

# Not piped straight into tail, which would hide a failed build.
built=$("$linux" build "$package")
entry=$(basename "$(printf '%s\n' "$built" | tail -n 1)")
key=${entry%%-*}
name=${package%@*}

# shellcheck disable=SC2086
"$linux" push "$package" $registry

fresh="rb-e2e-$$"
trap 'docker volume rm "$fresh" > /dev/null 2>&1 || true' EXIT
# shellcheck disable=SC2086
RB_STORE_VOLUME=$fresh "$linux" install "$name" "$key" $registry

# Both trees, as tar with times and owners normalized, must be identical.
# Each tar must succeed, so two missing trees can't compare equal.
platform=${RB_PLATFORM:-linux/arm64}
docker run --rm --platform "$platform" \
    --volume "rb-store-${platform#linux/}:/built:ro" \
    --volume "$fresh:/installed:ro" \
    ubuntu:24.04 sh -ec "
        for store in built installed; do
            tar -C /\$store/store/$entry --sort=name --mtime=@0 --owner=0 --group=0 \
                --numeric-owner -cf /tmp/\$store.tar .
        done
        cmp /tmp/built.tar /tmp/installed.tar
    "

echo "installed $entry by key and it matches the build"

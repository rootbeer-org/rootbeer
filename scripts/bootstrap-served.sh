#!/usr/bin/env bash
# Copies the bootstrap releases rbpkg.com serves now, each document with its
# archive, into <out>. A platform with no document yet is skipped, and any
# other failure stops, so a deploy never drops a release by accident.
#
#   scripts/bootstrap-served.sh <out>
set -euo pipefail

out=$1
base=https://rbpkg.com/bootstrap/v1
mkdir -p "$out/archives"

# Succeeds on 200 and fails on 404. Anything else exits.
get() {
    local status
    status=$(curl -sSL --retry 3 -o "$2" -w '%{http_code}' "$1")
    case $status in
        200) return 0 ;;
        404) rm -f "$2"; return 1 ;;
        *) echo "$1 returned $status" >&2; exit 1 ;;
    esac
}

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

for platform in x86_64-linux aarch64-linux aarch64-macos; do
    if ! get "$base/$platform.json" "$work/$platform.json"; then
        continue
    fi

    url=$(jq -r '.payload | @base64d | fromjson | .url' "$work/$platform.json")
    if [[ $url != "$base/archives/"* ]]; then
        echo "$platform.json names $url, outside $base/archives" >&2
        exit 1
    fi

    if ! get "$url" "$out/archives/${url##*/}"; then
        echo "$platform.json names $url, which is gone" >&2
        exit 1
    fi

    mv "$work/$platform.json" "$out/$platform.json"
done

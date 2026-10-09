#!/usr/bin/env bash
# Signs a v1 bootstrap release for one platform. It copies the archive to
# <out>/archives/<sha256>.tar.gz and writes <out>/<platform>.json, a DSSE
# envelope naming it, which is the layout served at
# https://rbpkg.com/bootstrap/v1/.
#
#   scripts/bootstrap-sign.sh <key.pem> <out> <platform> <serial> <version> <revision> <archive>
set -euo pipefail

# The envelope counts bytes, so lengths must never be locale characters.
export LC_ALL=C

key=$1
out=$2
platform=$3
serial=$4
version=$5
revision=$6
archive=$7

type=application/vnd.rootbeer.bootstrap.v1+json
sha=$(openssl dgst -sha256 -r "$archive" | cut -d ' ' -f 1)
mkdir -p "$out/archives"
cp "$archive" "$out/archives/$sha.tar.gz"

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

jq -cjn \
    --arg platform "$platform" \
    --argjson serial "$serial" \
    --arg version "$version" \
    --arg url "https://rbpkg.com/bootstrap/v1/archives/$sha.tar.gz" \
    --arg sha256 "$sha" \
    --arg revision "$revision" \
    '{$platform, $serial, $version, $url, $sha256, $revision}' > "$work/payload"

# DSSE signs a pre-authentication encoding, not the payload alone.
length=$(wc -c < "$work/payload" | tr -d ' ')
{
    printf 'DSSEv1 %s %s %s ' "${#type}" "$type" "$length"
    cat "$work/payload"
} > "$work/pae"

openssl pkeyutl -sign -inkey "$key" -rawin -in "$work/pae" -out "$work/sig"
keyid=$(openssl pkey -in "$key" -pubout -outform DER | tail -c 32 | od -An -v -tx1 | tr -d ' \n')

jq -cn \
    --arg type "$type" \
    --arg payload "$(openssl base64 -A -in "$work/payload")" \
    --arg keyid "$keyid" \
    --arg sig "$(openssl base64 -A -in "$work/sig")" \
    '{payloadType: $type, $payload, signatures: [{$keyid, $sig}]}' > "$out/$platform.json"

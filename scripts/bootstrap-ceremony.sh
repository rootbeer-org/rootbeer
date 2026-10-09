#!/usr/bin/env bash
# Makes the bootstrap release keys. The active key signs every release in CI,
# and the offline backup can sign alone, so a lost or leaked active key is
# replaced without a client release. Prints each raw public key, which rb
# embeds.
#
#   scripts/bootstrap-ceremony.sh <directory>
#
# Afterwards active.pem becomes the release environment's RB_BOOTSTRAP_KEY
# secret. Keep backup.pem offline and delete both here.
set -euo pipefail

out=$1
if [[ -e $out ]]; then
    echo "$out already exists" >&2
    exit 1
fi

umask 077
mkdir -p "$out"
for key in active backup; do
    openssl genpkey -algorithm ed25519 -out "$out/$key.pem"
    public=$(openssl pkey -in "$out/$key.pem" -pubout -outform DER | tail -c 32 | od -An -v -tx1 | tr -d ' \n')
    echo "$key $public"
done

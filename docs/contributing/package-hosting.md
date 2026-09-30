# Package distribution and trust

| Site            | Published content                                                             |
| --------------- | ----------------------------------------------------------------------------- |
| `rbpkg.com`     | Documentation, package search, installer, and nightly CLI binaries.           |
| `pdr.rbpkg.com` | The signed root, one document per package, and independently signed records. |
| GHCR            | Source builds and byte-identical upstream mirrors, addressed by digest.       |

The web UI and `rb search` read the signed root alone. Installing a package fetches
that package's document and the one signed record it names, then the artifact: a
source build from GHCR, or an upstream binary from its vendor or mirror. No other
package's data is downloaded.

## Published files

Everything except the root is addressed by digest, beside the root. Trust comes from
the root's signature and those digests, never from a URL, so a mirror is a copy of the
directory.

- `current.json`: the signed root. Per package: metadata, license, maintainers,
  publication times, and for each platform the installable version, its kind and
  commands; plus the digest of the package's document.
- `roots/<sha256>.json`: immutable copies of each root, which locks pin.
- `packages/<sha256>.json`: every published version of one package, with each
  platform's recipe, record digest, and publication time.
- `records/<sha256>.json`: individually signed package records identifying exact
  artifact bytes, installation instructions, provenance, and when they were signed.

The root is signed as compact JSON with recursively sorted object keys:

```text
["rootbeer-pdr-v3",sequence,packages]
```

Each package record signs its exact embedded JSON bytes. A record distinguishes
source-build evidence from verified upstream binaries.

A package job prepares and checks one package/platform, retains the result, then
signs and uploads it to GHCR. `rootbeer-forge publish-records` adds successful
records to those the previous root carried, keeping a previous record only while the
catalog still approves exactly its recipe and revision. It rebuilds and downloads
nothing. A platform keeps listing its previous version until its new default is
published, so a failed package never takes a working one away.

## Release configuration

CLI and website builds embed `ROOTBEER_PDR_URL`, the URL of `current.json`, and
`ROOTBEER_PDR_PUBLIC_KEY`. Set both together. The key is a 32-byte Ed25519 public
key encoded as 64 lowercase hex characters. The private key belongs only in the
publishing environment and its secure backup.

Rootbeer checks the root's signature and rejects rollback relative to its verified
local history. It checks package documents and records against their digests,
verifies the record's signature, identity, platform, and recipe, then checks artifact
hashes during installation. Fresh clients have no previous sequence to compare. The
root has no expiry policy.

A package whose `min_engine_level` is above what this rb implements is listed by
search as needing a newer rb, and installing it asks you to run `rb self-update`.
A root whose schema is newer than this rb fails with the same instruction.

Matching locks require no refresh. On network availability errors, Rootbeer uses
its verified cached root and reports that fallback. Invalid signatures, corrupt
metadata, and rollback attempts fail. `rb apply --update` requires a successful
refresh; offline use requires cached metadata and contents. A missing published
package never triggers an implicit local source build.

Keep roots, package documents, records, and GHCR blobs available for existing locks.
If moving the endpoint, copy the directory, preserve cross-origin browser access,
then rebuild the CLI and website with the new URL. Cached trust history is scoped to
endpoint and verification key.

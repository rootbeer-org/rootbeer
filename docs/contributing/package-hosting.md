# Index hosting and trust

| Location                       | Published content                                       |
| ------------------------------ | ------------------------------------------------------- |
| `rbpkg.com`                    | Documentation, the installer, and the bootstrap channel |
| `rootbeer-org.github.io/index` | The signed package index, a TUF repository              |
| `ghcr.io/rootbeer-org/store`   | Package outputs, addressed by derivation key            |

Trust comes from the index's signatures, never from a URL. The registry is
untrusted storage.

## The index

[rootbeer-org/index](https://github.com/rootbeer-org/index) serves `metadata/`
and `targets/` from GitHub Pages. Each target `<name>.json` lists a package's
description, license, default version per platform, and every version's outputs:
the derivation key, the manifest digest, and its commands and macOS apps.

The offline root keys sign `root.json` and never come near CI. pdr's push to main
writes new entries with `rb drv index`, and the index's sign workflow merges them
and signs with the online key. A daily run renews the signatures, since TUF
metadata expires.

`rb` embeds the initial root and refreshes through
[`tough`](https://github.com/awslabs/tough), which handles key rotation, expiry,
and rollback. Each trusted root keeps its own metadata under
`~/.local/state/rootbeer`.

## Installing

Installing a package refreshes the index, reads that package's target, and picks
a version for the platform. `rb` then pulls the output's OCI manifest from
`ghcr.io/rootbeer-org/store` by the signed digest, does the same for each store
reference, and hands the archive to `rb-helper pull`, which checks its digest.

A rebuild retires an output's old key. It stays installable for existing locks,
since the index keeps its manifest signed.

`--offline` never opens the index, so only outputs already in the store will do.
GitHub release requests and local recipe checkouts are not signed by the index.
Profiles and `rootbeer.lock` record them as such.

## The bootstrap channel

`rbpkg.com/bootstrap/v1/<platform>.json` names the newest released `rb` for each
platform: a DSSE envelope over its version, serial, archive URL, and SHA-256,
signed with an Ed25519 key `rb` embeds. The installer starts from it. An `rb`
that meets state written by a newer one, such as a store layout, generation, or
lock schema it doesn't know, fetches the release, checks the signature, serial,
and digest, and runs it in its place.

Rootbeer's deploy workflow publishes it on every push to main. It installs the
`rootbeer` package from the signed index, checks that its serial is newer than
what's served and that its commit descends from the served one, signs the
releases, and deploys them with the site. A gate then checks that the previous
release relaunches into the new one.

The protocol is frozen. Changes to `rootbeer-bootstrap` are additive only.

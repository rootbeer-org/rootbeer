# Updates and offline use

Rootbeer keeps the versions it installed so your tools don't change unexpectedly.
`rb use` records them in your user profile's generations; Lua configurations save
them in `rootbeer.lock` alongside `init.lua`. `rb run` records nothing and uses
the index's current default each time.

## Update tools installed with use

```sh
rb use --update jq ripgrep
rb use --update
```

`rb use --update` moves the packages you name, or every package with no names, to
what the index has now, keeping your other tools. An exact request such as
`jq@1.8.2` pins that version, including with `--update`, though it can move to a
rebuild of the same version. Packages built from a local checkout are left alone
unless you name them.

These commands do not read or modify your configuration's `rootbeer.lock`.

## Update a Lua configuration

```sh
rb apply --update
```

This refreshes packages declared in your configuration and saves the results in
`rootbeer.lock`. Review and commit that file alongside `init.lua`. Exact versions
in your configuration stay fixed.

| Command | Versions and lockfile | Package network access |
| --- | --- | --- |
| `rb apply` | Keep locked entries that still answer their declarations; resolve new or changed ones. | Allowed when needed. |
| `rb apply --update` | Resolve every declaration again, respecting explicit version pins. | Allowed. |
| `rb apply --locked` | Require a matching lock; never write it. | Allowed to fetch locked outputs. |
| `rb apply --offline` | Resolve nothing new; fail if a locked output isn't in the store. | None. |
| `rb apply --locked --offline` | Require a matching lock and every locked output in the store. | None. |

`--locked` and `--offline` are independent. `--update` conflicts with either.
Adding a package during plain apply does not upgrade unrelated packages. Removing
packages updates the lock, including when the final package is removed.

A lock from an older `rb` is replaced on the next apply without `--locked`. A
lock written by a newer `rb` makes Rootbeer run that newer release instead.

## Update Rootbeer

`rb self-update` (also `rb update`) installs the latest `rootbeer` package from
the signed index into your user profile. A pinned version stays pinned. If your
Lua configuration installs Rootbeer, it directs you to `rb apply --update`
instead and never changes your lock implicitly.

## What offline mode covers

`--offline` never contacts the signed index or GitHub, so a saved lock alone is
not enough: package outputs must already be in `/opt/rb`. Rootbeer reports what's
missing instead of downloading it. This includes the 1Password CLI that
[`rb.secret.op`](/reference/secrets) installs, so secret providers that need it
fail offline.

Offline mode controls package resolution and downloads. A tool launched by
`rb run`, or a command in your Lua configuration, can still access the network.

## Using multiple machines

`rootbeer.lock` keeps separate entries for each platform. Applying on another
platform adds that platform's entries and leaves the others alone; `--locked`
fails if this platform has none yet. [Profiles](/guide/profiles) can choose
packages per machine.

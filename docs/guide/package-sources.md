# Other package sources

[Browse the catalog](https://search.rbpkg.com) for packages available by name. For another tool,
you can request a GitHub release with `rb run`, `rb use`, or `rb.packages()`.

## Build packages locally

To try a recipe before it's published, point your configuration at a checkout
of the package index:

```lua
local rb = require("rootbeer")

rb.package_catalog("~/code/index")
rb.packages({ "my-tool", "jq" })
```

The checkout holds `packages/<name>.lua` recipes and `host.lua`, like the index
repository. A name it defines comes from its recipe, and everything else still
comes from the signed index. When the index publishes the same build, Rootbeer
downloads it, and otherwise it builds on your machine and runs the recipe's
checks.

Builds use your machine's toolchain in place of the one `host.lua` names, so a
source build and its source-built dependencies build locally rather than
matching the index. Prebuilt recipes don't depend on the toolchain and still
match. On Linux, source builds need the builder image.

If the checkout doesn't exist, as on another machine sharing the
configuration, Rootbeer warns and the index serves those names.

`rootbeer.lock` marks these packages as local and records the key each recipe
evaluated to. Editing a recipe in a way that changes the build makes
`--locked` fail until you apply without it.

For a one-off, `rb use --catalog ~/code/index my-tool` installs from a checkout
into your user profile. `rb use --update` with no names leaves packages built
this way alone, while naming one without `--catalog` moves it to the index's.

With `--offline`, only builds already in the store will do.

## Install from GitHub

Use `github:owner/repository@tag`:

```sh
rb run github:BurntSushi/ripgrep@15.2.0 -- --version
rb use github:BurntSushi/ripgrep@15.2.0
```

Or declare the same request in your configuration:

```lua
local rb = require("rootbeer")

rb.packages({ "github:BurntSushi/ripgrep@15.2.0" })
```

The release tag must match exactly, including a leading `v` when the project uses
one. Rootbeer selects a release asset for your platform. Supported formats include
tar.gz, tar.xz, ZIP, and standalone executables.

If automatic selection is ambiguous, put the request and its options in one entry.
Choose an exact release asset and name its exported command paths:

```lua
rb.packages({
    "jq",
    {
        request = "github:BurntSushi/ripgrep@15.2.0",
        asset = "ripgrep-15.2.0-aarch64-apple-darwin.tar.gz",
        bins = { rg = "ripgrep-15.2.0-aarch64-apple-darwin/rg" },
    },
})
```

This asset targets Apple silicon macOS; select platform-specific declarations with
normal Lua and `rb.host`. Keep versions in the request's `@version` syntax and
the resolver in its `github:` prefix. `asset` and `bins` are options for
explicit GitHub requests. The CLI's `--bin` chooses an already exported command;
it does not select a release asset or an archive path.

`rb.package(entry)` accepts the same individual entry. Exact raw package tables
can also appear in the list; see the [package API](/reference/core#rootbeer-packages).

Direct GitHub requests record download hashes in their saved resolutions. They
do not carry the Rootbeer repository publisher's signature.

See [updates and offline use](/guide/package-locks) for saved versions, or
[repository hosting and trust](/contributing/package-hosting) for verification and
fallback behavior.

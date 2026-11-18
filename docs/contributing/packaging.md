# Package authoring

Package recipes live in [pdr](https://github.com/rootbeer-org/pdr), one Lua file
per tool under `packages/`. Its
[README](https://github.com/rootbeer-org/pdr#readme) is the source of truth for
contributing. Start from an existing recipe such as `jq.lua`:

```lua
return {
    name = "jq",
    description = "Query and transform JSON",
    homepage = "https://jqlang.org",
    recipe_maintainers = { "tale" },
    default_license = "MIT",
    upstream = { github = "jqlang/jq", repository_id = 5101141, tag = "jq-{version}" },
    prebuilt = { github = "jqlang/jq", tag = "jq-{version}", asset = "jq-{target}" },
    outputs = {
        bins = { "jq" },
        checks = {
            { "jq", "--version" },
            { "jq", "--null-input", "--exit-status", "[1,2,3] | add == 6" },
        },
    },
    platforms = {
        ["aarch64-macos"] = { target = "macos-arm64", default_version = "1.8.2" },
        -- ...
    },
    versions = {
        ["1.8.2"] = {
            digests = { ["aarch64-macos"] = "2d75340b..." },
            revision = 3,
        },
    },
}
```

- Use the canonical lowercase name and exact upstream versions.
- Each version lists its source or asset digest per platform under `digests`.
- Keep older versions, since locks pin them. Shared fields apply to every
  retained version and rekey it, so give a build change to one version as an
  override on that version.
- Declare only platforms CI can verify, with checks that exercise the tool.
- Keep a source's patches as `.patch` files in a directory named after the
  recipe, such as `packages/unzip/`, and list their file names in order under
  `source.patches`. A patch's content keys the build.
- List `source.mirrors` for fallback URLs tried in order after `source.url`,
  with the same placeholders. They don't change keys, so use only URLs that
  serve the same bytes.
- A `source.archive` of `zip` or `tar.bz2` extracts with the catalog's 7zip,
  so list `7zip` in `build.dependencies` with `kind = "build"`.
- Linked dependencies' `bin/` follows build dependencies' on `PATH`, so a
  library's `*-config` tool is found. `CPATH`, `LIBRARY_PATH`, and
  `PKG_CONFIG_PATH` cover direct linked dependencies only.
- Linking a package whose `build.libraries` include `{shared_extension}` as
  `link` or `all` fails evaluation, since only `link_runtime` gives the output
  a run-path to it. List it as `build` too if you also need its commands.

## Build and check locally

Use an `rb` from Rootbeer at pdr's `.github/rb-engine-revision` or newer. In a
Rootbeer checkout, `scripts/bin/rb` runs the working tree. From a pdr checkout:

```sh
rb drv build jq
```

This evaluates the recipe against `packages/` and `host.lua`, fetches or builds
it and its dependencies in the sandbox, and runs its checks. On macOS, pass
`--host host.local.lua` naming this machine's toolchain. For Linux, run
`RB_CATALOG=<pdr checkout> scripts/linux.sh build jq` from Rootbeer, which uses
the pinned builder image.

`rb drv show jq` prints a package's derivations and keys. To see what a change
rekeys, compare `rb drv keys` before and after with `rb drv diff`.

To try a recipe from your own configuration before it's published, see
[build packages locally](/guide/package-sources#build-packages-locally).

## CI and publication

pdr CI plans every declared version against `ghcr.io/rootbeer-org/store` and builds
only what the store lacks, one job per platform in dependency order. A failed
package holds back only what depends on it.

Pull requests build and check the keys they change and stage each output once it
passes. Main installs what's staged instead of rebuilding it and publishes to the
store, then the [index](/contributing/package-hosting) signs the new entries
with a key pdr never sees.

# Contribute packages

Package recipes live in the [package distribution repository](https://github.com/rootbeer-org/pdr)
(PDR). Rootbeer owns the package engine; the PDR owns recipes and publication. A new
recipe normally needs no Rust changes or Rootbeer release.

Discovery, package builds, and publication run in the PDR's CI, which pins Forge with
its `package-engine-revision` file. Engine CI here runs regression tests only. The
engine ships no recipes: pass `--catalog` with the `packages/` directory of a PDR
checkout to every Forge command that reads recipes.

```sh
cargo build --bin rootbeer-forge
rootbeer-forge --catalog ../pdr/packages check
rootbeer-forge --catalog ../pdr/packages build jq --output /tmp/jq-build
```

## Write a recipe

A recipe is one Lua file in `packages/`, named after the package. It declares an
identity, how each platform gets its artifact, what it exports, and a digest per
platform for every version:

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
        ["aarch64-linux"] = { target = "linux-arm64", default_version = "1.8.2" },
        ["aarch64-macos"] = { target = "macos-arm64", default_version = "1.8.2" },
        ["x86_64-linux"] = { target = "linux-amd64", default_version = "1.8.2" },
    },
    versions = {
        ["1.8.2"] = {
            digests = {
                ["aarch64-linux"] = "8b85c817…",
                ["aarch64-macos"] = "2d75340b…",
                ["x86_64-linux"] = "b1c22172…",
            },
        },
    },
}
```

- `recipe_maintainers` are the people who own the recipe, not the upstream authors.
- `default_license` is an SPDX expression; `NOASSERTION` is the explicit unknown. A
  version can set its own `license`, which is how a relicense is recorded.
- `aliases` lists alternate names, such as `rg` for ripgrep.
- `min_engine_level` marks a recipe that needs a newer client. Older `rb` builds skip
  it and show it as needing a newer version rather than rejecting the whole catalog.

Unknown fields fail validation. Run
`rootbeer-forge --catalog packages format` after editing so that discovery's later
rewrites don't reflow the file; CI runs `format --check`.

### Prebuilt or source

Each platform gets its artifact exactly one way:

- `prebuilt` downloads upstream's binary. The PDR attests it is byte-for-byte what
  upstream published.
- `source` plus `build` compiles it. The PDR attests the inputs and the build.

When upstream ships a usable binary, prefer `prebuilt`. A platform declaring both
fails validation. A prebuilt names exactly one provider:

```lua
prebuilt = { github = "sharkdp/fd", tag = "v{version}", asset = "fd-{tag}-{target}.tar.gz" },
prebuilt = { url = "https://release.files.ghostty.org/{version}/Ghostty.dmg", install = "Dmg" },
```

A `github` prebuilt needs an `asset`; its `tag` defaults to the upstream tag. A `url`
prebuilt downloads the URL directly. Archives and standalone executables are detected
from the file name; `install` selects anything else, such as `Dmg`.

### Platforms

The keys of `platforms` are the supported systems: `aarch64-macos`, `aarch64-linux`,
and `x86_64-linux`. Declare only platforms you can verify. Each platform sets its
`default_version` and, optionally, a `target` substituted for `{target}` in its
templates. `target` is free-form because upstream naming conventions vary
(`aarch64-apple-darwin`, `macos-arm64`, `linux-x64-musl`).

A platform can override `upstream`, `prebuilt`, `source`, `build`, or `outputs`. An
override replaces the shared field, except `outputs`, which overlays per field: a
platform overriding `apps` keeps the shared `bins`. This lets one package use a
different upstream per platform:

```lua
platforms = {
    ["aarch64-macos"] = {
        default_version = "0.17.2.1",
        upstream = { github = "imputnet/helium-macos" },
        prebuilt = {
            github = "imputnet/helium-macos",
            asset = "helium_{version}_arm64-macos.dmg",
            install = "Dmg",
        },
        outputs = { apps = { ["Helium.app"] = "Helium.app" } },
    },
    ["x86_64-linux"] = {
        default_version = "0.17.2.1",
        upstream = { github = "imputnet/helium-linux" },
        prebuilt = { github = "imputnet/helium-linux", asset = "helium-{tag}-x86_64.AppImage" },
        outputs = { bins = { "helium" } },
    },
},
```

A platform can also build from source while the others use upstream binaries.

### Versions

A version is a digest per platform. Which platforms carry a version is implied by
which have a digest, so platforms can advance independently: a platform whose asset
disappeared from newer releases keeps its older `default_version`. Retain older
versions; explicit version requests never fall back.

For a prebuilt, the digest pins the downloaded artifact. For a source build, it pins
the source archive. Digests are never shared or inferred from version names.

`revision` defaults to 1. Increment it when a recipe change alters the bytes of an
existing version. A version can also override `prebuilt`, `source`, `build`, or
`outputs`, but keep that as a rare escape hatch. Shared changes affect every retained
version, so review them with `rootbeer-forge --catalog packages show <name>`.

### Templates

URLs, assets, tags, strip prefixes, Go linker variables, configure flags, build
arguments, build steps, and checks accept these placeholders:

| Placeholder | Value |
| --- | --- |
| `{version}` | The version, such as `8.22.0` |
| `{tag}` | The upstream release tag for the version |
| `{target}` | The platform's `target` |
| `{major}`, `{minor}`, `{patch}` | Components of the version before any `-` or `+` |
| `{commit}` | The version's recorded `commit` |

`{commit}` requires the version to record a full lowercase SHA-1 `commit`; discovery
records it automatically when a template uses it. An unknown placeholder in a URL,
asset, or path fails validation. Commands keep other braces as written, so checks
can contain shell or Python, and the build fills `{prefix}`, `{dependencies}`, and
`{jobs}` itself.

### Outputs

`outputs.bins` lists exported commands. Use a list when they sit at their default
paths, or a map from command name to path when they don't:

```lua
bins = { kitty = "kitty.app/Contents/MacOS/kitty", kitten = "kitty.app/Contents/MacOS/kitten" },
```

Paths must stay inside the artifact. The full archive tree is retained, including
resources and app bundles.

Checks run during qualification, as argument arrays without shell interpolation.
They should exercise the tool's real functionality and bundled runtime files, not
only `--version`. Don't depend accidentally on a developer's Homebrew installation
or CI-only libraries.

### macOS apps and disk images

Declare app exports explicitly:

```lua
outputs = { apps = { ["Ghostty.app"] = "Ghostty.app" } },
```

Keys are exported `.app` filenames; values are relative paths to the bundles. App
platforms must be macOS. `rb use` and Lua package application manage links in
`~/Applications`; existing apps and unmanaged links cause a conflict. Temporary
`rb run --app` launches do not create these links. Neither configures login items
or grants permissions. App-only recipes may omit `bins` and `checks`: qualification
checks each bundle's `Info.plist`, executable, and deep code signature without
launching it.

`install = "Dmg"` mounts the verified image read-only, copies only the declared
bundles, and detaches the image even when copying fails. Finder backgrounds and the
`/Applications` shortcut are not included. Internal relative symlinks are preserved;
links outside a bundle are rejected. Bundles with extended-attribute code signatures
are rejected because the package archive cannot preserve them; embedded signatures
remain intact. PKG installers and installer scripts are unsupported.

### Moving releases and mirroring

A moving tag such as `tip` or a "latest" URL is not a version. Give each approved
snapshot an exact version and pin its digest. Set `mirror = true` on the prebuilt
to retain the qualified artifact in the PDR's registry, so old installations keep
working if upstream replaces or removes it. Receipts retain the original URL and
digest. Omit `upstream` for these recipes and update them by hand.

## Track upstream releases

`upstream` describes release discovery, independently of how the package is built:

```lua
upstream = {
    github = "curl/curl",
    repository_id = 569041,
    tag = "curl-{version}",
    separator = "_",
},
```

- `tag` maps a version to its release tag; it defaults to `{version}`.
- `separator` replaces the dots of a version inside its tag (`curl-8_22_0`).
- `exclude_tags` drops unrelated tags that would otherwise fail discovery.
- `repository_id` pins the repository's identity, so a rename or takeover stops
  discovery. Discovery fills it in on first run.

GitHub is the only upstream provider today. Omit `upstream` to opt out of discovery.

```sh
rootbeer-forge --catalog packages updates --cache .upstream-metadata --output candidates
```

Discovery considers stable dotted-numeric versions and skips drafts, prereleases,
excluded tags, and tags the template doesn't match. Tags normalizing to the same
version fail rather than guess. Each platform advances to the newest release that
publishes what it downloads, and never moves below its current version. For a GitHub
prebuilt, the digest is the one GitHub publishes for the asset; for a URL or source
archive, discovery downloads it once and hashes the bytes. A new version inherits
`default_license`. Nothing is built or executed during discovery.

`updates` writes `report.json`, `summary.md`, and a complete candidate catalog in
`candidates/packages/` when recipes change. One platform's failure doesn't hold back
the others; errors are reported with a nonzero exit after successful candidates are
written. The metadata cache sends ETags and reuses responses only after HTTP 304; use
a trusted cache directory, since its entries are not signed. `GITHUB_TOKEN`
authenticates API requests, and `--max-pages` (20 by default) bounds release history.

The PDR runs discovery daily and proposes each update as a pull request, which goes
through the same package CI as a hand-written change.

Rootbeer itself has a separate path. Successful engine CI pushes a `rootbeer-update`
request to the PDR, with a 15-minute scheduled poll as fallback. Both resolve the
current main commit, require successful CI, hash its source archive, and retain
existing versions. The notification workflow mints a short-lived token from the
Rootbeer Bot GitHub App (`ROOTBEER_BOT_CLIENT_ID`, `ROOTBEER_BOT_PRIVATE_KEY`), scoped
to the PDR. Updating the `rootbeer` package does not advance `package-engine-revision`.

## Source builds

A source platform declares the archive under `source` and how to compile it under
`build`:

```lua
source = {
    url = "https://github.com/owner/tool/releases/download/{tag}/tool-{version}.tar.gz",
    archive = "tar.gz",
    strip_prefix = "tool-{version}",
},
build = {
    backend = "autotools",
    configure = { "--disable-shared", "--with-openssl={dependencies}" },
    dependencies = { "openssl@4.0.2", "zlib@1.3.2" },
},
```

`archive` is `tar.gz` (the default), `tar.xz`, or `zip`. The backend is `autotools`,
`rust`, `go`, `zig`, or `custom`. Recipe commands and upstream build scripts execute
trusted code.

Use `source.patches` for reviewed unified diffs applied with `-p1` before
compilation. Patches are part of the recipe and build receipt. Keep them narrowly
focused, such as pinning a git-derived version or replacing a fixed installation path
with executable-relative lookup.

For development builds, add `git = { github = "owner/repo", branch = "main" }` under
`source`. The branch defaults to the repository's HEAD. Declare it only when the build
works on Git archives, including any preparation release archives normally provide.
GitHub archives don't include submodules. Pin the archive URL to a full commit and use
an exact snapshot version.

### Rust

```lua
build = {
    backend = "rust",
    rust = {
        packages = { "rootbeer-cli", "rootbeer-forge" },
        environment = { RB_SOURCE_REVISION = "486a952cf43a3f8183b95f064bb4914c3a05ff5e" },
    },
},
```

`packages` names the workspace packages to install. `features` and
`no_default_features` select Cargo features; `environment` sets build-time variables.
The pinned Rust sysroot is part of the build identity. Upstream test suites don't run;
`outputs.checks` verify the installed commands.

### Go

```lua
build = {
    backend = "go",
    go = {
        binaries = { tool = "./cmd/tool" },
        variables = { ["main.version"] = "v{version}" },
        tags = { "netgo" },
    },
},
```

`binaries` must match `outputs.bins`. CGO is disabled unless `cgo = true`. The source
archive must contain `go.mod` and `go.sum`. Fetch vendors dependencies and verifies
their checksums, rejecting changes to either module file. The build uses that vendor
tree with module downloads disabled, followed by the recipe's checks. Upstream lint and
test suites don't run; `outputs.checks` verify the installed commands.
`generate` selects local packages whose `go generate` directives run offline before
compilation; generators must already be vendored or declared build dependencies.
`experiments` selects `GOEXPERIMENT` values. Projects needing frontend assets must
provide them in the verified source or use a custom build.

Host builds hash the installed Go compiler and its toolchain directory. Pinned
environments must declare `tools.go` and `inputs.go-toolchain` pointing to that
compiler's GOROOT. Rootbeer disables automatic toolchain downloads, workspace
discovery, and user Go configuration.

### Zig

Zig recipes must depend on an exact catalog compiler such as `zig@0.16.0`. Use `args`
for project `-D` options; Rootbeer controls the install prefix and isolates build
caches. Installed commands belong under `bin/`, and runtime resources must keep
working after the installation is moved.

### Custom build phases

Use `backend = "custom"` when a project doesn't fit a preset. Each phase contains
argument arrays, run in order from the unpacked source root:

```lua
steps = {
    configure = {
        { "perl", "./Configure", "--prefix=/", "--libdir=lib",
          "--openssldir=/etc/ssl", "no-shared", "no-module" },
    },
    build = { { "make", "-j{jobs}" } },
    check = { { "make", "test" } },
    install = { { "make", "DESTDIR={prefix}", "install_sw" } },
}
```

`{prefix}` is the isolated staging directory, `{dependencies}` is the merged dependency
prefix, and `{jobs}` is the compiler parallelism limit. `{jobs}` must not change the
output. Build, check, and install must contain commands; configure may be empty.
Arguments are passed directly, without a shell: for shell syntax, invoke `sh`
explicitly and pass paths as positional arguments.

### Dependencies

`build.dependencies` takes exact packages. A bare string exposes everything; a table
scopes the dependency:

```lua
dependencies = {
    { package = "cmake@4.4.3", kind = "build" },
    { package = "zlib@1.3.2", kind = "link" },
},
```

| Kind | Effect |
| --- | --- |
| `build` | Exposes its commands on `PATH` |
| `link` | Exposes its libraries and headers, including transitive link inputs, without its build tools |
| `all` | Both; the same as a bare string |
| `runtime` | Installs it with the package, without exposing commands or headers |
| `link_runtime` | Exposes link inputs and installs it |

Runtime edges propagate only through other runtime edges; a compiler's own runtime
dependencies remain build inputs for its consumer.

Source-capable dependencies always use their source recipe, even when upstream
binaries exist. With `--pdr`, a dependency the PDR has published for its current recipe
and revision installs from its signed record instead of compiling, and its dependents'
input keys name that published build. Otherwise verified outputs are reused from the
build cache, and a miss compiles the dependency. Binary-only dependencies use their
pinned upstream artifacts. A failed source build never falls back to an upstream binary.

### Libraries

Declare exported libraries in `build.libraries`, such as
`libraries = { "lib/libssl.a", "lib/libcrypto.a" }`. Use self-contained `.a`,
`.dylib`, `.so`, or versioned `.so.*` files under `lib/` or `lib64/`. Library-only
packages set `bins = {}` and `checks = {}`, and their build must run the upstream test
suite.

Rootbeer makes the transitive dependency set available during a build: commands on
`PATH`, headers under `include/`, libraries under `lib/` and `lib64/`, and pkg-config
metadata. Conflicting exports fail the build. The builder sets `CPATH`,
`LIBRARY_PATH`, `PKG_CONFIG_LIBDIR`, and `PKG_CONFIG_SYSROOT_DIR` for the merged
prefix; declare `pkgconf` when the project needs the tool. Metadata should use the
prefix `/`.

Static dependencies become part of the consuming binary. Shared libraries need
`link_runtime` edges and loader-relative references to their store entries. Rootbeer
does not rewrite binaries or set global loader variables.

### Runtime layout

Runtime packages occupy separate content-addressed store entries. The placeholder
`{runtime:name@version}` expands to a runtime dependency's pinned directory name, such
as `sha256-<hash>-name-version`. For a binary in `bin/` or a library in `lib/`:

```lua
"-Wl,-rpath,$ORIGIN/../../{runtime:example-library@1}/lib"
```

On macOS use `@loader_path` in place of `$ORIGIN`, and give shared libraries an
`@rpath/libname.dylib` install name. These are literal arguments; shell commands and
Makefiles need their own dollar-sign escaping.

Installation verifies and realizes the runtime closure before the requested output,
including on cache hits. Runtime commands are not added to the user profile. Build
output includes `runtime/` archives beside the root archive and receipt; keep them
together when moving builds.

## Build and check locally

```sh
rootbeer-forge --catalog packages check           # validate every recipe, print the catalog digest
rootbeer-forge --catalog packages show jq         # one package's resolved recipes
rootbeer-forge --catalog packages plan curl       # dependency graph, without fetching
rootbeer-forge --catalog packages build curl --output /tmp/curl-build \
  --cache /tmp/rootbeer-builds --cache-context "$BUILD_ENVIRONMENT_ID"
```

`build` (also `prepare`) resolves binary inputs up front, builds or downloads the
package and its dependencies, runs the checks and audit, and writes an installable
archive and receipt. A local success only covers the current platform; PDR CI checks
every declared platform. `list` prints names and defaults; `catalog` prints the
expanded catalog as JSON.

The build cache reuses dependencies across root packages. Its key covers compilation
inputs and revision, verified dependency outputs and library exports, platform, build
backend, environment variables, tool hashes, and the `--cache-context` host identity.
Checks and job allocation don't change build keys, so changing only checks reruns them
against cached outputs. Cache hits verify archive and output hashes and rerun checks.
`--recheck` rebuilds each node; `--phase-timeout SECONDS` bounds each command.

Builds set `SOURCE_DATE_EPOCH=1` and `ZERO_AR_DATE=1`. Failed phases keep their scratch
directory, including files such as `config.log`; the error reports its path.

### Pinning a build environment

Without a lock, Rootbeer hashes the host compiler and basic tools, but the
`--cache-context` identity must account for SDKs, libraries, and other utilities. A
lock pins them explicitly. Write a specification with absolute executable paths in
`tools`, SDK and compiler resource directories in `inputs`, and explicit `variables`:

```json
{
  "tools": {
    "sh": "/opt/toolchain/bin/sh",
    "cc": "/opt/toolchain/bin/cc",
    "c++": "/opt/toolchain/bin/c++",
    "make": "/opt/toolchain/bin/make",
    "patch": "/opt/toolchain/bin/patch"
  },
  "inputs": {
    "toolchain": "/opt/toolchain",
    "sysroot": "/opt/sysroot"
  },
  "variables": {
    "CFLAGS": "--sysroot=/opt/sysroot -O2",
    "LDFLAGS": "--sysroot=/opt/sysroot"
  }
}
```

List every other utility the recipe needs, such as `ar`, `ranlib`, `sed`, `mkdir`,
and `cp`, and declare script interpreters at their exact paths. On macOS, declare the
actual compiler and its resource directory, include the SDK in `inputs`, and set
`SDKROOT`. Use `xcrun --find clang`, `xcrun --find make`, and similar: `/usr/bin` holds
developer-directory shims. Hashing an SDK reads the full tree. Directory symlinks must
resolve within their input root.

```sh
rootbeer-forge pin-environment environment.json > environment.lock.json
rootbeer-forge --catalog packages build xz --output /tmp/xz-build \
  --environment environment.lock.json \
  --cache /tmp/rootbeer-builds --cache-context "$BUILD_ENVIRONMENT_ID"
```

A lock pins tool bytes, input tree contents, platform, paths, and variables. The
executor verifies it before cache lookup and after each package's checks; changed
inputs fail the build. Pinned execution clears inherited variables and puts only
dependency and declared tools on `PATH`. Rootbeer owns `PATH`, `CC`, `CXX`, shell
selection, temporary directories, locale, and package-config lookup. Dependency paths
are prepended to `CPATH` and `LIBRARY_PATH`; your `CPPFLAGS` and `LDFLAGS` are kept.

### Isolation

Add `--isolate` (which requires `--environment`) to enforce filesystem and network
restrictions while package code runs. Isolation never falls back to host execution.
Sources are downloaded and extracted first; tool probes, patches, build phases, and
checks all run inside the sandbox, including checks on cache hits.

Declared tools, SDK inputs, and dependencies are read-only. Each build or check gets
its own writable scratch directory. Undeclared files and all network access, including
loopback, are denied. The OS runtime remains a declared exception: macOS loader and
framework locations and Linux library and loader locations are readable, and macOS
permits filesystem metadata queries. This is not a fully pinned OS image, so these
inputs still need a stable `BUILD_ENVIRONMENT_ID`.

On macOS, Rootbeer uses `/usr/bin/sandbox-exec`. On Linux, install Bubblewrap at
`/usr/bin/bwrap`; the host must permit user, mount, process, and network namespaces.
Restricted containers can prevent this, and Rootbeer reports a startup failure rather
than reducing isolation. On Ubuntu 24.04, install `apparmor-profiles` and load
`/usr/share/apparmor/extra-profiles/bwrap-userns-restrict`. Compiler helpers must be
declared too: GCC reports its `cc1` with `cc -print-prog-name=cc1`.

Cache keys distinguish host and isolated execution, and receipts record the isolation
identity.

### Auditing native outputs

```sh
rootbeer-forge audit /path/to/installed/package
rootbeer-forge audit /path/to/store/package --receipt /path/to/receipt.json
```

The audit emits a JSON report and fails when native loader references can't be
justified. It parses ELF and Mach-O metadata, including every slice of universal
binaries, without executing package code. Source builds run it before packaging and
again on realized outputs, including cache hits; failed audits can't enter the cache.

Libraries must resolve inside the package or its pinned runtime closure through
loader-relative paths: ELF `$ORIGIN` or Mach-O `@loader_path`, `@executable_path`, and
`@rpath`. Resolution checks architecture, word size, and byte order, and follows
inherited rpaths along dependency chains. Absolute library identities,
working-directory searches, undeclared host prefixes, and missing bundled libraries
fail even if the current machine could load them.

The OS baseline is explicit: macOS libraries under `/usr/lib` and
`/System/Library/Frameworks`, and Linux libc, libm, libdl, libpthread, librt,
libgcc_s, libstdc++, and the glibc/musl loaders. The audit does not check symbol
versions or OS version compatibility. Scripts, static archives, object files, and
runtime `dlopen` calls are outside its scope.

With `--receipt`, the audit verifies installed output hashes and reads runtime entries
beside the package. Without one, only the package itself and the OS baseline are
allowed.

## CI and publication

A pull request to the PDR compares expanded recipes and runs one job per changed
package, version, and platform. Changed dependencies also select their consumers.
Metadata-only changes and unrelated engine updates don't rebuild anything. Each job
runs the same commands you can run by hand:

```sh
pdr=(--pdr https://pdr.rbpkg.com/v3/current.json --pdr-public-key "$PDR_PUBLIC_KEY")
rootbeer-forge --catalog packages package-plan curl@8.22.0 --context "$BUILD_CONTEXT" "${pdr[@]}"
rootbeer-forge --catalog packages build curl@8.22.0 --input-key "$INPUT_KEY" \
  --output result --cache "$CACHE" --cache-context "$BUILD_CONTEXT" \
  "${pdr[@]}" --pdr-root "$PDR_ROOT"
```

`package-plan` emits one task per exact package supported on the current machine,
each with an input key covering its recipe and checks, platform, build backend, and
tool and environment hashes. `build --input-key` stops if this machine's inputs
differ, rather than producing a result under the wrong key. A source task also names the
PDR root it planned against; `--pdr-root` makes the build read that same root, so a
publication in between can't change which dependencies it installs.

PR jobs have no signing credentials. After merge, publication promotes the exact
verified artifacts from the PR without rebuilding. Each package is signed in its own
job, so one failure doesn't block the others:

```sh
rootbeer-forge --catalog packages release --receipt result/receipt.json \
  --input-key "$INPUT_KEY" --registry rootbeer-org/pdr/shfmt --output release \
  --key "$SIGNING_KEY_FILE" --public-key "$PDR_PUBLIC_KEY"
rootbeer-forge push release --public-key "$PDR_PUBLIC_KEY"
```

`release` checks the recipe, archive, installed contents, and runtime audit against
the receipt, and verifies every published dependency the receipt used against the PDR
key, then signs a record. It does not rebuild or execute the package, and the
signature doesn't authenticate the receipt's producer: CI must establish that before
handing it over. `push` needs ORAS authenticated to GHCR; it uploads the release,
checks anonymous downloads, and tags the record `inputs-<key>`. Retry it after upload
failures without re-signing.

Tags are only locators. `verify-record` checks a record's signature and exact inputs
before a later run reuses it. `publish-records` then merges signed records into the
discovery manifest at `https://pdr.rbpkg.com/v3/current.json`; existing versions stay
available while replacements are pending. Users need neither ORAS nor a container
runtime to install.

See [repository hosting and trust](/contributing/package-hosting) for verification and
fallback behavior.

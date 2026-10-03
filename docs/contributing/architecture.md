# Architecture

Rootbeer's native API is built in three layers. Each layer builds on the one
below it, keeping individual pieces small while giving users a high-level,
declarative interface.

## Crate boundaries

| Crate | Owns |
|---|---|
| `rootbeer-store-legacy` | Content hashing, normalized trees, and storage (the current `/opt/rootbeer` store, deleted at cutover) |
| `rootbeer-package` | Package/recipe models, Lua recipe parsing, dependency graphs, resolution, verified downloads and installation |
| `rootbeer-build` | Resolved build plans, backend phases, process execution, and persistent build results |
| `rootbeer-packaging` | Release discovery, qualification, bundling, signing, and publication |
| `rootbeer-core` | Configuration Lua, operation plans, lock adapters, profiles, and apply |
| `rootbeer-cli` | The `rb` configuration and package-consumption CLI |
| `rootbeer-forge` | The package-maintainer CLI |

`rootbeer-core` depends on package and store APIs. It does not depend on the
build or packaging crates. Both applications share the package runtime;
compilation and publishing belong to the maintainer application.

A build plan owns its recipes and resolved binary inputs. Execution never
re-resolves package metadata. Autotools, Zig, Rust, and Custom backend modules produce
phases consumed by one executor. Graph planning, receipt validation, and cache
closure selection share the same dependency model.

Dependency edges distinguish build tools from link libraries. A library’s build
tools stay out of its consumers’ environments; link dependencies propagate
headers and libraries. Explicit runtime edges pin the installed dependency closure;
`link_runtime` contributes both link inputs and runtime installation. Existing
string dependencies retain their combined transitive exports.

Release discovery currently handles GitHub binary assets; source builds still
require explicit versions and source hashes.

Source output qualification statically audits ELF and Mach-O loader references.
Bundled libraries resolve through package-relative search paths, while external
runtime references must resolve within the declared closure or the OS-runtime
baseline. The audit runs before package checks, on cache hits, and when bundling
source receipts.

The store lives at `/opt/rootbeer/store` (`ROOTBEER_ROOT` overrides the root for tests
and CI); per-user state stays in `~/.local/state/rootbeer`. `layout.json` in the root
records the layout version so a newer `rb` migrates an older one.

`/opt/rootbeer` is owned by root and shared by every user without a daemon. Users
insert through `rb-store`, a small setuid-root helper installed at
`/opt/rootbeer/bin/rb-store` by the one-time sudo setup. `rb` streams the tree to it on
stdin (`rootbeer_store_legacy::stream`); the helper never opens a path the caller names,
rejects entries that escape the tree, and files what it wrote under its own hash, so it
needs no trust in the caller. An overridden root, or `rb` running as root, writes
directly. Root-owned entries are trusted without rehashing on use, since only the
helper can create them; entries the caller could have written are verified every time.

The helper has a release (`helper::RELEASE`, bumped on any change) separate from the
stream protocols it reads (`helper::PROTOCOLS`). The sudo setup records what the
installed binary reports (`rb-store version`) in `layout.json`. `rb` reinstalls its own
helper, with one sudo prompt, only when the installed one lacks the protocol `rb` sends
or is below `helper::MINIMUM_RELEASE`; a newer helper is never downgraded, so helpers
keep reading older protocols.

Garbage collection works from roots rather than scanning references. Each user owns
`/opt/rootbeer/var/roots/<uid>/` (created by `rb-store roots`, so no one can claim
another user's directory), with one file per owner (`user`, `configuration`) listing
the store entry names that owner's runtime closure needs. `rb use`, `rb unuse`, and
`rb apply` rewrite their root after activating; any command that touches the store
first writes whichever of the caller's roots are missing from what is installed. `rb gc` (`rb-store gc` for a shared
store) deletes every entry no root lists, under the root's `.lock`. Entries younger
than an hour are kept, which covers an install that has committed an entry but not yet
written its root. Deletion renames the entry first so a partly deleted tree never
looks sealed. Cached `rb run` environments are not roots; they fetch again after
collection.

Runtime dependencies live in separate content-addressed store entries. Receipts
and package locks carry exact recursive runtime facts; realization verifies the
whole closure even when the requested output is cached. Loader-relative sibling
references survive store relocation. Lockfiles expose all retained store paths
for garbage collection roots. The build audit checks declared sibling entries
without consulting the host's library search paths.

The current executor builds for its own host. Cross-compilation remains separate work. Build environment locks pin declared executable
bytes, SDK/toolchain trees, and variables, and verification precedes cache
lookup. Pinned builds use a declared tool path. They still require a host image
identity for OS runtime inputs. Optional isolation uses macOS Seatbelt or Linux
Bubblewrap to deny host networking, restrict reads, and make declared inputs
read-only. Compilation, tests, tool probes, and cache-hit checks share this boundary.
Source and Cargo dependency acquisition happen before offline compilation. Source export caching goes through the same executor.

## Rust packages

Use `build.backend = "rust"` with `build.rust.packages` selecting explicit Cargo
workspace packages. `features`, `no_default_features`, and `environment` configure
features and compile-time application values. `outputs.bins` selects installed
executables. The executor owns release mode, job limits, Cargo directories, and
locked/offline behavior; recipes cannot override those through Rust settings.

The source archive must contain `Cargo.lock`. Cargo vendors its locked dependencies
in a network-enabled acquisition phase, reusing Cargo's download cache. Build and
test phases use the private vendor configuration and `--frozen`. Cargo owns registry
checksum and Git revision verification. Isolation applies after acquisition;
upstream build scripts never run in the acquisition phase.

Host builds resolve the active Rust sysroot before entering the extracted source,
then pin its Cargo/rustc executables and library tree in the build receipt and cache
identity. Repository rustup overrides cannot select a different compiler during the
build. Explicit environment locks must declare `cargo`, `rustc`, and a
`rust-libraries` input pointing to that toolchain's `lib` directory. No rustup proxy
or developer Cargo home is used by build phases. CI selects an exact toolchain.

## Build scheduling and recovery

Catalog export uses the dependency scheduler in `rootbeer-build`; packaging owns
qualification and publication. `--workers` remains the maximum number of package
operations. `--jobs` is the compiler budget shared by source operations. Binary
imports occupy a worker but do not reserve compiler slots. Ready source operations
share the available slots; priority follows the longest remaining dependency chain.
Each operation reports its allocation and elapsed time. Failures block dependants
while independent work can finish and save verified results.

Allocations remain fixed for an operation. This does not resize running compilers,
parallelize the steps of one recipe, or impose a memory limit. Single-package build
plans still walk dependencies in order. Those are explicit limits of this bounded
scheduler, not requirements before adding more packages.

### Existing tools

| Tool | Decision |
| --- | --- |
| [GNU jobserver via the Rust crate](https://docs.rs/jobserver/latest/jobserver/) | Reuse when backends explicitly support cooperative job allocation. Do not inject into arbitrary custom commands. |
| [Make/Ninja](https://ninja-build.org/manual.html#_gnu_jobserver_support) | Continue using upstream build tools for compilation. Ninja's Unix jobserver support needs FIFO transport and no explicit `-j`; existing recipes pass explicit job counts. |
| [sccache](https://github.com/mozilla/sccache) | Candidate for compiler caching across changed source builds. It cannot replace verified package-output caching. No mandatory launcher or remote service in this change. |
| [ccache](https://ccache.dev/manual/latest.html) | Suitable for C/C++ compiler caching, but not a replacement for dependency planning, qualification, or publication. |

Keep the existing content-addressed build results, receipts, and GitHub Actions
cache transport. Do not loosen build-cache identities to improve hit rates without
proving that the omitted inputs cannot change outputs. A compiler-cache rollout
needs measured benefit and explicit toolchain configuration; it does not block
HTTP/3 or subsequent package ports.

HTTP downloads retry transient failures up to five times with exponential backoff
and jitter. `Retry-After` seconds and dates are respected; requests that require a
wait longer than one minute fail rather than retry prematurely. Integrity failures
and permanent HTTP errors are not retried.

Index CI keeps restoration, timed export, and cache saving in separate steps.
The export deadline is 55 minutes inside a 75-minute job, leaving time for setup
and recovery. Cache saving uses `!cancelled()` so failures can retain completed
results while explicit cancellation remains effective. Partial bundles never pass
publication. Workflow configuration changes still require main-branch verification.

## Layer 1 — Rust Primitives

The bottom layer defines the fundamental operations Rootbeer can perform.
These are pure Rust types with no Lua involvement.

**Plan** (`crates/rootbeer-core/src/plan.rs`) defines an `Op` enum
representing every side-effect the system can produce:

```rust
pub enum Op {
    WriteFile    { path: PathBuf, content: String },
    Symlink      { src: PathBuf, dst: PathBuf },
    Exec         { cmd: String, args: Vec<String>, cwd: PathBuf },
    Chmod        { path: PathBuf, mode: u32 },
    SetRemoteUrl { dir: PathBuf, url: String },
}
```

**Executors** (`crates/rootbeer-core/src/executor/`) consume a `Vec<Op>` and
carry it out. `apply.rs` writes to the real filesystem; `dry_run.rs` only
reports what would happen.

Nothing in this layer knows about Lua. Adding a new kind of operation starts
here — add a variant to `Op`, then handle it in each executor.

## Layer 2 — Native Lua Bindings

The middle layer exposes Rust functionality to Lua scripts through
[mlua](https://docs.rs/mlua). Each module registers functions onto the
`rootbeer` global table:

| Module     | Registers                                                                                | Purpose                                                       |
|------------|------------------------------------------------------------------------------------------|---------------------------------------------------------------|
| `fs.rs`    | `rootbeer.file`, `link`, `link_file`, `copy_file`, `path_exists`, `is_file`, `is_dir`, `exec`, `remote` | File writes, symlinks, command exec, path queries             |
| `writer/`  | `rootbeer.json`, `toml`, `yaml`, `plist`, `scripts`                                       | Format codecs and script writers (`encode`/`decode`/`read`/`write`) |
| `sys.rs`   | `rootbeer.host`                                                                          | Runtime system info (OS, arch, hostname, user, home, shell)   |
| `secret.rs`| `rootbeer.secret`                                                                        | Read secrets from external providers (1Password via `op`)     |

Common Lua-context primitives live in `lua/mod.rs` next to `ctx()`:
`slurp`, `defer_write`, and `defer_chmod`. These wrap the boilerplate of
reading the runtime / pushing onto the run log, and are reused by `fs.rs`
and the writer submodules.

Everything is wired together in `vm.rs`, which creates the Lua VM, registers
all modules, and sets up the custom `require` loader:

```rust
let rb = lua.create_table()?;
fs::register(&lua, &rb)?;
writer::register(&lua, &rb)?;
sys::register(&rb)?;
secret::register(&lua, &rb)?;
lua.globals().set("rootbeer", &rb)?;
```

### Codecs

Each format under `lua/writer/` (json, toml, yaml, plist) implements a
`Codec` trait with two functions: `encode(&mlua::Value) -> String` and
`decode(&Lua, &str) -> mlua::Value`. mlua's `serialize` feature gives
`mlua::Value` a `Serialize` impl, so each codec is a thin wrapper around
the format crate's `to_string` / `from_str` — no per-format walker code.
A single `register::<C>(lua, parent)` wires the four-function shape onto
a sub-table on `rb`.

### Plan/Execute Model

I/O functions like `rootbeer.file()` do **not** write to disk immediately.
They push an `Op` onto a shared `Vec<Op>` (the "run log"). The CLI later
drains that log and hands it to an executor. This separation means Lua
scripts are always safe to evaluate — no filesystem changes happen until the
user explicitly applies.

### Type Annotations

Because the Lua language server can't see into Rust, a `@meta` file at
`lua/rootbeer/core.lua` declares type signatures for every native function.
This file is never executed — it exists solely for editor tooling and doc
generation. When you add or change a native binding, update `core.lua` to
match.

## Layer 3 — High-Level Lua Modules

The top layer is pure Lua. Modules like `git.lua` and `zsh.lua` live in
`lua/rootbeer/` and provide opinionated, declarative APIs that consume the
lower layers.

A typical module follows this pattern:

1. Accept a structured config table from the user.
2. Transform it into the format the target tool expects.
3. Call `rootbeer.file()` or a format writer (`rootbeer.json.write()`,
   `rootbeer.toml.write()`, …) to produce output.

For example, `git.lua` takes a `git.Config` table and:

- Builds a gitconfig table from typed fields (`user`, `signing`, `lfs`, …).
- Quotes string values per gitconfig rules and emits the text directly in
  Lua (gitconfig isn't strictly INI, so it's handled here rather than as
  a native codec).
- Writes the result via `rootbeer.file()`, plus an optional
  `.gitignore` alongside it.

Each module is self-contained with its own `@class` annotations for the
language server. Users load them via `require("rootbeer.git")`.

### Authoring Principles

Generator code (Lua → tool config) is naturally verbose, but the verbosity
should be **uniform** across modules — read one, you've read them all.
Follow these conventions:

**1. Iterate user-supplied maps with `rootbeer.tbl.sorted_pairs`.**
Lua's `pairs()` has no defined order, so plain `pairs()` over a user table
produces nondeterministic output: different diffs every run, unstable
tests, and noisy git history. Use the sorted iterator for any map whose
keys are user-controlled (`aliases`, `functions`, `hosts`, `env`, gitconfig
sections, …). Insertion-order iteration over arrays (`ipairs`) is fine.

```lua
local tbl = require("rootbeer.tbl")
for name, body in tbl.sorted_pairs(cfg.functions) do
  ...
end
```

**2. Split multi-line user input with `rootbeer.str.split_lines`.**
`s:gmatch("[^\n]+")` silently drops blank lines, which mangles user
function bodies and templates. `str.split_lines` preserves them. Pair with
`str.indent` when indenting a block — it skips empty lines so generated
output stays diff-friendly.

**3. Keep helpers local until at least three modules need them.**
The shared stdlib (`rootbeer.str`, `rootbeer.tbl`) exists for patterns
that recur across modules. One-off formatters (gitconfig quoting, SSH
`yes`/`no` coercion, path basename) stay as `local` functions in the
module that owns them. Premature extraction creates more cognitive load
than it saves.

**4. Don't reach for a generic builder.**
Each module's "what counts as a section, when to emit a blank separator,
what trailing newline policy" is genuinely different. Direct line-buffer
loops (`lines[#lines + 1] = ...; rb.file(path, table.concat(lines, "\n")
.. "\n")`) are clearer than a Lines class wrapping the same. The
duplication is shallow and reading the next module never requires
learning a new API.

**5. Stay in Lua unless you need Rust.**
Rust earns its place when you need filesystem access, subprocess, perf,
or new `Op` variants. Pure data transformation, string munging, and
table iteration belong in Lua — users can read it, hack it, and the
LSP picks up types directly. If a helper is three lines of Lua, it
doesn't belong in `crates/rootbeer-core/`.

**6. Preserve backwards-compatible input schemas.**
Module config tables are a user-facing contract. Adding optional fields
is fine; renaming or restructuring existing ones breaks every user's
`init.lua`. When the generator's output changes (e.g. switching to
sorted iteration), make sure the *input* schema is unchanged so users
don't need to touch their config.

## Lua Standard Library Loading

The `rootbeer.*` modules in `lua/rootbeer/` can be loaded two ways:

- **Filesystem (debug builds)** — Modules are read from disk via `FsRequirer`,
  using the `ROOTBEER_LUA_DIR` path set at compile time. This means `cargo run`
  picks up Lua changes immediately with no Rust recompile.
- **Embedded (release builds)** — When the `embedded-stdlib` feature is enabled
  (it is by default), release builds bake every module into the binary via
  `include_str!`. The `EmbeddedRequirer` serves them from memory so the binary
  is fully self-contained.

The selection is automatic: `cargo build` (debug) always uses the filesystem,
`cargo build --release` uses embedded. Passing `--lua-dir` to the CLI forces
filesystem loading in either mode.

See [Packaging](./packaging) for distribution-specific build instructions.

## Adding a New Module

1. **Primitives** — If the module needs a new kind of side-effect, add an
   `Op` variant and handle it in the executors.
2. **Bindings** — If the module needs a new native function or serializer,
   add it in `crates/rootbeer-core/src/lua/` and register it in `vm.rs`.
   Update `lua/rootbeer/core.lua` with the type signature.
3. **Lua module** — Create `lua/rootbeer/<name>.lua`. Define `@class` types,
   accept a config table, transform it, and call the lower-level APIs.
   Follow the [authoring principles](#authoring-principles) — prefer
   `rootbeer.tbl.sorted_pairs` over `pairs` for user maps, and
   `rootbeer.str.split_lines` over `gmatch("[^\n]+")` for multi-line bodies.
4. **Tests** — Add `crates/rootbeer-core/src/lua/tests/<name>.rs` and wire
   it into `tests/mod.rs`. Drive your module via `test_support::run` and
   assert on the produced `Vec<Op>` — no filesystem or fixtures needed.
5. **Docs** — Add a page in `docs/modules/<name>.md` with a hand-written
   intro followed by a VitePress `@include` directive that pulls in the
   generated reference from `docs/api/_generated/<name>.md`. Copy the
   footer pattern from any existing module page (e.g.
   [`docs/modules/zsh.md`](https://github.com/rootbeer-org/rootbeer/blob/main/docs/modules/zsh.md))
   verbatim. Register the page in the appropriate sidebar category in
   `.vitepress/nav.ts`. Never hand-edit files under `docs/api/_generated/`.

## Adding a secret provider

The Lua surface follows the same two-shape convention so users get a
predictable API across providers. At the Rust layer, deferred writes
flow through a single `Op::WriteFile { source: WriteSource::<Provider> }`
variant — there is no per-provider write op. To add a provider:

1. Add a `WriteSource` variant in [`plan.rs`](https://github.com/rootbeer-org/rootbeer/blob/main/crates/rootbeer-core/src/plan.rs)
   carrying whatever the provider needs to fetch at apply time (e.g.
   `Rage { ciphertext: PathBuf, identity: PathBuf }`).
2. Extend `resolve_source` in [`apply.rs`](https://github.com/rootbeer-org/rootbeer/blob/main/crates/rootbeer-core/src/executor/apply.rs)
   with the shell-out, and `WriteSource::fetch_label` in `plan.rs` so
   the CLI announces the fetch automatically.
3. Add `rb.secret.<provider>(…)` (sync) and `rb.secret.<provider>_document(…)`
   (deferred) bindings in [`lua/secret.rs`](https://github.com/rootbeer-org/rootbeer/blob/main/crates/rootbeer-core/src/lua/secret.rs),
   plus matching annotations in [`lua/rootbeer/secret.lua`](https://github.com/rootbeer-org/rootbeer/blob/main/lua/rootbeer/secret.lua).

No CLI changes are required — the dry-run / apply output picks up the
new provider through `fetch_label`.

## Internal runtime tools

Providers use the per-pipeline `ToolRuntime` in `crates/rootbeer-core/src/tools.rs`
for packaged executables. Pass a pinned `PackageRequest` and exported command
name to `command()`; the provider owns that declaration, while package recipes
remain in their registry. The returned `Command` uses an absolute store path.

Preparation uses the standalone package resolver, download cache, and verified
realizer without activating a user profile. All exports from a prepared package
are reused for that pipeline, including across Lua evaluation and apply. Failed
preparation can be retried. Package offline mode is inherited from the pipeline;
provider network access and authentication remain the provider's responsibility.
No secret values are cached by this runtime. See `one_password.rs` for a consumer.

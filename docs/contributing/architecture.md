# Architecture

Rootbeer's native API is built in three layers. Each layer builds on the one
below it, keeping individual pieces small while giving users a high-level,
declarative interface.

## Crate boundaries

| Crate | Owns |
|---|---|
| `rootbeer-cli` | The `rb` command, including the hidden `rb drv` maintainer commands |
| `rootbeer-core` | Configuration Lua, operation plans, and apply |
| `rootbeer-drv` | Derivations and their keys. Pure, with no IO |
| `rootbeer-eval` | Lua recipes into derivations and package metadata |
| `rootbeer-sandbox` | Fetching, building, and checking a derivation in a sandbox |
| `rootbeer-cache` | Store outputs to and from an OCI registry |
| `rootbeer-store` | The shared `/opt/rb` store and its output database |
| `rootbeer-helper` | `rb-helper`, the setuid helper that writes to the store |
| `rootbeer-profile` | Profiles, generations, `rootbeer.lock`, and GC roots |
| `rootbeer-trust` | The signed TUF package index |
| `rootbeer-bootstrap` | The frozen protocol for fetching a newer `rb` |

`rootbeer-core` depends only on `rootbeer-profile`. The CLI hands it package
installation (`tools::Install`), so configuration never depends on the store,
trust, cache, eval, or sandbox crates. `rootbeer-bootstrap` depends on no other
Rootbeer crate.

## Packages

A recipe in [pdr](https://github.com/rootbeer-org/pdr) evaluates
(`rootbeer-eval`) to a derivation. Its key is the hash of its encoding, so an
identical key always means an identical output. `rootbeer-sandbox` realizes a
derivation: it fetches and verifies inputs, builds in Seatbelt on macOS or
Bubblewrap on Linux (with no network unless the derivation fetches), runs the
recipe's checks, and scans the output for store references. `rootbeer-cache` publishes outputs to
`ghcr.io/rootbeer-org/store`, keyed by derivation key.

The registry is untrusted storage. The [index](./package-hosting) signs each
package's versions with the key and manifest digest of every output, and
`rootbeer-trust` verifies it through `tough`. Installing pulls an output and its
references by those signed digests. GitHub release requests and local recipe
checkouts (`rb.package_catalog`) bypass the index and are recorded as such in
profiles and locks.

## Store and profiles

The store lives at `/opt/rb`, shared by every user without a daemon:

| Path | Holds |
|---|---|
| `store/` | Outputs, each named by its key |
| `var/db.sqlite` | Which outputs are present and what they reference |
| `var/layout.json` | Layout and helper versions |
| `libexec/rb-helper` | The setuid helper |
| `profiles/<uid>/` | Each user's `default` and `user` profiles |

The first command that needs the store runs `sudo rb-helper setup <group>`
once per machine. It creates the directories, installs the helper, and lets the
group (`admin` on macOS, `sudo` or `wheel` on Linux) add to the store. Members
then write through the helper: `seal` registers an output built in place, `pull`
unpacks an archive from stdin and checks its digest, `profile` creates the
caller's profile directory, and `gc` collects. Root writes directly.

`var/layout.json` records `LAYOUT` and `HELPER_VERSION`. A newer helper makes
`rb` run setup again. A layout newer than this `rb` understands makes it relaunch
into a newer released one through `rootbeer-bootstrap`.

Each profile is a symlink to its current generation, a directory of links into
the store plus a `generation.json` manifest. `rb apply` installs into `default`,
and `rb use` into `user`, which wins on `PATH`. `rb rollback` and
`rb generations` act on `user`.

Garbage collection works from roots. `rb gc` reads every generation of every
user's profiles, plus the keys of running `rb run` commands and of tools `rb`
holds for itself, and removes every output nothing live references. Installs and
builds hold `var/gc.lock` shared, so collection waits for them.
`rb gc --older-than 30d` first drops your older generations, except each
profile's current one.

`rb migrate` moves a user's packages from the old `/opt/rootbeer` store into
`/opt/rb`. Once every user's `var/roots` there is empty,
`sudo /opt/rb/libexec/rb-helper remove-legacy` deletes it.

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

See [Distributing Rootbeer](./distribution) for distribution-specific build instructions.

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

Providers get packaged executables from the per-pipeline `ToolRuntime` in
`crates/rootbeer-core/src/tools.rs`. Pass a package name, exact version, and
command to `command()`. The returned `Command` runs an absolute store path.

The CLI supplies the installer (`tools::Install`). It installs the exact version
from the signed index into `/opt/rb` without touching any profile, and holds it
from GC while `rb` runs. A package is installed once per pipeline and shared
across Lua evaluation and apply. A failed install can be retried. `--offline`
can't install, since it needs the signed index. Provider network access and
authentication remain the provider's responsibility, and no secret values are
cached. See `one_password.rs` for a consumer.

Rootbeer is a rust library and command line tool that executes a user-provided
lua script in a sandboxed environment, creating a system configuration. Think of
it akin to a dotfile manager like home-manager or chezmoi.

- `crates/rootbeer-cli`: The `rb` command line tool, including the hidden
  `rb drv` commands package maintainers and CI use
- `crates/rootbeer-core`: The core library that runs the lua script
- `crates/rootbeer-drv`: Derivations and their keys. Pure: values in, values out,
  no IO, no side effects, no global state.
- `crates/rootbeer-eval`: Evaluates Lua recipes into derivations and package
  metadata
- `crates/rootbeer-sandbox`: Realizes a derivation into the store, building in a
  Seatbelt or Bubblewrap sandbox and scanning outputs for references
- `crates/rootbeer-cache`: Moves store outputs to and from an OCI registry
  (`ghcr.io/rootbeer-org/store`)
- `crates/rootbeer-store`: The shared `/opt/rb` store and its output database
- `crates/rootbeer-helper`: `rb-helper`, the setuid helper at
  `/opt/rb/libexec/rb-helper` that sets up the store and lets trusted users
  write to it
- `crates/rootbeer-profile`: Per-user profiles and generations under
  `/opt/rb/profiles/<uid>`, `rootbeer.lock`, and GC roots
- `crates/rootbeer-trust`: Reads the signed package index from a TUF repository
  (through `tough`), so a client believes only what the index's keys signed
- `crates/rootbeer-bootstrap`: The frozen protocol for fetching a verified rb new
  enough to run a command (first installs, and state written by a newer rb). Changes
  are additive only, and it must never depend on another rootbeer crate.

New crates use Rust edition 2024.

Code comments (including doc comments) wrap at 80 columns and never use
semicolons or colons. Split the thought into separate sentences instead.

Configuration must not depend on the package crates. `rootbeer-core` depends
only on `rootbeer-profile`, and the CLI hands it package installation, so it
never depends on store, trust, cache, eval, or sandbox. Coordinate recipe schema
changes (`rootbeer-eval`) with pdr and its `.github/rb-engine-revision` pin.

Agent skills live in `.agents/skills/` and are model-agnostic. Use
`.agents/skills/adversarial-review` after writing a new crate or module.

Production package recipes belong exclusively in pdr's `packages/`
(`rootbeer-org/pdr`). Rootbeer contains generic package tooling and regression
tests; never embed or duplicate production recipes in this repository.

## Package CI and Publication

- pdr CI plans every declared version against `ghcr.io/rootbeer-org/store` and
  builds only what the store lacks. PRs build and check. Pushes to main build
  again and publish, then `rootbeer-org/index` signs the new entries.
- Rootbeer's deploy workflow releases the `rootbeer` package the index signed
  through the bootstrap channel (`rbpkg.com/bootstrap/v1/<platform>.json`).
- Derivation keys decide reuse. Never loosen a key to improve cache hits unless
  the omitted input provably can't change the output. A new `rootbeer-drv` field
  must be optional and omitted at its default, or every key changes.
- Never bypass integrity or provenance checks to save CI time.

User configuration is provided through a layering system in the core library,
where the base fundamentals (such as symlinking files, creating new files,
running commands, etc.) are provided by the library, callable from the user's
lua script.

Because the lua scripts are meant to run with 0 external dependencies, the core
library also builds up fundamentals such as JSON serialization, string formats,
and more (akin to Nix's stdlib).

The highest API layer is mostly defined in Lua and wraps the lower level APIs
in nice types, functions, and design patterns. For example, a `zsh` module is
defined in the highest layer which consumes the lower level APIs, allowing the
user to follow a nicely typed API to manage their zsh configuration.

This pattern needs to remain consistent across all modules and there are a few
different tools built around these assumptions defined below.

## Require Syntax

Standard Lua dot-separated require paths are used **everywhere** — stdlib
modules, user scripts, docs, and examples:

- `require("rootbeer.git")` — dot syntax, used in **all** code.
- `require("helper")` — resolves from the user's source directory.

A Rust-level require wrapper in `vm.rs` translates dot paths to Luau-native
`@`-prefixed paths before they reach Luau's C++ layer. This is a hidden
implementation detail — all Lua files look like vanilla Lua and work with
lua-language-server without special configuration. Never use `@`-prefixed
paths in `.lua` files.

The LSP setup (`rb lsp` / `rb init`) writes type definitions to
`~/.local/share/rootbeer/typedefs/` and a `.luarc.json` with
`workspace.library` pointing there. A generated `init.lua` lets
`require("rootbeer")` resolve to the `rootbeer` class type. No LuaLS plugin
is needed — standard `workspace.library` resolution handles everything.

- I/O operations run in a plan/execute mode, where calls only append to a log of
  operations that need to be executed on the apply stage.
- High-level Lua modules (zsh, git, ssh, …) MUST iterate user-supplied maps
  with `rootbeer.tbl.sorted_pairs` (not `pairs`) so generated output is
  deterministic across runs. Multi-line user input MUST be split with
  `rootbeer.str.split_lines` (not `gmatch("[^\n]+")`) so blank lines are
  preserved. See `docs/contributing/architecture.md` → "Authoring Principles"
  for the full convention.
- The lua language server is used to automatically generate markdown docs for
  the documentation site defined in the `docs` directory (built with Vitepress).
  The `docs/api/_generated/` directory is auto-generated from `lua/rootbeer/*.lua`
  meta files via `scripts/lua2md.ts`. NEVER hand-edit files in `_generated/` and
  NEVER manually write API field tables in doc pages. Instead, update or create
  the corresponding `lua/rootbeer/*.lua` meta file with `@class`/`@field`
  annotations, and use `<!--@include: ../api/_generated/<name>.md-->` in the doc
  page (also taking care to ensure the path is added to the sidebar if necessary).
- When updating native API functions, the `core.lua` meta file should contain
  the correct type-signatures for the language server to pick up on. Each module
  (core, host, git, zsh, etc.) has its own meta file in `lua/rootbeer/`.

## Documentation Structure

The docs site (`docs/`) follows a strict organizational structure:

- `docs/guide/` — Core principles and getting started. Hand-written prose only.
  Keep pages concise and scannable. Currently: getting-started, core-concepts,
  multi-device (profiles).
- `docs/modules/` — One page per integration module (zsh, git, ssh, brew, etc.).
  Each page should have a brief hand-written intro + examples at the top, with
  `<!--@include: ../api/_generated/<name>.md-->` at the bottom for autogenerated
  API reference.
- `docs/reference/` — Low-level API reference pages (core, host). Same pattern:
  hand-written context + autogenerated reference via include.
- `docs/contributing/` — Developer docs (setup, architecture).
- `docs/api/_generated/` — Autogenerated only. Never hand-edit.

When adding a new module doc page:
1. Create `docs/modules/<name>.md` with concise intro, one or two examples, and
   the `<!--@include: ../api/_generated/<name>.md-->` footer.
2. Add it to the correct category in `.vitepress/config.ts` sidebar under
   "Modules". Categories use `collapsed: true` for scalability. Current
   categories: Shell, Developer Tools, Package Managers. Create new categories
   as needed.
3. Ensure the corresponding `lua/rootbeer/<name>.lua` meta file exists for
   autogeneration.

Documentation principles:
- Be concise. Short paragraphs, let code examples speak.
- Never duplicate API field tables by hand — always use the generated include.
- Guide pages explain concepts; module pages show usage + reference.
- Cross-reference between guide and module pages where relevant.

# Development Setup

## Prerequisites

Rust is pinned in `rust-toolchain.toml`; Node and pnpm are pinned in
`package.json`. Mise reads these files through the project settings. Install
[rustup](https://rustup.rs/) and [mise](https://mise.jdx.dev/), then run:

```bash
rustup show
mise install
```

This installs the development tools:

- **Rust 1.98.1** (with rustfmt, clippy, and rust-analyzer)
- **Node + pnpm** (for the docs site)
- **lua-language-server** (for Lua type checking and doc generation)
- **lefthook** (git hooks)

Alternatively, if you have [Nix](https://nixos.org/) installed, you can use the
provided flake to get a development shell with all dependencies:

```bash
nix develop
```

Or with [direnv](https://direnv.net/), the `.envrc` will automatically load
the shell when you enter the project directory.

## Building

### With Cargo

```bash
# Debug build (binaries land in target/debug/)
cargo build

# Release build
cargo build --release
```

`scripts/bin/rb` is on your `PATH` through the `mise.toml` env config and runs
the working tree with `cargo run`. `rb` expects `rb-helper` beside it to set up
`/opt/rb`, and `cargo build` builds both.

### Prebuilt binaries

Successful main CI pushes the tested `rb` and `rb-helper` binaries to
`ghcr.io/rootbeer-org/rb:<commit>-<target>` as OCI artifacts for Linux x86-64,
Linux ARM64, and macOS ARM64, with GitHub build attestations.
[setup-rb](https://github.com/rootbeer-org/setup-rb) installs them in CI, as pdr
does at its pinned engine commit.

For example, on Linux x86-64 with [ORAS](https://oras.land), set `revision` to
the full commit:

```sh
target=x86_64-unknown-linux-gnu
oras pull "ghcr.io/rootbeer-org/rb:$revision-$target"
for binary in rb rb-helper; do
  gh attestation verify "$binary-$target" --repo rootbeer-org/rootbeer \
    --signer-workflow rootbeer-org/rootbeer/.github/workflows/build.yml \
    --source-ref refs/heads/main --source-digest "$revision" --deny-self-hosted-runners
done
chmod +x "rb-$target" "rb-helper-$target"
```

### With Nix

```bash
# Build the package
nix build

# Run checks (clippy + fmt)
nix flake check
```

The binary is built to `./result/bin/rb`.

## Running Tests

```bash
cargo test --workspace
```

The test fixture lives in `test/` — `rootbeer.lua` is a sample manifest and
`dotfiles/` contains files used for symlink tests.

## Docs Site

The documentation site is built with [VitePress](https://vitepress.dev/).
API reference pages are auto-generated from Lua doc comments via
`scripts/lua2md.ts`.

```bash
# Install dependencies
pnpm install

# Start the dev server (auto-runs lua2md first)
pnpm dev

# Production build
pnpm build
```

### Writing Docs

The north-star pages for documentation style are
[What is Rootbeer?](../guide/what-is-rootbeer) and
[Getting Started](../guide/getting-started). New pages should feel like they
belong next to those: direct, practical, and oriented around what the user is
trying to do.

Guidelines:

- Keep the table of contents useful. Prefer a small number of substantial
  sections over many thin headings.
- Write for the ideal path first. Show the recommended way to use a feature,
  and mention deviations only when they add useful context.
- Be concise. Short paragraphs and focused examples are better than exhaustive
  explanation.
- Let layout carry meaning. Use headings, tips, and code blocks to make pages
  scannable without turning them into API inventories.
- If a section is too thin, either merge it with a nearby section or add enough
  explanation and examples to make it worth scanning to.
- Keep API details in generated references. Guide pages should explain why and
  how to use a feature; the include at the bottom can carry function-level
  detail.

## Git Hooks

Install the pre-commit and pre-push hooks with:

```bash
lefthook install
```

This runs automatically on commit:

| Hook | Action |
|------|--------|
| `pre-commit` | `cargo fmt`, `cargo clippy --fix`, `pnpm oxfmt` |

All fixers auto-stage corrected files so commits are always clean.

## Editor Setup

For the best experience working on Lua modules, configure your editor to use
**lua-language-server** with the `lua/` directory. The `@meta` file at
`lua/rootbeer/core.lua` provides type information for all native bindings,
giving you autocomplete and diagnostics in the high-level modules.

Build isolation tests need `sandbox-exec` on macOS or `/usr/bin/bwrap` on Linux
(`sudo apt-get install bubblewrap` on Debian/Ubuntu). Linux must allow user and
mount namespaces. These tests exercise real filesystem and network denials;
a surrounding sandbox or restricted container can prevent them from starting.

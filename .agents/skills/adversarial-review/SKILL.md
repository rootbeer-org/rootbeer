---
name: adversarial-review
description: Adversarial quality pass over a crate or diff. Removes low-value tests, slop (narrating comments, defensive guards for impossible states), imperative helper patterns, hand-rolled code a dependency already provides, and every way non-test code can panic. Use after writing a new crate or module, or when asked to trim, de-slop, or harden code.
---

# Adversarial review

Assume the code was written quickly and padded. Your job is to make it smaller and
more honest without changing behavior. Edit the code directly; don't just report.

## Process

1. Read the target's stated contract first: the crate doc comment, `AGENTS.md`, and
   any design doc it references. Constraints stated there (purity, no IO, stable
   encodings) override everything below.
2. Read every file in the target, including tests and fixtures.
3. Apply the five passes below. For each finding, either fix it or leave it and say
   why it survives.
4. Verify: run `cargo fmt` (apply it, don't just check), then
   `cargo clippy --all-targets -- -D warnings` and `cargo test` for the crate. If correctness depends on a dependency feature that another
   workspace crate may enable (e.g. `serde_json/preserve_order`), also run the tests
   with `--features <dep>/<feature>`. Golden or pinned values must not change unless the change is the
   point; if one changes, stop and report instead of updating the pin.
5. Report what changed, grouped by pass, plus anything you deliberately kept.

## Pass 1: weak tests

Remove a test when it:

- Restates the implementation (asserts the same expression the code computes).
- Tests the language, std, or a dependency (serde round-trips of derived types,
  `Display` of a string newtype, enum string tables already pinned elsewhere).
- Asserts only `is_ok()`/`is_err()` where the error or value is the actual contract;
  tighten it instead if the contract matters.
- Is subsumed by another test (a golden test already pins the same bytes).
- Exists to raise the count: near-duplicates differing only in input with no new
  boundary.

Keep a test when it pins a contract, a spec vector (RFC test vectors), a boundary,
or a bug that actually happened. Prefer one table-driven test over many similar
functions.

When a spec implementation moves to a dependency, don't test the dependency
directly. Pin its observable result through our public API instead (exact bytes,
golden keys), so a dependency upgrade that changes output fails loudly.

Tests follow the crate's constraints where it's cheap: in a pure crate, load
fixtures with `include_str!` rather than reading files at runtime.

## Pass 2: slop

Remove or rewrite:

- Comments that narrate what the next line does. Keep only why-comments, one or two
  lines. Doc comments belong on public API, and only when the name doesn't already
  say it.
- Defensive guards for states the types already rule out, unreachable error
  variants, `Result` returned from functions that cannot fail.
- Validation repeated in several layers. Validate once, at the boundary where
  untrusted data enters. A check that a dependency setting already enforces may
  stay when the stated contract names it, or when it fails early instead of late;
  say which.
- Long `expect` messages, section-divider comments, placeholder or speculative
  items (unused constants, one-variant enums "for later", `pub` items nothing uses).
  In a new crate with no consumers yet, judge `pub` items against the API its design
  doc plans; anything the doc doesn't call for goes.

## Pass 3: declarative over imperative

Prefer:

- Iterator chains (`all`, `any`, `find`, `map`, `collect`, `try_for_each`) over
  manual loops with mutable accumulators and early-exit flags.
- Serde attributes (`try_from`, `into`, `rename`, `default`, `skip_serializing_if`)
  over hand-written `Serialize`/`Deserialize` impls.
- Data (tables, `const` arrays, match expressions) over branching code.
- Types that make invalid states unrepresentable over runtime checks.

Flag these helper patterns:

- One-call wrappers that only rename a std or dependency function.
- Helpers used once, unless they name a genuinely separate concept.
- Boolean flag parameters that switch behavior; split or use an enum.
- `utils`/`helpers` modules.
- Near-identical predicate functions that differ by a character class; one
  parameterized check or a pattern is clearer.

Keep imperative code where it is clearly simpler and no dependency covers it. If
Pass 4 applies, it wins.

## Pass 4: no NIH

Replace hand-rolled implementations of a spec, format, or algorithm with a
maintained crate: encodings (base32, hex), canonical JSON, hashing, parsing,
globbing, semver. Before adding one:

- Check it is maintained (recent release, real users) and implements the spec
  exactly. Read its docs or source for the edge cases the hand-rolled version
  handles; if the crate gets one wrong, keep ours and note why.
- Prefer a crate already in the workspace's `Cargo.lock`. A new one is still worth
  it when it replaces our implementation of a spec or format, however short ours is:
  spec edge cases are where hand-rolled code goes wrong. It isn't worth it for a
  one-off check of a few lines (a character class doesn't need `regex`).
- A dependency must not weaken a stated constraint (a pure crate must not gain a
  dependency that does IO or global state).
- Check what features it turns on for shared dependencies
  (`cargo tree -e features -i <shared-dep>`). Feature unification applies them
  workspace-wide; report any that change behavior elsewhere.

Std is fine where it's a one-liner. The goal is less code we own, not more crates.

## Pass 5: no panics

Non-test code never panics. Every failure is a value the caller handles. This pass
has no exceptions for "can't happen": if a state truly can't happen, make the types
say so; if it can, handle it.

Remove from non-test code:

- `unwrap()`, `expect()`, `unwrap_err()`, `expect_err()`.
- `panic!`, `unreachable!`, `todo!`, `unimplemented!`, and `assert!`/`assert_eq!`
  used for runtime checks.
- Indexing and slicing that can go out of bounds: `v[i]`, `&s[a..b]`, `map[&key]`.
  String slices at byte offsets also panic off a char boundary.
- Arithmetic that can overflow or divide by zero, and `as` casts that truncate or
  change sign.
- `Mutex::lock().unwrap()`, `RefCell::borrow_mut()` where a borrow can overlap,
  `process::exit` in library code, and `try_into().unwrap()`.

Replace with, in order of preference:

1. **Types that rule the state out**: newtypes validated at construction,
   fixed-size arrays (`[u8; 32]` instead of `&[u8]` plus a length check),
   `first_chunk`/`split_first_chunk` instead of slicing.
2. **Propagation**: `?` with a real error variant, `ok_or`/`ok_or_else`, and
   `let ... else { return Err(...) }`.
3. **Non-panicking accessors**: `get`, `get_mut`, `checked_*`, `try_from`,
   `char_indices`, `split_once`.
4. **A default**, via `unwrap_or`, `unwrap_or_else` or `unwrap_or_default`, *only*
   when the default is genuinely correct behavior. A default that hides a failure
   is worse than the panic it replaced: the bug still happens, but now silently.
   If you can't say why the default is right, propagate the error.

Tests may unwrap: a panic is how a test fails. Still prefer asserting the specific
error (`matches!(result, Err(Error::Digest { .. }))`) when the error is what the test
is about.

Enforce it mechanically, so this pass doesn't depend on review. A crate this pass
has cleaned declares:

```toml
[lints.clippy]
unwrap_used = "deny"
expect_used = "deny"
panic = "deny"
unreachable = "deny"
todo = "deny"
unimplemented = "deny"
indexing_slicing = "deny"
arithmetic_side_effects = "deny"
cast_possible_truncation = "deny"
cast_sign_loss = "deny"
```

and the workspace `clippy.toml` allows them in tests (`allow-unwrap-in-tests`,
`allow-expect-in-tests`, `allow-panic-in-tests`, `allow-indexing-slicing-in-tests`).
`arithmetic_side_effects` has no test option; a test that needs runtime arithmetic
allows it on that test. To confirm enforcement, run clippy with `--all-targets` and
`-D warnings`: an unknown lint or option name fails, and tests that unwrap passing
proves the exemptions apply.

Beyond panics, check the other ways code fails silently or never returns:

- **Hangs.** Every network call has a timeout, covering the whole transfer, not just
  connecting. A frozen value (one shipped in clients that can't be patched) is
  generous enough for slow links.
- **Partial writes.** Anything another process reads appears all at once: write to
  a staging path, `sync_all`, then rename into place.
- **Leaks.** Temporary files and directories are cleaned up even after a crash,
  for example by pruning stale ones on the next run. Code that leaves something
  behind on purpose (a directory an `exec`'d process runs from) is fine when the
  design says so; don't add cleanup machinery for it, just confirm the reason.
- **Ignored errors.** `let _ =` is only acceptable for best-effort work whose
  failure must not block the caller, such as cleanup, and needs a why-comment.

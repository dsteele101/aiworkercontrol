<!-- agentcfg:start -->
<!-- language/rust/tasks/new-unit.md · v1.0.1 -->
# Adding a crate

The exact procedure for adding or modifying a single crate in this Cargo
workspace. Follow every step in order; do not skip or reorder.

The crate to add: $ARGUMENTS

---

## Pre-flight

Before writing any code:

1. **Ask for confirmation.** State which crate you are about to add and what it
   will contain. Wait for explicit approval. Do not start on your own initiative.

2. **Check if the crate already exists:**
   ```bash
   ls crates/<name>/Cargo.toml 2>/dev/null && echo EXISTS || echo MISSING
   ```
   If it exists, report the finding and ask: skip / overwrite / modify.
   Never silently overwrite.

---

## 9-step checklist (one crate)

Complete all nine steps before committing. Never commit a partial crate.

### 1. Scaffold the crate

```bash
cargo new --lib crates/<name>     # use --bin for an executable instead
```

Crate directory naming: `kebab-case` (e.g. `crates/data-store`). The crate's
identifier in code becomes `snake_case` (`use data_store::…`).

### 2. `crates/<name>/Cargo.toml`

Inherit shared metadata from the workspace:

```toml
[package]
name = "<name>"
version.workspace = true
edition.workspace = true
license.workspace = true
repository.workspace = true
rust-version.workspace = true

[dependencies]
# Shared deps come from the workspace:
# serde = { workspace = true }

[lints]
workspace = true
```

The `[lints]` table is **not** optional. Without it the crate silently opts out
of the workspace lints in `Cargo.toml` and `-D warnings` will not catch a
missing doc comment or an `unwrap()` in a non-test path.

`version.workspace = true` is not optional either. `cargo new` writes
`version = "0.1.0"`; replace it. `bump-version.yml` bumps the workspace version
and refuses to tag when crate versions diverge.

### 3. `crates/<name>/src/lib.rs` (or `main.rs`)

- Public API surface only. Implementation lives in submodules.
- Every `pub` item gets a `///` doc comment.
- Crate-level docs go in a `//!` block at the top of the file.

### 4. Module split

Any item > ~150 LOC moves to its own file: `crates/<name>/src/<module>.rs`,
declared with `mod <module>;`. Filenames are `snake_case.rs`.

### 5. Unit tests

Inline at the bottom of the file under test:

```rust
#[cfg(test)]
mod tests {
    // Test code asserts rather than propagating; see Rust code review.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]

    use super::*;

    #[test]
    fn computes_expected_result() {
        assert_eq!(do_thing(2), 4);
    }
}
```

The inner `#![allow(...)]` is what makes the exemption real. All three are set
workspace-wide and fire in test targets too, so without it a `.unwrap()` in a
test fails the clippy gate. The third is needed the moment a test returns
`Result` — an `assert!` inside one is exactly what `panic_in_result_fn` is for,
and every async test in this workspace has that shape.

### 6. Integration tests + doc tests

- Integration tests: `crates/<name>/tests/<feature>.rs`. Each file compiles as
  a separate binary against the crate's public API.
- Doc tests: every public function gets a runnable example in its `///` doc
  comment unless the behavior is trivially obvious from the signature.

### 7. Workspace wiring

The workspace `Cargo.toml` already globs `crates/*`, so new crates are picked
up automatically. If this crate depends on another workspace crate, add it
with a path dependency:

```toml
[dependencies]
other-crate = { path = "../other-crate" }
```

### 8. Verification gate

If `.agents/new-crate.local.md` exists, follow it now, before the gate. It holds
the steps this repository adds to this checklist; it is written by hand, and
`agentcfg` leaves it alone.

All four gates must pass, with zero warnings, before committing:

```bash
just ci
```

### 9. Commit, then hand the PR over

```
feat(<crate>): add <name> crate
```

One crate per PR. Never batch multiple crates. Commit and hand the PR over as
*Git flow* and *PR instructions* say.

---

## Before committing

Points that are easy to get wrong, so verify each one:

- `[lints] workspace = true` in the crate manifest. Without it the crate opts
  out of the workspace lints and the clippy gate cannot catch anything.
- Package keys inherit from the workspace: `edition.workspace = true`, and the
  same for `version`, `license`, `repository` and `rust-version`.
- Every `pub` item has a `///` doc comment, and public functions have a runnable
  doc-test — `missing_docs` is a workspace lint and the gate runs with
  `-D warnings`.
- Every public type is `Debug`. `missing_debug_implementations` is on, and a
  derive will not do for a type holding a `dyn` trait object or an unbounded
  generic — write the impl out.
- The test module carries
  `#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]`.
  Without it an `.unwrap()` in a test fails the clippy gate, and so does an
  `assert!` in a test that returns `Result`.
- `just ci` passes before you commit.
<!-- agentcfg:end -->

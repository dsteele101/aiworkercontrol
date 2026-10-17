<!-- agentcfg:start -->
<!-- language/rust/testing.md · v1.0.1 -->
# Testing instructions

## Before merging any change

```bash
just ci
```

That runs the four gates in order:

- [ ] `just fmt-check` — formatting is clean
- [ ] `just lint` — clippy, warnings as errors
- [ ] `just test` — nextest *(falls back to `cargo test` if not installed)* plus doc-tests
- [ ] `just deny` — licenses + advisories

All four must pass before pushing the branch. The underlying cargo commands live
in the `justfile`; call the recipe rather than copying them.

## What CI adds

`.github/workflows/ci.yml` calls the organization's reusable `rust-ci.yml`,
which runs the same four recipes as separate jobs, so a red build names the gate
that broke. Running `just ci` locally first is still the rule — CI is the
backstop, not the first place you find out.

Two things CI checks that a local run does not:

- **MSRV.** A job pinned to the MSRV (via `RUSTUP_TOOLCHAIN`, which overrides
  `rust-toolchain.toml`) proves the workspace still builds on the `rust-version`
  in `Cargo.toml`. Locally you are on stable, so you would never notice. The
  MSRV is declared in `Cargo.toml`, `clippy.toml` and the `msrv` input in
  `ci.yml`, and this job is what catches a partial bump.
- **Advisories over time.** `.github/workflows/audit.yml` runs weekly, because a
  new advisory lands against dependencies you already have, with no commit to
  trigger a push build.

Coverage is produced as a downloadable HTML artifact on every run. It is not a
gate — nothing fails on a coverage number.

## Test layout

| Test type | Location | When to use |
|-----------|----------|-------------|
| Unit | `#[cfg(test)] mod tests { … }` inline at the bottom of the file under test | Testing private functions or internal logic |
| Integration | `crates/<name>/tests/<feature>.rs` | Testing the crate's public API end-to-end. Each file is compiled as a separate binary. |
| Doc test | `///` doc comment on a public item | Verifying that documented usage examples actually compile and run |
| Property | with `proptest` crate, inside unit or integration tests | Invariant-style tests across a generated input space |
| Benchmark | `crates/<name>/benches/<name>.rs` | Performance regression tracking. Optional. |

## Running specific test types

```bash
cargo test --doc                              # doc-tests only
cargo nextest run -p <crate>                  # one crate
cargo nextest run -p <crate> <test_name>      # one test
cargo test --test <integration_file>          # one integration file
```

## Watching tests during development

```bash
cargo watch -x 'nextest run --workspace'
```
<!-- agentcfg:end -->

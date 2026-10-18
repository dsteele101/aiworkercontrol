# AGENTS.md

AI Worker Control: a Rust app that picks up work items from `work/`, hands each
one to an AI coding agent (the official `claude`, `codex` or `agy` (Antigravity) CLI), and
lands the result on a branch in the project's target repo. A local web
dashboard shows worker progress and holds the settings.

## Layout

| Path | Contents |
|------|----------|
| `crates/` | The Cargo workspace, one directory per crate |
| `work/{open,inprogress,inreview,closed,hold}/<project>/` | Work items, one file each, moved between states with their `.result.md` / `.meta.json` sidecars |
| `work/usage.json` | Each provider's latest reported plan usage (git-ignored) |
| `spec/<project>/` | Per-project spec files handed to the agents |
| `worktrees/` | Per-item git worktrees of target repos (git-ignored) |
| `aiworkercontrol.toml` | Runtime settings, including API keys (git-ignored) |

## Rules

Read these before changing anything; *Git flow* first.

- [Git flow](git-flow.md) — the branch → commit → PR loop
- [Execution order](execution-order.md) — one PR per crate, in dependency order
- [Branch naming](.agents/branch-naming.md)
- [Commit message guidelines](.agents/commit-conventions.md)
- [PR instructions](pr-guidelines.md)
- [Code review](.agents/code-review.md) and [Rust code review](rust-code-review.md)
- [Adding a crate](new-crate.md)
- [Directories and file naming](rust-file-naming.md)
- [Testing instructions](rust-testing.md)
- [Merge gates](gates.md)
- [Releases](tag-only-release.md)

## Build and test commands

```bash
just ci          # the four merge gates, in order
just fmt-check   # cargo fmt --all -- --check
just lint        # clippy, warnings as errors
just test        # nextest (or cargo test) plus doc-tests
just deny        # licenses + advisories
```

The MSRV is declared in `Cargo.toml` (`rust-version`) and `clippy.toml`
(`msrv`); a bump edits both.

Never leave the workspace failing to compile.

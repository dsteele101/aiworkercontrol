<!-- agentcfg:start -->
<!-- core/execution-order.md · v1.0.1 -->
# Execution order

Defines the branch / PR structure for work in this workspace.

---

## Branching and PR strategy

See *Branch naming* for the branch name format.

| Work type | Branch prefix | One PR per |
|-----------|--------------|-----------|
| Foundation scaffold | `chore/` | scaffold step |
| Toolchain / config bump | `chore/` | bump |
| Group of crates | `feat/` | crate — a group is a *sequence* of PRs, not one PR |
| Single isolated crate | `feat/` | crate |
| Rename / refactor | `refactor/` | logical rename unit |
| Docs / rules | `docs/` | change |

**The loop is defined in *Git flow*** — branch from `main`, commit, open
the PR, wait for the merge, repeat. No stacked PRs, and every PR must leave
`main` green on its own.

---

## Within each group

- Build **one crate at a time**, each on its own branch and its own PR.
- Follow the 9-step checklist in *Adding a crate* for each.
- Wait for the crate's PR to be merged before cutting the branch for the next.
- Order the crates so each one compiles against what is already on `main`. A
  crate that needs a not-yet-merged sibling belongs later in the sequence.
- Existing crates in scope get an **audit-pass** (lint + tests + a read-through);
  only commit if a real defect is found.

## Audit-pass checklist (existing crates)

1. Read the crate — check for outdated deps, missing doc comments on the
   public API, and the error-handling shortcuts *Code review* bans in non-test
   paths.
2. Run `cargo clippy -p <crate> --all-targets -- -D warnings` and `cargo nextest run -p <crate>`.
3. Surface anything broken. Only commit if a fix is needed — and give the fix its
   own branch and PR rather than folding it into unrelated work.
<!-- agentcfg:end -->

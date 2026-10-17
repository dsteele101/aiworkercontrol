<!-- agentcfg:start -->
<!-- core/git-flow.md · v1.0.1 -->
# Git flow

The branch → commit → PR loop for **every** change in this repository. There is
one flow; it applies to tooling, docs, config and crates alike. Read it
before the other rules — everything else fits inside it.

The defining constraint: **no stacked PRs.** Each branch is cut from an
up-to-date `main` and is merged before the next one is cut. Only one branch is
ever in flight.

---

## The loop

```bash
# 1. Start from an up-to-date main
git switch main
git pull --ff-only

# 2. Cut the branch (format, or an assigned one: see Branch naming)
git switch -c <type>/<short-description>

# 3. Make the change, then run the gates
just ci                              # all four gates; see Testing instructions

# 4. Commit (format: see Commit message guidelines)
git add <the files this change touches>
git commit

# 5. Push
git push -u origin <type>/<short-description>
```

**6. Open the PR.**

Open it with the GitHub tools you have, with the title and description from
*PR instructions*. Without them, output the title and description instead, and
the user opens it.

**7. The user reviews and merges the PR.** Never merge it yourself.

**8. On the user's go-ahead, return to step 1** for the next change.

---

## While the PR is open

- Answer a review comment with a new commit on the same branch, once the gates
  pass again. Commits inside a PR are review steps: the squash merge folds them
  into one commit on `main`.
- Merge `main` into the branch only when something requires it: a conflict, or
  a required check that wants the branch up to date. A rebase would rewrite
  commits the reviewer has already read, and need a force-push.

## Hard rules

| Rule | Why |
|------|-----|
| Cut every branch from `main` | A branch cut from another branch is a stacked PR |
| Never start change N+1 before N is merged | Same reason; only one branch in flight |
| Never push to `main` | `main` only advances through merged PRs |
| Never merge a PR, `gh pr merge` included | Merging is the user's call |
| Never rebase or force-push a branch under review | It rewrites what the reviewer has read |

A push to `main` is not only a process violation here. `bump-version.yml`
triggers on it, reads the commit subject, and pushes a release tag — so a
hand-pushed commit silently cuts a release.

The same mechanism is why every PR is squash-merged:

- **A merge commit cuts no release.** Its subject — `Merge pull request #12
  from …` — matches no commit type, so no release is cut while the workflow
  still reports success.
- **A rebase merge cuts the wrong one.** Only the subject of the newest commit
  on `main` is read. A rebase merge of `feat: X` then `fix: Y` cuts a patch
  release, and the feature ships under a version that says nothing was added.
- **A squash merge lands one commit, with the PR title as its subject.** The
  title picks the release, and the commits inside the PR never reach `main`.

This rests on two repository settings: squash is the only merge method allowed,
and a squash commit takes the PR title as its subject.

## Verify the merge before continuing

Read the merge from the PR, or from `main`'s log, never from whether the branch
still exists: branches are deleted by hand, and not always. A squash merge
rewrites the commit, so the local branch will not be an ancestor of `main` —
check the subject line, not the hash. Look past the newest commit: the squash
appends ` (#N)` to the subject, and `bump-version.yml` may push a
`chore: release` commit on top of it.

```bash
git fetch origin --prune
git log origin/main -5 --format='%h %s'
```

Then delete the stale local branch (`git branch -D <branch>`) and start the next
one. Do not assume a merge; if neither the PR nor `main` shows one, ask.

A branch the environment assigned, which the PRs take turns on, is restarted
from `main` after each merge instead, so the name repeats and the history
doesn't: `git switch -C <branch> origin/main`. Its old commits are already on
`main`, in the squash, so pushing the restarted branch, which takes
`--force-with-lease`, rewrites nothing under review.

`main` moving is not the same as the change having landed. A merge here starts
work the pull request never ran — the version bump, the tag, and whatever
watches for that tag — under different triggers and a different token, so a
green pull request says nothing about any of it. Where the merge triggered
something, check that it produced the right thing rather than that it went
green: a release with no assets, or notes describing the wrong range, is a
success as far as the workflow is concerned. What a merge sets off varies with
this repository's deployment, and its rules say which; where it sets off
nothing, there is nothing to check.

## Splitting work

Work too large for one PR is split into a **sequence** of PRs, not a stack.
Each does one thing, passes the gates on its own, and is merged before the next
begins. Order them so every PR leaves `main` green — a PR that needs a later PR
to build is in the wrong position.
<!-- agentcfg:end -->

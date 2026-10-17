<!-- agentcfg:start -->
<!-- core/pr-guidelines.md · v1.0.1 -->
# PR instructions

## Title

Follow the same format as commit messages: `<type>(<scope>): <description>`.  
Keep it under 72 characters. The PR is squash-merged, so its title becomes the
subject of the one commit that lands on `main`, which picks the release — see
*Commit message guidelines*. If review changes what the PR does, change the
title to match.

## Description template

```markdown
## What
<!-- One-paragraph summary of the change -->

## Why
<!-- Motivation: bug, feature request, refactor reason -->

## How
<!-- Non-obvious implementation decisions -->

## Testing
<!-- How was this manually verified? Screenshots for UI changes. -->
```

## Rules

- One logical change per PR; split unrelated work into separate PRs
- Branch from an up-to-date `main`, so no rebase is needed before review
- `just ci` must pass before the branch is pushed — all four gates, zero warnings
- Link to the relevant section of AGENTS.md or a rule file if the PR establishes a new pattern

## Size guidance

| Lines changed | Action |
|--------------|--------|
| < 200 | Normal review |
| 200 – 600 | Add context in the description about where to start reading |
| > 600 | Consider splitting — or at minimum call it out and justify it |

## Who opens the PR

Once the gates pass and the branch is pushed, Claude opens the PR with the
GitHub tools it has. Without them, it outputs the title and description, and
the user opens the PR. Either way, the user merges it; Claude never does. See
*Git flow* for the full loop.

Because a branch is only pushed once the gates already pass, there is no
work-in-progress state to represent, so a PR opens ready for review. The one
exception is a finished PR that waits on a check the gates can't make, such as
a visual approval or a decision still pending: open it as a draft, and mark it
ready once the check is done.
<!-- agentcfg:end -->

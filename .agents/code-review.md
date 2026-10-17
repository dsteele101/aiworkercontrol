<!-- agentcfg:start -->
<!-- core/code-review.md · v1.0.1 -->
# Code review

The checks every change gets, whatever the language. The language's own review
rules — error handling, unsafe code, public API docs — load for the same trigger
and are read alongside these.

## What to check

### Gates
- `just ci` passes — all four gates, zero warnings.
- No `#[allow(...)]` attribute added without a justifying comment.

### Dependencies
- New dependencies have a one-line justification in the PR description.
- The MSRV is not raised unless the change explicitly intends to.

### Tests
- New behavior is covered by at least one test.
<!-- agentcfg:end -->

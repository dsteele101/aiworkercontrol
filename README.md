# AI Worker Control

AI Worker Control is a Rust app that turns a folder of task files into pull
requests. It picks up work items from `work/`, hands each one to an AI coding
agent (the official `claude`, `codex`, `agy` or `vibe` CLI), and lands the result on a
branch in your project's git repository, optionally pushing it and opening a
GitHub pull request. A local web dashboard shows what every worker is doing and
holds the settings.

## How it works

1. You drop a plain-text file describing a task into `work/open/<project>/`.
2. When you press **Start** on the dashboard, each worker claims an open item
   (an atomic file move into `work/inprogress/`), creates a git worktree for it
   on a branch named `ai/<item-name>`, and runs the agent there with a prompt
   made of the task plus the project's spec files.
3. The agent implements the task and commits to the branch.
4. If the project has pull requests configured, the branch is pushed and a PR is
   opened through `gh`, using a title and description the agent wrote. The item
   moves to `work/inreview/`.
5. Every few minutes the app checks the PR: merged PRs close the item, closed
   PRs put it on hold, and requested changes or failing CI queue a rework for
   the next free worker. Worktrees of merged items are cleaned up automatically.

Items move between state directories as files:

| Directory | Meaning |
|-----------|---------|
| `work/open/<project>/` | Waiting for a worker |
| `work/inprogress/<project>/` | Being worked on. Items left here when the app stops return to `open` on the next start |
| `work/inreview/<project>/` | Pushed, pull request awaiting review |
| `work/closed/<project>/` | Done |
| `work/hold/<project>/` | Set aside for manual review, for example after using all of its turns. Returned to an agent only by hand |

Beside each item, `<name>.result.md` holds the agent's summary and
`<name>.meta.json` tracks its turns, provider, PR and hold reason. They move
with the item.

## Features

- **Several providers:** Anthropic (Claude), Gemini (Antigravity), OpenAI,
  Mistral (Vibe), Deepseek, OpenRouter and any OpenAI-compatible endpoint. Anthropic,
  OpenAI, Gemini and Mistral can run on your plan login instead of an API key.
  Mistral's model comes from Vibe's own config, and its usage limits are not
  tracked.
- **Load balancing across providers:** priority failover, round robin, weighted
  or pace-based, so work is spread by share or by remaining plan allowance.
- **Plan usage limits:** warning levels, and a stop level at which the app fails
  over to the next provider or pauses. You can override a stop from the dashboard.
- **Live dashboard:** per-worker activity with timers, results, and usage.
- **Safety:** agents that hang are stopped along with their whole process tree,
  and the server listens on loopback only.

## Requirements

- Linux or macOS (the app uses Unix process and file-permission APIs).
- A Rust toolchain. [`rust-toolchain.toml`](rust-toolchain.toml) selects stable
  and the minimum supported version is `rust-version` in [`Cargo.toml`](Cargo.toml).
- `git`.
- At least one agent CLI on your `PATH`, signed in or with an API key set in
  the app: [`claude`](https://docs.claude.com/en/docs/claude-code) (also used
  for Deepseek and OpenRouter), `codex` (OpenAI and OpenAI-compatible), `agy` (Gemini), or
  [`vibe`](https://github.com/mistralai/mistral-vibe) (Mistral).
  Setting `cli_dir` in the settings file looks them up elsewhere.
- [`gh`](https://cli.github.com/), signed in, if you want pull requests opened.
- Optional, for development: [`just`](https://github.com/casey/just),
  `cargo-nextest` and `cargo-deny`.

## Getting started

```bash
git clone https://github.com/dsteele101/aiworkercontrol.git
cd aiworkercontrol
cargo run --release -p server
```

Then open <http://127.0.0.1:8787/>.

The working directory must contain a `work/` directory (the repository ships
the layout). Without an `aiworkercontrol.toml` the app starts with defaults.

### Command-line options

```text
aiworkercontrol [--root DIR] [--port PORT]
```

| Option | Default | Meaning |
|--------|---------|---------|
| `--root DIR` | current directory | Directory holding `work/`, `spec/` and `aiworkercontrol.toml` |
| `--port PORT` | `8787` | Port to serve the dashboard on (loopback only) |
| `-h`, `--help` | | Print usage |

Pass options after `--` when using cargo:

```bash
cargo run --release -p server -- --port 9000
```

### First run

1. Open **Settings**, choose the active provider, and either sign in with the
   plan login (the app can start the CLI's login flow) or paste an API key.
2. Add a project: a name, a spec directory, and the path of the git repository
   the work should land in (see below).
3. Create a work item, for example:

   ```bash
   mkdir -p work/open/myproject
   echo "Add a --verbose flag to the CLI and document it." > work/open/myproject/verbose-flag.txt
   ```

4. Press **Start** on the dashboard. The item's branch will be `ai/verbose-flag`.

Work item file names must be unique per project because the branch, worktree
and sidecar files are named after the file stem.

## Configuration

Settings are edited in the dashboard and saved to `aiworkercontrol.toml` in the
root directory (created with owner-only permissions because it holds API keys,
and git-ignored). You can also edit it by hand while the app is stopped.

```toml
workers = 2                  # parallel workers
active = "anthropic"         # anthropic | gemini | open_ai | deepseek | open_router | open_ai_compatible | mistral
max_turns = 3                # agent runs an item may use before going on hold
fallbacks = ["gemini"]       # providers to fail over to, or to share work with
# cli_dir = "/opt/agents/bin"  # where claude/codex/agy/vibe/gh live; PATH when unset

[[providers]]
provider = "anthropic"
use_api_key = false          # false = use the CLI's plan login
api_key = ""
model = ""                   # CLI default when empty
effort = "medium"            # low | medium | high | xhigh | max (codex: minimal..xhigh)
base_url = ""                # Deepseek, OpenRouter and OpenAI-compatible endpoints only

[[projects]]
name = "myproject"           # matches work/<state>/myproject/
spec_dir = "spec/myproject"  # spec files listed in the agent's prompt
repo = "/home/me/dev/myproject"
base_ref = "main"            # branches are cut from here
# setup = "cp $AIWC_REPO/.env $AIWC_WORKTREE/"   # run in each new worktree first

[projects.pr]                # omit to commit locally without pushing
push_remote = "origin"       # remote the branch is pushed to
repo = "owner/name"          # GitHub repo the PR is opened against
base = "main"
draft = false

[limits]
warn = [50, 75]              # percent of plan window at which the dashboard warns
stop = 85                    # percent at which no new runs start
failover = true              # move to the next fallback instead of pausing

[balancing]
scheme = "priority"          # priority | round_robin | weighted | pace
rework_any_provider = true   # false: reworks wait for the provider that opened the PR

[balancing.weights]          # used by the weighted scheme
anthropic = 20
gemini = 80
```

Relative `spec_dir` and `repo` paths are resolved against the root. Worktrees
for a project are created in `<repo>-worktrees/` beside its repository, along
with agent logs and a per-worker build cache.

## Project layout

| Path | Contents |
|------|----------|
| `crates/agent-backend` | Providers, CLI invocations, auth and agent event/usage parsing |
| `crates/work-queue` | The file-based work queue and its state transitions |
| `crates/worker-pool` | Workers, git worktrees, PR publishing and review watching, usage limits and balancing |
| `crates/server` | The `aiworkercontrol` binary: dashboard, settings and routes |
| `work/` | Work items (runtime state, git-ignored) |
| `spec/<project>/` | Per-project spec files handed to the agents |

## Development

```bash
just ci          # the four merge gates, in order
just fmt-check   # cargo fmt --all -- --check
just lint        # clippy, warnings as errors
just test        # nextest (or cargo test) plus doc-tests
just deny        # licenses + advisories
```

Without `just`, use the aliases in `.cargo/config.toml` (`cargo lint`,
`cargo fmt-check`) and `cargo test --workspace`.

Contribution rules live in [AGENTS.md](AGENTS.md): start with
[git-flow.md](git-flow.md), then [commit conventions](.agents/commit-conventions.md)
and [PR guidelines](pr-guidelines.md).

## Security notes

The app runs agents with shell access and stores API keys, so it binds to
`127.0.0.1` only and rejects requests with an unexpected `Host` header or from
other sites. Do not expose it through a proxy or tunnel, and only point it at
repositories whose work you are willing to let an agent change.

## License

Licensed under either of MIT or Apache-2.0, at your option.

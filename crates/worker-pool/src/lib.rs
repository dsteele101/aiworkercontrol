//! A resizable pool of workers that take items off the [`work_queue`] and have
//! an agent carry each one out in its project's repository.
//!
//! For each item a worker:
//!
//! 1. claims it (`open` → `inprogress`);
//! 2. checks out a fresh git worktree on branch `ai/<item>`, beside the
//!    project's repo in `<repo>-worktrees/`, so the user's own checkout and
//!    the other workers are never touched;
//! 3. runs the configured provider's CLI there with a prompt built from the
//!    item and the project's spec files;
//! 4. closes the item with a result file if the agent succeeded and left at
//!    least one commit, and otherwise returns it to `open` to be tried again.
//!
//! Every agent run on an item is a turn, counted in the item's metadata
//! ([`ItemMeta`]). An item that uses up [`PoolConfig::max_turns`] goes to
//! `hold` for manual review, and leaves only through [`Supervisor::resume`].
//!
//! Once a closed item's branch is merged, [`Supervisor::clean_up_merged`]
//! removes its worktree and branch to free the space.

mod agent;
mod error;
mod git;
mod github;
mod meta;
mod pool;
mod project;
mod prompt;
mod status;

pub use error::PoolError;
pub use meta::ItemMeta;
pub use pool::{BranchOutcome, Cleanup, PoolConfig, Supervisor};
pub use project::{Project, PullRequests};
pub use status::{ActiveJob, Activity, PoolSnapshot, WorkerStatus};

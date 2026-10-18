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
//! Each provider's plan usage, as its CLI reports it, is checked against
//! [`UsageLimits`] before every run: at the stop level a worker fails over
//! to the next of [`PoolConfig::fallbacks`], or waits, until the window
//! resets or the user calls [`Supervisor::override_stop`].
//!
//! Once a closed item's branch is merged, [`Supervisor::clean_up_merged`]
//! removes its worktree and branch to free the space.

mod agent;
mod balance;
mod error;
mod git;
mod github;
mod limits;
mod meta;
mod pool;
mod project;
mod prompt;
mod status;
mod usage;

pub use balance::{Balancing, Scheme};
pub use error::PoolError;
pub use limits::UsageLimits;
pub use meta::ItemMeta;
pub use pool::{BranchOutcome, Cleanup, DEFAULT_STALL_AFTER, PoolConfig, ReviewChange, Supervisor};
pub use project::{Project, PullRequests};
pub use status::{ActiveJob, Activity, PoolSnapshot, WorkerStatus};
pub use usage::ProviderUsage;

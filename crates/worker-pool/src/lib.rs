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
//! Every run is also recorded in the work history (`history.jsonl`): who
//! ran it, with which model and effort, how long it took, what it used and
//! how it ended. [`Supervisor::history`] summarizes it per item.
//!
//! Runs that fail for a reason within the agent's control are evaluated by
//! an agent, which may suggest a lesson for the project's notes file
//! ([`NOTES_FILE`]); [`Supervisor::suggestions`]
//! lists them for the user to accept, reject or comment on.
//!
//! Once a closed item's branch is merged, [`Supervisor::clean_up_merged`]
//! removes its worktree and branch to free the space.

mod agent;
mod balance;
mod error;
mod git;
mod github;
mod history;
mod limits;
mod meta;
mod pool;
mod project;
mod prompt;
mod status;
mod suggestions;
mod usage;

pub use balance::{Balancing, Scheme};
pub use error::PoolError;
pub use history::{
    Entry, EventKind, ItemEvent, ItemSummary, QuotaSpend, TurnOutcome, TurnReason, TurnRecord,
    summarize,
};
pub use limits::{ProviderLimits, Span, StopHit, UsageLimits};
pub use meta::ItemMeta;
pub use pool::{
    BranchOutcome, Cleanup, DEFAULT_STALL_AFTER, DEFAULT_SUGGESTION_RETENTION, PoolConfig,
    ReviewChange, Supervisor,
};
pub use project::{DEFAULT_BUILD_JOBS, Project, PullRequests};
pub use status::{ActiveJob, Activity, PoolSnapshot, WorkerStatus};
pub use suggestions::{
    Author, NOTES_FILE, NotesVersion, ProjectSuggestions, Remark, Suggestion, SuggestionStatus,
};
pub use usage::ProviderUsage;
pub use work_queue::ClaimOrder;

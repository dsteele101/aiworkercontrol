//! The pool's error type.

use std::path::PathBuf;

/// Why an item could not be carried out, or a pool operation failed.
#[derive(Debug, thiserror::Error)]
pub enum PoolError {
    /// A work-queue operation failed.
    #[error(transparent)]
    Queue(#[from] work_queue::QueueError),
    /// The provider settings cannot be run.
    #[error(transparent)]
    Config(#[from] agent_backend::ConfigError),
    /// A git command failed.
    #[error("git {args} failed: {stderr}")]
    Git {
        /// The git arguments.
        args: String,
        /// What git printed on stderr.
        stderr: String,
    },
    /// A `gh` (GitHub CLI) command failed.
    #[error("gh {args} failed: {stderr}")]
    GitHub {
        /// The gh arguments.
        args: String,
        /// What gh printed on stderr.
        stderr: String,
    },
    /// The push remote's URL does not name a GitHub owner.
    #[error("remote {remote} ({url}) is not a GitHub repository")]
    NotGitHub {
        /// The remote.
        remote: String,
        /// Its URL.
        url: String,
    },
    /// The agent CLI could not be started.
    #[error("could not start {program} (is it installed and on PATH?): {source}")]
    Spawn {
        /// The executable.
        program: String,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
    /// Another filesystem or process I/O operation failed.
    #[error("{op} {}: {source}", path.display())]
    Io {
        /// What was being done.
        op: &'static str,
        /// The path involved.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
    /// The project's setup command failed.
    #[error("setup command failed ({status}): {output}")]
    Setup {
        /// How it exited.
        status: String,
        /// The end of what it printed.
        output: String,
    },
    /// The item's project is not configured.
    #[error("no project named {0} is configured")]
    UnknownProject(String),
    /// Only closed items can be cleaned up; others may still be in use.
    #[error("{0} is not closed")]
    NotClosed(work_queue::WorkItem),
    /// An item's metadata file is not valid.
    #[error("{}: {source}", path.display())]
    BadMeta {
        /// The metadata file.
        path: std::path::PathBuf,
        /// The parse error.
        #[source]
        source: serde_json::Error,
    },
    /// Only items on hold can be returned to an agent.
    #[error("{0} is not on hold")]
    NotOnHold(work_queue::WorkItem),
    /// Only items in review have a pull request to check or bump.
    #[error("{0} is not in review")]
    NotInReview(work_queue::WorkItem),
    /// The item has no pull request, or its project opens none.
    #[error("{0} has no pull request")]
    NoPullRequest(work_queue::WorkItem),
    /// Feedback is waiting for a worker; a new commit would go under it.
    #[error("{0} has feedback waiting for a worker")]
    ReworkQueued(work_queue::WorkItem),
    /// The link is not a GitHub pull request.
    #[error("{0:?} is not a GitHub pull request link (https://github.com/OWNER/REPO/pull/N)")]
    NotPullRequestLink(String),
    /// No project opens pull requests against the link's repository.
    #[error("no project opens pull requests against {0}")]
    NoProjectForRepo(String),
    /// Only a pull request that is still open can be imported.
    #[error("{0} is not open")]
    PullRequestNotOpen(String),
    /// The pull request's branch is not on the project's push remote, so a
    /// rework could not push to it.
    #[error("{url} comes from {head}, not from the push remote {remote}")]
    ForeignBranch {
        /// The pull request.
        url: String,
        /// Where its branch lives, as `owner:branch`.
        head: String,
        /// The project's push remote.
        remote: String,
    },
    /// The pull request is already in the queue.
    #[error("{url} is already in the queue as {item}")]
    AlreadyImported {
        /// The pull request.
        url: String,
        /// The item that has it.
        item: work_queue::WorkItem,
    },
    /// The agent reported failure or exited unsuccessfully.
    #[error("agent failed: {0}")]
    AgentFailed(String),
    /// The run was stopped because its provider went past the stop level.
    #[error("stopped: {0} went past its plan-usage stop level")]
    ProviderLimit(&'static str),
    /// The agent finished without committing anything on its branch.
    #[error("agent finished without committing to {0}")]
    NoCommits(String),
    /// The project has no suggestion with this number.
    #[error("there is no suggestion {0}")]
    NoSuggestion(u32),
    /// The suggestion is not in a state the action applies to.
    #[error("suggestion {0} cannot be changed that way now")]
    SuggestionState(u32),
    /// The project's notes file has no version with this number.
    #[error("the notes file has no version {0}")]
    NoNotesVersion(u32),
    /// The evaluating agent's answer was not the JSON it was asked for.
    #[error("the agent's answer could not be read: {0}")]
    BadAnswer(String),
    /// A provider's usage report could not be read.
    #[error("could not read {provider}'s plan usage: {reason}")]
    UsageReport {
        /// The provider's label.
        provider: &'static str,
        /// What went wrong.
        reason: String,
    },
}

impl PoolError {
    pub(crate) fn io(
        op: &'static str,
        path: impl Into<PathBuf>,
    ) -> impl FnOnce(std::io::Error) -> Self {
        let path = path.into();
        move |source| Self::Io { op, path, source }
    }
}

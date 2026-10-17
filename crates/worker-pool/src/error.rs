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
    /// The agent reported failure or exited unsuccessfully.
    #[error("agent failed: {0}")]
    AgentFailed(String),
    /// The agent finished without committing anything on its branch.
    #[error("agent finished without committing to {0}")]
    NoCommits(String),
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

//! The queue's error type.

use std::path::PathBuf;

/// An error from a queue operation.
#[derive(Debug, thiserror::Error)]
pub enum QueueError {
    /// A filesystem operation on `path` failed.
    #[error("{op} {}: {source}", path.display())]
    Io {
        /// What was being done, e.g. "read" or "move".
        op: &'static str,
        /// The path the operation was applied to.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
}

impl QueueError {
    pub(crate) fn io(
        op: &'static str,
        path: impl Into<PathBuf>,
    ) -> impl FnOnce(std::io::Error) -> Self {
        let path = path.into();
        move |source| Self::Io { op, path, source }
    }
}

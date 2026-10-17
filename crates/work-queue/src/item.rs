//! Work items and the states they move through.

use std::fmt;
use std::path::Path;

/// The state of a work item, which is also the directory it lives in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum State {
    /// Available to be picked up by a worker.
    Open,
    /// Picked up by a worker.
    InProgress,
    /// Pushed, with a pull request open for review.
    InReview,
    /// Completed.
    Closed,
    /// Set aside for manual review; returned to an agent only by hand.
    Hold,
}

impl State {
    /// Every state, in lifecycle order.
    pub const ALL: [State; 5] = [
        State::Open,
        State::InProgress,
        State::InReview,
        State::Closed,
        State::Hold,
    ];

    /// The directory name for this state under the queue root.
    ///
    /// ```
    /// assert_eq!(work_queue::State::InProgress.dir_name(), "inprogress");
    /// ```
    pub fn dir_name(self) -> &'static str {
        match self {
            State::Open => "open",
            State::InProgress => "inprogress",
            State::InReview => "inreview",
            State::Closed => "closed",
            State::Hold => "hold",
        }
    }
}

/// One work item: a file inside a project directory.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WorkItem {
    /// The project directory the item sits in, e.g. `phase`.
    pub project: String,
    /// The item's file name, e.g. `timesifter.txt`.
    pub file: String,
}

impl WorkItem {
    /// Creates an item from its project and file name.
    pub fn new(project: impl Into<String>, file: impl Into<String>) -> Self {
        Self {
            project: project.into(),
            file: file.into(),
        }
    }

    /// The file name without its extension, used for branch and directory names.
    ///
    /// ```
    /// let item = work_queue::WorkItem::new("phase", "timesifter.txt");
    /// assert_eq!(item.stem(), "timesifter");
    /// ```
    pub fn stem(&self) -> &str {
        Path::new(&self.file)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(&self.file)
    }

    /// The item's path relative to a state directory: `<project>/<file>`.
    pub fn relative_path(&self) -> String {
        format!("{}/{}", self.project, self.file)
    }
}

impl fmt::Display for WorkItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.project, self.file)
    }
}

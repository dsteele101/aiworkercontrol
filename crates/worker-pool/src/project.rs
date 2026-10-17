//! A project: where its specs live and which repository its work lands in.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use work_queue::WorkItem;

/// A project whose work items sit in `work/<state>/<name>/`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    /// The project's directory name under each work state, e.g. `phase`.
    pub name: String,
    /// The directory holding the project's spec files.
    pub spec_dir: PathBuf,
    /// The git repository the work is done in.
    pub repo: PathBuf,
    /// The ref each item's branch is cut from.
    #[serde(default = "default_base_ref")]
    pub base_ref: String,
}

fn default_base_ref() -> String {
    "main".to_string()
}

impl Project {
    /// A project whose branches are cut from `main`.
    pub fn new(
        name: impl Into<String>,
        spec_dir: impl Into<PathBuf>,
        repo: impl Into<PathBuf>,
    ) -> Self {
        Self {
            name: name.into(),
            spec_dir: spec_dir.into(),
            repo: repo.into(),
            base_ref: default_base_ref(),
        }
    }

    /// Where this project's worktrees go: a `<repo>-worktrees` directory
    /// beside the repo, outside any other project's instruction files.
    ///
    /// ```
    /// use std::path::Path;
    /// use worker_pool::Project;
    ///
    /// let project = Project::new("phase", "/specs/phase", "/dev/phase");
    /// assert_eq!(project.worktree_root(), Path::new("/dev/phase-worktrees"));
    /// ```
    pub fn worktree_root(&self) -> PathBuf {
        let name = self
            .repo
            .file_name()
            .map_or_else(|| "repo".into(), |n| n.to_string_lossy());
        let parent = self.repo.parent().unwrap_or(&self.repo);
        parent.join(format!("{name}-worktrees"))
    }

    /// The worktree an item is carried out in.
    pub fn worktree_path(&self, item: &WorkItem) -> PathBuf {
        self.worktree_root().join(item.stem())
    }

    /// The agent's log for an item, kept beside (not inside) its worktree.
    pub fn log_path(&self, item: &WorkItem) -> PathBuf {
        self.worktree_root().join(format!("{}.log", item.stem()))
    }

    /// The branch an item's work is committed on.
    ///
    /// ```
    /// use work_queue::WorkItem;
    /// assert_eq!(worker_pool::Project::branch(&WorkItem::new("phase", "timesifter.txt")), "ai/timesifter");
    /// ```
    pub fn branch(item: &WorkItem) -> String {
        format!("ai/{}", item.stem())
    }
}

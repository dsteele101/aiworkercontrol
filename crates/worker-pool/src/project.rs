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
    /// A shell command run in each new worktree before the agent starts, to
    /// supply what a fresh checkout lacks (generated or gitignored files).
    /// It sees `$AIWC_REPO` (the repo) and `$AIWC_WORKTREE`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup: Option<String>,
    /// Where finished work is pushed and its pull request opened; nothing is
    /// pushed when `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr: Option<PullRequests>,
    /// How many jobs each of the agent's cargo builds runs in parallel,
    /// passed as `CARGO_BUILD_JOBS`.
    #[serde(default = "default_build_jobs")]
    pub build_jobs: u32,
}

/// How finished work is published: pushed to a remote, then proposed in a
/// GitHub pull request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequests {
    /// The git remote branches are pushed to, e.g. `fork`. Its URL names the
    /// GitHub owner the pull request comes from.
    pub push_remote: String,
    /// The GitHub repository the pull request is opened against, as
    /// `owner/name`, e.g. `phase-rs/phase`.
    pub repo: String,
    /// The branch the pull request targets.
    #[serde(default = "default_base_ref")]
    pub base: String,
    /// Open pull requests as drafts.
    #[serde(default)]
    pub draft: bool,
}

fn default_base_ref() -> String {
    "main".to_string()
}

/// Enough to keep a build moving without starving the other workers.
pub const DEFAULT_BUILD_JOBS: u32 = 4;

fn default_build_jobs() -> u32 {
    DEFAULT_BUILD_JOBS
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
            setup: None,
            pr: None,
            build_jobs: DEFAULT_BUILD_JOBS,
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

    /// The build cache of worker `id` (from 0), reused across the items it
    /// carries out so each build after its first is incremental.
    ///
    /// ```
    /// use std::path::Path;
    /// use worker_pool::Project;
    ///
    /// let project = Project::new("phase", "/specs/phase", "/dev/phase");
    /// assert_eq!(project.build_cache(0), Path::new("/dev/phase-worktrees/.target-1"));
    /// ```
    pub fn build_cache(&self, id: usize) -> PathBuf {
        self.worktree_root().join(format!(".target-{}", id + 1))
    }

    /// Where agents write pull request drafts: outside every worktree, so a
    /// draft is never committed.
    pub fn pr_draft_dir(&self) -> PathBuf {
        self.worktree_root().join(".pr")
    }

    /// The pull request draft for an item: its title on the first line, then
    /// a blank line and the description.
    pub fn pr_draft_path(&self, item: &WorkItem) -> PathBuf {
        self.pr_draft_dir().join(format!("{}.md", item.stem()))
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

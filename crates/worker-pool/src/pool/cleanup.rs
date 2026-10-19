//! Removing the worktrees of closed items, to free their space once their
//! work is merged.

use work_queue::{RESULT_SUFFIX, State as ItemState, WorkItem};

use super::Supervisor;
use super::worker::PR_LINE;
use crate::{ItemMeta, PoolError, Project, git, github};

/// What cleaning up an item did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cleanup {
    /// Whether a worktree folder was removed (false if it was already gone).
    pub worktree_removed: bool,
    /// What happened to the item's branch.
    pub branch: BranchOutcome,
}

/// What cleaning up did to an item's `ai/<item>` branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BranchOutcome {
    /// Its changes were all in the base, or its pull request was merged,
    /// so it was deleted.
    Deleted,
    /// The base lacks some of its changes (or has them only as a squash), so
    /// it was kept.
    KeptUnmerged,
    /// It no longer existed.
    Missing,
}

impl Supervisor {
    /// Removes a closed item's worktree, and deletes its branch if it is
    /// merged: every commit on it is already in the project's base, or its
    /// pull request was merged (which also covers squash merges).
    pub async fn clean_up(&self, item: &WorkItem) -> Result<Cleanup, PoolError> {
        if !self.queue().list(ItemState::Closed)?.contains(item) {
            return Err(PoolError::NotClosed(item.clone()));
        }
        let project = self.project(item)?;
        let worktree_removed =
            git::remove_worktree(&project.repo, &project.worktree_path(item)).await?;
        let branch = Project::branch(item);
        let outcome = if !git::branch_exists(&project.repo, &branch).await? {
            BranchOutcome::Missing
        } else if self.is_merged(&project, item, &branch).await? {
            git::delete_branch(&project.repo, &branch).await?;
            BranchOutcome::Deleted
        } else {
            BranchOutcome::KeptUnmerged
        };
        self.shared.changed();
        Ok(Cleanup {
            worktree_removed,
            branch: outcome,
        })
    }

    /// Cleans up every closed item that still has a worktree and whose
    /// branch is merged into its project's base, or was deleted. Returns the
    /// items cleaned up.
    pub async fn clean_up_merged(&self) -> Result<Vec<WorkItem>, PoolError> {
        let mut cleaned = Vec::new();
        for item in self.queue().list(ItemState::Closed)? {
            let Ok(project) = self.project(&item) else {
                continue;
            };
            if !project.worktree_path(&item).exists() {
                continue;
            }
            let branch = Project::branch(&item);
            let merged = !git::branch_exists(&project.repo, &branch).await?
                || self.is_merged(&project, &item, &branch).await?;
            if merged {
                self.clean_up(&item).await?;
                cleaned.push(item);
            }
        }
        Ok(cleaned)
    }

    /// The pull request of `item` while it is in `state`, if one was
    /// opened: from its metadata, or from its result for older items.
    pub fn pull_request(&self, state: ItemState, item: &WorkItem) -> Option<String> {
        if let Some(url) = ItemMeta::load(self.queue(), state, item)
            .ok()
            .and_then(|m| m.pr_url)
        {
            return Some(url);
        }
        let result = self.queue().sidecar_path(state, item, RESULT_SUFFIX);
        let text = std::fs::read_to_string(result).ok()?;
        text.lines()
            .find_map(|line| line.strip_prefix(PR_LINE))
            .map(|url| url.trim().to_string())
    }

    async fn is_merged(
        &self,
        project: &Project,
        item: &WorkItem,
        branch: &str,
    ) -> Result<bool, PoolError> {
        if git::is_merged(&project.repo, &project.base_ref, branch).await? {
            return Ok(true);
        }
        match self.pull_request(ItemState::Closed, item) {
            Some(url) => {
                let gh = github::program(self.config().cli_dir.as_deref());
                github::is_merged(&gh, &url).await
            }
            None => Ok(false),
        }
    }

    pub(super) fn project(&self, item: &WorkItem) -> Result<Project, PoolError> {
        self.config()
            .projects
            .into_iter()
            .find(|p| p.name == item.project)
            .ok_or_else(|| PoolError::UnknownProject(item.project.clone()))
    }
}

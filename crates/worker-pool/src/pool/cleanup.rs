//! Removing the worktrees of closed items, to free their space once their
//! work is merged.

use work_queue::{State as ItemState, WorkItem};

use super::Supervisor;
use crate::{PoolError, Project, git};

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
    /// Its changes were all in the base, so it was deleted.
    Deleted,
    /// The base lacks some of its changes (or has them only as a squash), so
    /// it was kept.
    KeptUnmerged,
    /// It no longer existed.
    Missing,
}

impl Supervisor {
    /// Removes a closed item's worktree, and deletes its branch if every
    /// commit on it is already in the project's base.
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
        } else if git::is_merged(&project.repo, &project.base_ref, &branch).await? {
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
                || git::is_merged(&project.repo, &project.base_ref, &branch).await?;
            if merged {
                self.clean_up(&item).await?;
                cleaned.push(item);
            }
        }
        Ok(cleaned)
    }

    fn project(&self, item: &WorkItem) -> Result<Project, PoolError> {
        self.config()
            .projects
            .into_iter()
            .find(|p| p.name == item.project)
            .ok_or_else(|| PoolError::UnknownProject(item.project.clone()))
    }
}

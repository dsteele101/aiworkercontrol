//! Items in review: watching their pull requests, and returning held items.

use std::path::Path;

use std::sync::Arc;

use work_queue::{Queue, State as ItemState, WorkItem};

use super::Supervisor;
use super::suggest::Evidence;
use crate::github::{self, PrState};
use crate::history::{EventKind, feedback_kinds};
use crate::{ItemMeta, PoolError, Project, git};

/// What a check of the pull requests in review did to an item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewChange {
    /// Its pull request was merged; the item is closed.
    Merged,
    /// Its pull request was closed without merging; the item is on hold.
    ClosedUnmerged,
    /// Changes were requested or CI failed; it waits for a rework.
    ChangesRequested,
}

impl Supervisor {
    /// Checks the pull request of every item in review once: a merged one
    /// closes its item, one closed unmerged puts it on hold, and feedback on
    /// its latest commit (changes requested, failing CI) queues a rework for
    /// the next free worker. Feedback on a commit already reworked is not
    /// acted on again, unless a reviewer requests changes anew.
    pub async fn check_reviews(&self) -> Result<Vec<(WorkItem, ReviewChange)>, PoolError> {
        let gh = github::program(self.config().cli_dir.as_deref());
        let mut changes = Vec::new();
        for item in self.queue().list(ItemState::InReview)? {
            if let Some(change) = self.check_item(&gh, &item, false).await? {
                changes.push((item, change));
            }
        }
        if !changes.is_empty() {
            self.shared.changed();
        }
        Ok(changes)
    }

    /// Checks the pull request of one item in review now, as
    /// [`check_reviews`](Self::check_reviews) does for all of them, and also
    /// acting on inline comments on the latest commit, such as a bot's, that
    /// the periodic check leaves alone. `None` means there is nothing new to
    /// act on.
    pub async fn check_review(&self, item: &WorkItem) -> Result<Option<ReviewChange>, PoolError> {
        if !self.queue().list(ItemState::InReview)?.contains(item) {
            return Err(PoolError::NotInReview(item.clone()));
        }
        let gh = github::program(self.config().cli_dir.as_deref());
        let change = self.check_item(&gh, item, true).await?;
        if change.is_some() {
            self.shared.changed();
        }
        Ok(change)
    }

    /// Checks the pull request of one item in review as
    /// [`check_review`](Self::check_review) does, but acts on feedback on its
    /// latest commit even if a rework already answered that commit. This is
    /// the way out when a rework pushed nothing and left the feedback standing.
    pub async fn rework_again(&self, item: &WorkItem) -> Result<Option<ReviewChange>, PoolError> {
        let queue = self.queue();
        if !queue.list(ItemState::InReview)?.contains(item) {
            return Err(PoolError::NotInReview(item.clone()));
        }
        let mut meta = ItemMeta::load(queue, ItemState::InReview, item)?;
        if meta.needs_rework.is_some() {
            return Err(PoolError::ReworkQueued(item.clone()));
        }
        meta.answered_head = None;
        meta.answered_review_at = None;
        meta.save(queue, ItemState::InReview, item)?;
        self.check_review(item).await
    }

    /// Pushes an empty commit to the pull request of an item in review, so
    /// the remote checks run again on it.
    pub async fn bump(&self, item: &WorkItem) -> Result<(), PoolError> {
        let queue = self.queue();
        if !queue.list(ItemState::InReview)?.contains(item) {
            return Err(PoolError::NotInReview(item.clone()));
        }
        let mut meta = ItemMeta::load(queue, ItemState::InReview, item)?;
        if meta.pr_url.is_none() {
            return Err(PoolError::NoPullRequest(item.clone()));
        }
        // A worker is about to rework the item in its worktree.
        if meta.needs_rework.is_some() {
            return Err(PoolError::ReworkQueued(item.clone()));
        }
        let project = self.project(item)?;
        let Some(pr) = &project.pr else {
            return Err(PoolError::NoPullRequest(item.clone()));
        };
        let worktree = project.worktree_path(item);
        let branch = Project::branch(item);
        git::ensure_worktree(&project.repo, &worktree, &branch).await?;
        let remote_branch = meta.remote_branch.clone().unwrap_or_else(|| branch.clone());
        if meta.remote_branch.is_some() {
            let source = git::push_url(&project.repo, &pr.push_remote).await?;
            git::fast_forward(&worktree, &source, &remote_branch).await?;
        }
        git::commit_empty(&worktree, "chore: re-run checks").await?;
        git::push_ahead(&worktree, &pr.push_remote, &branch, &remote_branch).await?;
        meta.pushed_head = Some(git::head(&worktree).await?);
        meta.save(queue, ItemState::InReview, item)?;
        self.shared.changed();
        Ok(())
    }

    async fn check_item(
        &self,
        gh: &Path,
        item: &WorkItem,
        any_comments: bool,
    ) -> Result<Option<ReviewChange>, PoolError> {
        let queue = self.queue();
        let mut meta = ItemMeta::load(queue, ItemState::InReview, item)?;
        let Some(url) = meta.pr_url.clone() else {
            return Ok(None);
        };
        if meta.needs_rework.is_some() {
            return Ok(None);
        }
        let status = github::review_status(gh, &url, any_comments).await?;
        let change = match status.state {
            PrState::Merged => {
                meta.save(queue, ItemState::InReview, item)?;
                queue.transition(item, ItemState::InReview, ItemState::Closed)?;
                self.shared.record(item, EventKind::Merged, None);
                ReviewChange::Merged
            }
            PrState::Closed => {
                meta.hold_reason = Some(format!(
                    "its pull request was closed without merging: {url}"
                ));
                meta.save(queue, ItemState::InReview, item)?;
                queue.transition(item, ItemState::InReview, ItemState::Hold)?;
                self.shared.record(item, EventKind::ClosedUnmerged, None);
                ReviewChange::ClosedUnmerged
            }
            PrState::Open => {
                let Some(feedback) = status.feedback else {
                    return Ok(None);
                };
                // Feedback on a commit already reworked is left alone, unless a
                // reviewer has asked for changes again since.
                if meta.answered_head.as_deref() == Some(status.head.as_str())
                    && status.reviewed_at <= meta.answered_review_at
                {
                    return Ok(None);
                }
                let kinds = feedback_kinds(&feedback);
                // A reviewer asking for changes may point at what the
                // instructions should have said.
                if kinds.iter().any(|k| k == "review") {
                    self.learn_from_review(item, &feedback);
                }
                meta.needs_rework = Some(feedback);
                meta.answered_head = Some(status.head.clone());
                meta.answered_review_at = status.reviewed_at;
                // Rework builds on what is actually on the pull request.
                meta.pushed_head = Some(status.head);
                meta.save(queue, ItemState::InReview, item)?;
                self.shared
                    .record(item, EventKind::FeedbackQueued, Some(kinds.join(",")));
                ReviewChange::ChangesRequested
            }
        };
        Ok(Some(change))
    }

    /// Has an agent evaluate the `feedback` a reviewer left on `item`'s pull
    /// request, in the background, for a lesson to suggest. Skipped when the
    /// project is unknown or the item's text cannot be read.
    fn learn_from_review(&self, item: &WorkItem, feedback: &str) {
        let (Ok(project), Ok(body)) = (
            self.project(item),
            self.queue().read(ItemState::InReview, item),
        ) else {
            return;
        };
        let evidence = Evidence::Review {
            feedback: feedback.to_string(),
        };
        let config = self.config();
        tokio::spawn(Arc::clone(&self.shared).learn_from(
            project,
            item.clone(),
            body,
            evidence,
            config,
        ));
    }

    /// Returns an item on hold to the agents with a fresh turn count. If its
    /// pull request is still open it goes back to review, keeping any
    /// feedback still to be addressed; otherwise it starts over from `open`.
    pub async fn resume(&self, item: &WorkItem) -> Result<(), PoolError> {
        let queue = self.queue();
        if !queue.list(ItemState::Hold)?.contains(item) {
            return Err(PoolError::NotOnHold(item.clone()));
        }
        let meta = ItemMeta::load(queue, ItemState::Hold, item)?;
        let pr_open = match &meta.pr_url {
            Some(url) => {
                let gh = github::program(self.config().cli_dir.as_deref());
                github::review_status(&gh, url, false).await?.state == PrState::Open
            }
            None => false,
        };
        let (fresh, to) = if pr_open {
            let fresh = ItemMeta {
                pr_url: meta.pr_url,
                pr_title: meta.pr_title,
                pushed_head: meta.pushed_head,
                needs_rework: meta.needs_rework,
                answered_head: meta.answered_head,
                answered_review_at: meta.answered_review_at,
                remote_branch: meta.remote_branch,
                ..ItemMeta::default()
            };
            (fresh, ItemState::InReview)
        } else {
            (ItemMeta::default(), ItemState::Open)
        };
        fresh.save(queue, ItemState::Hold, item)?;
        queue.transition(item, ItemState::Hold, to)?;
        self.shared.record(item, EventKind::Resumed, None);
        self.shared.changed();
        Ok(())
    }
}

/// Returns items left in progress by an earlier run, which no worker can
/// own any more: to review if they have a pull request, else to `open`.
pub(super) fn recover_orphans(queue: &Queue) -> Result<(), PoolError> {
    for item in queue.list(ItemState::InProgress)? {
        let meta = ItemMeta::load(queue, ItemState::InProgress, &item)?;
        let to = if meta.pr_url.is_some() {
            ItemState::InReview
        } else {
            ItemState::Open
        };
        queue.transition(&item, ItemState::InProgress, to)?;
    }
    Ok(())
}

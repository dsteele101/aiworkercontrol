//! Items in review: watching their pull requests, and returning held items.

use work_queue::{Queue, State as ItemState, WorkItem};

use super::Supervisor;
use crate::github::{self, PrState};
use crate::{ItemMeta, PoolError};

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
    /// acted on again.
    pub async fn check_reviews(&self) -> Result<Vec<(WorkItem, ReviewChange)>, PoolError> {
        let queue = self.queue();
        let gh = github::program(self.config().cli_dir.as_deref());
        let mut changes = Vec::new();
        for item in queue.list(ItemState::InReview)? {
            let mut meta = ItemMeta::load(queue, ItemState::InReview, &item)?;
            let Some(url) = meta.pr_url.clone() else {
                continue;
            };
            if meta.needs_rework.is_some() {
                continue;
            }
            let status = github::review_status(&gh, &url).await?;
            let change = match status.state {
                PrState::Merged => {
                    meta.save(queue, ItemState::InReview, &item)?;
                    queue.transition(&item, ItemState::InReview, ItemState::Closed)?;
                    ReviewChange::Merged
                }
                PrState::Closed => {
                    meta.hold_reason = Some(format!(
                        "its pull request was closed without merging: {url}"
                    ));
                    meta.save(queue, ItemState::InReview, &item)?;
                    queue.transition(&item, ItemState::InReview, ItemState::Hold)?;
                    ReviewChange::ClosedUnmerged
                }
                PrState::Open => {
                    let Some(feedback) = status.feedback else {
                        continue;
                    };
                    if meta.answered_head.as_deref() == Some(status.head.as_str()) {
                        continue;
                    }
                    meta.needs_rework = Some(feedback);
                    meta.answered_head = Some(status.head.clone());
                    // Rework builds on what is actually on the pull request.
                    meta.pushed_head = Some(status.head);
                    meta.save(queue, ItemState::InReview, &item)?;
                    ReviewChange::ChangesRequested
                }
            };
            changes.push((item, change));
        }
        if !changes.is_empty() {
            self.shared.changed();
        }
        Ok(changes)
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
                github::review_status(&gh, url).await?.state == PrState::Open
            }
            None => false,
        };
        let (fresh, to) = if pr_open {
            let fresh = ItemMeta {
                pr_url: meta.pr_url,
                pushed_head: meta.pushed_head,
                needs_rework: meta.needs_rework,
                answered_head: meta.answered_head,
                ..ItemMeta::default()
            };
            (fresh, ItemState::InReview)
        } else {
            (ItemMeta::default(), ItemState::Open)
        };
        fresh.save(queue, ItemState::Hold, item)?;
        queue.transition(item, ItemState::Hold, to)?;
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

//! The worker loop: claim an item, carry it out, record the outcome.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::SystemTime;

use agent_backend::AgentEvent;
use work_queue::{State as ItemState, WorkItem};

use super::run::{Finish, Kind};
use super::{POLL_INTERVAL, PoolConfig, Shared};
use crate::status::EVENT_TAIL;
use crate::{ActiveJob, Activity, ItemMeta, PoolError, Project};

/// How a result file records its pull request; read back by `pull_request`.
pub(crate) const PR_LINE: &str = "- Pull request: ";

impl Shared {
    /// Whether worker `id` should exit. Decided under the state lock, together
    /// with removing it, so [`Shared::spawn_missing`] never races a retiring
    /// worker into leaving a slot empty or doubly filled.
    fn retire(&self, id: usize) -> bool {
        let mut state = self.lock();
        let retire = !*self.running.borrow() || id >= self.config.borrow().workers;
        if retire {
            state.workers.remove(&id);
        }
        retire
    }

    pub(super) async fn worker(self: Arc<Self>, id: usize) {
        loop {
            if self.retire(id) {
                self.changed();
                return;
            }
            let claimed = self.claim();
            if claimed.is_ok() && self.lock().last_error.take().is_some() {
                self.changed();
            }
            match claimed {
                Ok(Some((item, project, config, kind))) => {
                    self.carry_out(id, item, project, config, kind).await
                }
                Ok(None) => self.idle().await,
                Err(e) => {
                    self.lock().last_error = Some(e.to_string());
                    self.changed();
                    self.idle().await;
                }
            }
        }
    }

    async fn idle(&self) {
        tokio::select! {
            () = self.wake.notified() => {}
            () = tokio::time::sleep(POLL_INTERVAL) => {}
        }
    }

    /// Claims the next item to run: first an in-review item whose feedback
    /// waits for a worker, then an open item.
    fn claim(&self) -> Result<Option<(WorkItem, Project, PoolConfig, Kind)>, PoolError> {
        let config = self.config.borrow().clone();
        let projects = config.projects.clone();
        let project_of =
            |item: &WorkItem| projects.iter().find(|p| p.name == item.project).cloned();
        for item in self.queue.list(ItemState::InReview)? {
            let Some(project) = project_of(&item) else {
                continue;
            };
            let meta = ItemMeta::load(&self.queue, ItemState::InReview, &item)?;
            let (Some(feedback), Some(pr_url), Some(pushed_head)) =
                (meta.needs_rework, meta.pr_url, meta.pushed_head)
            else {
                continue;
            };
            if self.queue.take(&item, ItemState::InReview)? {
                let kind = Kind::Rework {
                    pr_url,
                    pushed_head,
                    feedback,
                };
                return Ok(Some((item, project, config, kind)));
            }
        }
        let item = self.queue.claim_next(|item| project_of(item).is_none())?;
        Ok(item.and_then(|item| {
            let project = project_of(&item)?;
            Some((item, project, config, Kind::New))
        }))
    }

    async fn carry_out(
        &self,
        id: usize,
        item: WorkItem,
        project: Project,
        config: PoolConfig,
        kind: Kind,
    ) {
        self.set_activity(
            id,
            Activity::Working(ActiveJob {
                item: item.clone(),
                provider: config.provider.provider,
                started: SystemTime::now(),
                events: VecDeque::new(),
            }),
        );

        let outcome = match self.begin_turn(&item) {
            Ok(turn) => match self.run(id, &item, &project, &config, &kind).await {
                Ok(finish) => self.finish(&item, finish).map_err(|e| (turn, e)),
                Err(e) => Err((turn, e)),
            },
            // Without a turn count the item cannot be tracked; hold it.
            Err(e) => Err((config.max_turns, e)),
        };
        if let Err((turn, e)) = outcome
            && let Err(bookkeeping) =
                self.after_failure(&item, &e.to_string(), turn, config.max_turns)
        {
            self.lock().last_error = Some(format!("{item}: {e}; then {bookkeeping}"));
        }
        self.set_activity(id, Activity::Idle);
    }

    /// Counts a new turn for an in-progress item and returns its number.
    fn begin_turn(&self, item: &WorkItem) -> Result<u32, PoolError> {
        let mut meta = ItemMeta::load(&self.queue, ItemState::InProgress, item)?;
        meta.turns += 1;
        meta.save(&self.queue, ItemState::InProgress, item)?;
        Ok(meta.turns)
    }

    /// Moves a successfully run item on: to `closed`, to review with its
    /// pull request, or to hold when its work could not be published.
    fn finish(&self, item: &WorkItem, finish: Finish) -> Result<(), PoolError> {
        let queue = &self.queue;
        let mut meta = ItemMeta::load(queue, ItemState::InProgress, item)?;
        meta.last_error = None;
        let (report, to) = match finish {
            Finish::Close { report } => (report, ItemState::Closed),
            Finish::Review {
                report,
                pr_url,
                head,
            } => {
                meta.pr_url = Some(pr_url);
                meta.pushed_head = Some(head);
                meta.needs_rework = None;
                (report, ItemState::InReview)
            }
            Finish::Hold { report, reason } => {
                meta.hold_reason = Some(reason);
                (report, ItemState::Hold)
            }
        };
        meta.save(queue, ItemState::InProgress, item)?;
        queue.write_result(ItemState::InProgress, item, &report)?;
        Ok(queue.transition(item, ItemState::InProgress, to)?)
    }

    /// Puts a failed item on hold once it has used its last turn; otherwise
    /// returns it to be tried again: to review, still waiting for its rework,
    /// if it has a pull request, else to `open`.
    fn after_failure(
        &self,
        item: &WorkItem,
        reason: &str,
        turn: u32,
        max_turns: u32,
    ) -> Result<(), PoolError> {
        let mut meta = ItemMeta::load(&self.queue, ItemState::InProgress, item).unwrap_or_default();
        meta.last_error = Some(reason.to_string());
        if turn >= max_turns {
            meta.hold_reason = Some(format!(
                "used {turn} of {max_turns} turns; the last one failed: {reason}"
            ));
            meta.save(&self.queue, ItemState::InProgress, item)?;
            self.queue
                .transition(item, ItemState::InProgress, ItemState::Hold)?;
        } else if meta.pr_url.is_some() {
            meta.save(&self.queue, ItemState::InProgress, item)?;
            self.queue
                .transition(item, ItemState::InProgress, ItemState::InReview)?;
        } else {
            meta.save(&self.queue, ItemState::InProgress, item)?;
            self.queue.release(item)?;
        }
        Ok(())
    }

    fn set_activity(&self, id: usize, activity: Activity) {
        self.lock().workers.insert(id, activity);
        self.changed();
    }

    pub(super) fn push_event(&self, id: usize, event: AgentEvent) {
        {
            let mut state = self.lock();
            if let Some(Activity::Working(job)) = state.workers.get_mut(&id) {
                if job.events.len() == EVENT_TAIL {
                    job.events.pop_front();
                }
                job.events.push_back(event);
            }
        }
        self.version.send_modify(|v| *v = v.wrapping_add(1));
    }
}

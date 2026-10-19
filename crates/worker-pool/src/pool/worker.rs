//! The worker loop: claim an item, carry it out, record the outcome.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::SystemTime;

use agent_backend::{AgentEvent, PlanUsage, Provider};
use work_queue::{State as ItemState, WorkItem};

use super::run::{Finish, Kind};
use super::{POLL_INTERVAL, PoolConfig, Shared, State, unix_now};
use crate::balance::{Pick, Suspension};
use crate::status::EVENT_TAIL;
use crate::usage;
use crate::{ActiveJob, Activity, ItemMeta, PoolError, Project};

/// How a result file records its pull request; read back by `pull_request`.
pub(crate) const PR_LINE: &str = "- Pull request: ";

/// An item a worker has claimed, and how to run it.
struct Claim {
    item: WorkItem,
    project: Project,
    /// The settings, with the provider chosen for this run.
    config: PoolConfig,
    kind: Kind,
    /// Whether the provider is a fallback rather than the active one.
    fallback: bool,
}

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
                Ok(Some(claim)) => self.carry_out(id, claim).await,
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
    fn claim(&self) -> Result<Option<Claim>, PoolError> {
        let config = self.config.borrow().clone();
        let Some(general) = self.pick(&config, None) else {
            return Ok(None);
        };
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
            // A rework kept to its provider waits while that one can't run.
            let pick = match meta
                .provider
                .filter(|_| !config.balancing.rework_any_provider)
            {
                Some(provider) => match self.pick(&config, Some(provider)) {
                    Some(pick) => pick,
                    None => continue,
                },
                None => general.clone(),
            };
            if self.queue.take(&item, ItemState::InReview)? {
                let kind = Kind::Rework {
                    pr_url,
                    pushed_head,
                    feedback,
                    remote_branch: meta.remote_branch,
                };
                return Ok(Some(self.claimed(item, project, &config, kind, pick)));
            }
        }
        let item = self.queue.claim_next(|item| project_of(item).is_none())?;
        Ok(item.and_then(|item| {
            let project = project_of(&item)?;
            Some(self.claimed(item, project, &config, Kind::New, general))
        }))
    }

    /// Picks the provider for the next run per the plan-usage limits and
    /// the balancing scheme, or checks that `want` can run. `None` when it
    /// can't; without `want`, why is recorded for the dashboard.
    fn pick(&self, config: &PoolConfig, want: Option<Provider>) -> Option<Pick> {
        let mut state = self.lock();
        let state = &mut *state;
        let chosen = state.picker.choose(config, &state.usage, unix_now(), want);
        if want.is_none() {
            let limited = chosen.as_ref().err().cloned();
            if state.limited != limited {
                state.limited = limited;
                self.changed();
            }
        }
        chosen.ok()
    }

    /// The claim of `item`, run by `pick`'s provider, which is counted as
    /// used.
    fn claimed(
        &self,
        item: WorkItem,
        project: Project,
        config: &PoolConfig,
        kind: Kind,
        pick: Pick,
    ) -> Claim {
        self.lock().picker.commit(&pick, &config.balancing);
        let fallback = pick.fallback;
        let config = PoolConfig {
            provider: pick.config,
            ..config.clone()
        };
        Claim {
            item,
            project,
            config,
            kind,
            fallback,
        }
    }

    async fn carry_out(&self, id: usize, claim: Claim) {
        let Claim {
            item,
            project,
            config,
            kind,
            fallback,
        } = claim;
        self.set_activity(
            id,
            Activity::Working(ActiveJob {
                item: item.clone(),
                provider: config.provider.provider,
                started: SystemTime::now(),
                events: VecDeque::new(),
                last_event: SystemTime::now(),
                status: None,
                background: Vec::new(),
                background_since: None,
            }),
        );

        let outcome = match self.begin_turn(&item) {
            Ok(turn) => match self.run(id, &item, &project, &config, &kind).await {
                Ok(finish) => self
                    .finish(&item, finish, config.provider.provider)
                    .map_err(|e| (Some(turn), e)),
                Err(e) => Err((Some(turn), e)),
            },
            Err(e) => Err((None, e)),
        };
        if let Err((turn, e)) = outcome {
            let reason = e.to_string();
            // A failing fallback is set aside, and its run is not held
            // against the item: the item only ran there because the active
            // provider was stopped.
            let refund = fallback && turn.is_some();
            if refund {
                self.suspend(config.provider.provider, &reason);
            }
            // Without a turn count the item cannot be tracked; hold it.
            let turn = turn.unwrap_or(config.max_turns);
            if let Err(bookkeeping) =
                self.after_failure(&item, &reason, turn, config.max_turns, refund)
            {
                self.lock().last_error = Some(format!("{item}: {e}; then {bookkeeping}"));
            }
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
    /// pull request (and `provider`, who pushed to it), or to hold when its
    /// work could not be published.
    fn finish(&self, item: &WorkItem, finish: Finish, provider: Provider) -> Result<(), PoolError> {
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
                meta.provider = Some(provider);
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
    /// if it has a pull request, else to `open`. A `refund`ed run gives its
    /// turn back and never holds the item.
    fn after_failure(
        &self,
        item: &WorkItem,
        reason: &str,
        turn: u32,
        max_turns: u32,
        refund: bool,
    ) -> Result<(), PoolError> {
        let mut meta = ItemMeta::load(&self.queue, ItemState::InProgress, item).unwrap_or_default();
        meta.last_error = Some(reason.to_string());
        if refund {
            meta.turns = meta.turns.saturating_sub(1);
        }
        if !refund && turn >= max_turns {
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

    /// Stops using fallback `provider` after a failed run, until the active
    /// provider can run again.
    fn suspend(&self, provider: Provider, reason: &str) {
        let suspension = Suspension {
            error: reason.to_string(),
            since: unix_now(),
        };
        self.lock().picker.suspended.insert(provider, suspension);
        self.changed();
    }

    fn set_activity(&self, id: usize, activity: Activity) {
        self.lock().workers.insert(id, activity);
        self.changed();
    }

    pub(super) fn push_event(&self, id: usize, event: AgentEvent) {
        {
            let mut state = self.lock();
            if let AgentEvent::Usage(usage) = event {
                if let Some(Activity::Working(job)) = state.workers.get(&id) {
                    let provider = job.provider;
                    self.record_usage(&mut state, provider, usage);
                }
            } else if let Some(Activity::Working(job)) = state.workers.get_mut(&id) {
                match event {
                    AgentEvent::Status(status) => job.status = Some(status),
                    AgentEvent::Background(tasks) => {
                        job.background_since = match (tasks.is_empty(), job.background_since) {
                            (true, _) => None,
                            (false, Some(since)) => Some(since),
                            (false, None) => Some(SystemTime::now()),
                        };
                        job.background = tasks;
                    }
                    event => {
                        if job.events.len() == EVENT_TAIL {
                            job.events.pop_front();
                        }
                        job.events.push_back(event);
                        job.last_event = SystemTime::now();
                    }
                }
            }
        }
        self.version.send_modify(|v| *v = v.wrapping_add(1));
    }

    /// Records `provider`'s reported usage, and saves it when it changed.
    pub(super) fn record_usage(&self, state: &mut State, provider: Provider, usage: PlanUsage) {
        if state.usage.get(&provider) == Some(&usage) {
            return;
        }
        state.usage.insert(provider, usage);
        if let Err(e) = usage::save(&self.usage_file, &state.usage) {
            state.last_error = Some(e.to_string());
        }
    }
}

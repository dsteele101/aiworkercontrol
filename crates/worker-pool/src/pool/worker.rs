//! The worker loop: claim an item, carry it out, record the outcome.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use agent_backend::{AgentEvent, PlanUsage, Provider};
use work_queue::{State as ItemState, WorkItem};

use super::run::{Finish, Kind};
use super::suggest::Evidence;
use super::trace::Trace;
use super::{POLL_INTERVAL, PoolConfig, Shared, State, unix_now};
use crate::balance::{Pick, Suspension};
use crate::history::{Entry, EventKind, TurnOutcome, TurnReason, TurnRecord, feedback_kinds};
use crate::status::EVENT_TAIL;
use crate::suggestions;
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
        let item = self
            .queue
            .claim_next_by(config.claim_order, |item| project_of(item).is_none())?;
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

    async fn carry_out(self: &Arc<Self>, id: usize, claim: Claim) {
        let Claim {
            item,
            project,
            config,
            kind,
            fallback,
        } = claim;
        let (config, model_reason) = self.choose_model(&item, &kind, config).await;
        self.set_activity(
            id,
            Activity::Working(Box::new(ActiveJob {
                item: item.clone(),
                provider: config.provider.provider,
                model: config.provider.model.clone().unwrap_or_default(),
                started: SystemTime::now(),
                events: VecDeque::new(),
                last_event: SystemTime::now(),
                status: None,
                background: Vec::new(),
                background_since: None,
            })),
        );

        let started_at = unix_now();
        // Read now: the item leaves `inprogress` when the turn ends.
        let queued_at = self.queued_at(&item);
        let clock = Instant::now();
        let provider = config.provider.provider;
        let quota_before = self.lock().usage.get(&provider).cloned();
        let trace = Mutex::new(Trace::default());
        let mut context = None;
        let outcome = match self.begin_turn(&item) {
            Ok((turn, retry)) => {
                context = Some((turn, retry));
                match self.run(id, &item, &project, &config, &kind, &trace).await {
                    Ok(finish) => {
                        let to = match &finish {
                            Finish::Close { .. } => TurnOutcome::Closed,
                            Finish::Review { .. } => TurnOutcome::Review,
                            Finish::Hold { .. } => TurnOutcome::Hold,
                        };
                        self.finish(&item, finish, provider)
                            .map(|()| to)
                            .map_err(|e| (Some(turn), e))
                    }
                    Err(e) => Err((Some(turn), e)),
                }
            }
            Err(e) => Err((None, e)),
        };
        let mut failure = None;
        let mut refunded = false;
        if let Err((turn, e)) = &outcome {
            let reason = e.to_string();
            // A failing fallback is set aside, and its run is not held
            // against the item: the item only ran there because the active
            // provider was stopped. Nor is a run stopped for its provider's
            // limit: that is no fault of the item's.
            let halted = matches!(e, PoolError::ProviderLimit(_));
            let refund = (fallback || halted) && turn.is_some();
            refunded = refund;
            if refund && !halted {
                self.suspend(provider, &reason);
            }
            // A failure the agent could have avoided is worth a lesson for
            // later runs. The item's text is read before it leaves
            // `inprogress`.
            let kind_of_failure = failure_kind(e, trace_stalled(&trace));
            let lesson = (!refund && suggestions::eligible(kind_of_failure, &reason))
                .then(|| self.queue.read(ItemState::InProgress, &item).ok())
                .flatten();
            // Without a turn count the item cannot be tracked; hold it.
            let turn = turn.unwrap_or(config.max_turns);
            if let Err(bookkeeping) =
                self.after_failure(&item, &reason, turn, config.max_turns, refund)
            {
                self.lock().last_error = Some(format!("{item}: {e}; then {bookkeeping}"));
            }
            if let Some(body) = lesson {
                let evidence = Evidence::Failure {
                    kind: kind_of_failure,
                    error: reason.clone(),
                };
                tokio::spawn(Arc::clone(self).learn_from(
                    project.clone(),
                    item.clone(),
                    body,
                    evidence,
                    config.clone(),
                ));
            }
            failure = Some((kind_of_failure, reason));
        }
        if let Some((turn, retry)) = context {
            let quota_after = self.lock().usage.get(&provider).cloned();
            let record = turn_record(
                &item,
                &config,
                &kind,
                TurnFacts {
                    turn,
                    retry,
                    fallback,
                    started_at,
                    elapsed: clock.elapsed(),
                    outcome: outcome.as_ref().map_or(TurnOutcome::Failed, |o| *o),
                    failure,
                    refunded,
                    quota_before,
                    quota_after,
                    model_reason,
                },
                trace.into_inner().unwrap_or_else(PoisonError::into_inner),
                queued_at,
            );
            self.record_entry(&Entry::Turn(Box::new(record)));
        }
        self.set_activity(id, Activity::Idle);
    }

    /// Counts a new turn for an in-progress item and returns its number,
    /// and whether it follows a failed one.
    fn begin_turn(&self, item: &WorkItem) -> Result<(u32, bool), PoolError> {
        let mut meta = ItemMeta::load(&self.queue, ItemState::InProgress, item)?;
        let retry = meta.last_error.is_some();
        meta.turns += 1;
        meta.save(&self.queue, ItemState::InProgress, item)?;
        Ok((meta.turns, retry))
    }

    /// When the in-progress `item` was added to the queue, as far as its
    /// file's age tells (it keeps its age as it moves between states).
    fn queued_at(&self, item: &WorkItem) -> Option<u64> {
        std::fs::metadata(self.queue.path(ItemState::InProgress, item))
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
    }

    /// Adds `entry` to the work history. The history is a record, not part
    /// of the work, so a failure to write it is shown but stops nothing.
    fn record_entry(&self, entry: &Entry) {
        if let Err(e) = self.ledger.append(entry) {
            self.lock().last_error = Some(format!("work history: {e}"));
        }
    }

    /// Records an `event` in `item`'s life in the work history.
    pub(super) fn record(&self, item: &WorkItem, event: EventKind, detail: Option<String>) {
        if let Err(e) = self.ledger.event(item, event, detail) {
            self.lock().last_error = Some(format!("work history: {e}"));
        }
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
                pr_title,
                head,
            } => {
                meta.pr_url = Some(pr_url);
                if pr_title.is_some() {
                    meta.pr_title = pr_title;
                }
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
                    AgentEvent::Model(model) => job.model = model,
                    // For the work history only; not a step to show.
                    AgentEvent::Metrics(_) => {}
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

/// What the worker knows about a turn once it is over.
struct TurnFacts {
    turn: u32,
    retry: bool,
    fallback: bool,
    started_at: u64,
    elapsed: std::time::Duration,
    outcome: TurnOutcome,
    failure: Option<(&'static str, String)>,
    refunded: bool,
    quota_before: Option<PlanUsage>,
    quota_after: Option<PlanUsage>,
    model_reason: Option<String>,
}

fn trace_stalled(trace: &Mutex<Trace>) -> bool {
    trace.lock().unwrap_or_else(PoisonError::into_inner).stalled
}

/// A coarse category for a failed turn, for grouping in the history.
fn failure_kind(error: &PoolError, stalled: bool) -> &'static str {
    match error {
        PoolError::AgentFailed(_) if stalled => "stalled",
        PoolError::AgentFailed(_) => "agent",
        PoolError::ProviderLimit(_) => "limit",
        PoolError::NoCommits(_) => "no_commits",
        PoolError::Setup { .. } => "setup",
        PoolError::Spawn { .. } => "spawn",
        PoolError::Git { .. } => "git",
        PoolError::GitHub { .. } => "github",
        _ => "other",
    }
}

fn turn_record(
    item: &WorkItem,
    config: &PoolConfig,
    kind: &Kind,
    facts: TurnFacts,
    trace: Trace,
    queued_at: Option<u64>,
) -> TurnRecord {
    let (reason, feedback) = match kind {
        Kind::Rework { feedback, .. } => (TurnReason::Rework, feedback_kinds(feedback)),
        Kind::New if facts.retry => (TurnReason::Retry, Vec::new()),
        Kind::New => (TurnReason::First, Vec::new()),
    };
    let non_empty = |s: &Option<String>| s.as_deref().filter(|s| !s.is_empty()).map(str::to_string);
    let (failure, error) = match facts.failure {
        Some((kind, reason)) => (Some(kind.to_string()), Some(reason)),
        None => (None, None),
    };
    let mut metrics = trace.metrics;
    metrics.models.clear();
    let diff = trace.diff;
    let ended_at = facts.started_at + facts.elapsed.as_secs();
    TurnRecord {
        project: item.project.clone(),
        item: item.file.clone(),
        turn: facts.turn,
        reason,
        feedback,
        provider: config.provider.provider,
        fallback: facts.fallback,
        model_requested: non_empty(&config.provider.model),
        model_reason: facts.model_reason,
        models: trace.models.into_iter().collect(),
        effort: non_empty(&config.provider.effort),
        queued_at: (reason == TurnReason::First).then_some(queued_at).flatten(),
        started_at: facts.started_at,
        ended_at,
        duration_ms: u64::try_from(facts.elapsed.as_millis()).unwrap_or(u64::MAX),
        agent_ms: trace.agent_ms,
        outcome: facts.outcome,
        failure,
        error,
        stalled: trace.stalled,
        refunded: facts.refunded,
        metrics,
        tool_calls: trace.tool_calls,
        commits: trace.commits,
        lines_added: diff.map(|d| d.added),
        lines_removed: diff.map(|d| d.removed),
        files_changed: diff.map(|d| d.files),
        quota_before: facts.quota_before,
        quota_after: facts.quota_after,
        backfilled: false,
    }
}

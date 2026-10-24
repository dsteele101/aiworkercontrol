//! The supervisor: owns the shared pool state and spawns the workers.

mod backfill;
mod cleanup;
mod import;
mod review;
mod run;
mod suggest;
mod trace;
mod worker;

pub use cleanup::{BranchOutcome, Cleanup};
pub use review::ReviewChange;

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agent_backend::{Provider, ProviderConfig};
use tokio::sync::{Notify, watch};
use work_queue::{Queue, State as ItemState, WorkItem};

use crate::agent;
use crate::balance::Picker;
use crate::history::{self, ItemSummary, Ledger};
use crate::usage::{self, UsageMap};
use crate::{
    Activity, Balancing, ItemMeta, PoolError, PoolSnapshot, Project, UsageLimits, WorkerStatus,
};

/// The default for [`PoolConfig::stall_after`].
pub const DEFAULT_STALL_AFTER: Duration = Duration::from_secs(20 * 60);

/// The default for [`PoolConfig::suggestion_retention`]: two weeks.
pub const DEFAULT_SUGGESTION_RETENTION: Duration = Duration::from_secs(14 * 24 * 60 * 60);

/// How often idle workers look for new items when nothing wakes them.
const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Settings the pool runs with. Changes apply to the next item each worker
/// picks up.
#[derive(Debug, Clone)]
pub struct PoolConfig {
    /// How many workers to run.
    pub workers: usize,
    /// The AI system that carries out items.
    pub provider: ProviderConfig,
    /// The known projects. Items of any other project are left in `open`.
    pub projects: Vec<Project>,
    /// Where to find the agent CLIs; `PATH` when `None`.
    pub cli_dir: Option<PathBuf>,
    /// Agent runs an item may use before it goes to `hold`; at least 1.
    pub max_turns: u32,
    /// When plan usage warns and stops runs.
    pub limits: UsageLimits,
    /// Further providers, in order: fallbacks under [`crate::Scheme::Priority`]
    /// (when [`UsageLimits::failover`] is on), else peers of `provider` that
    /// share the work.
    pub fallbacks: Vec<ProviderConfig>,
    /// How work is spread across `provider` and `fallbacks`.
    pub balancing: Balancing,
    /// How long an agent may go without output while its processes use
    /// next to no CPU before it is stopped as stalled.
    pub stall_after: Duration,
    /// How long a rejected suggestion for the agent instructions is kept,
    /// in case the user changes their mind.
    pub suggestion_retention: Duration,
}

/// Owns the workers. Cheap to clone; every clone controls the same pool.
#[derive(Debug, Clone)]
pub struct Supervisor {
    shared: Arc<Shared>,
}

#[derive(Debug)]
struct Shared {
    queue: Queue,
    config: watch::Sender<PoolConfig>,
    running: watch::Sender<bool>,
    version: watch::Sender<u64>,
    state: Mutex<State>,
    wake: Notify,
    usage_file: PathBuf,
    ledger: Ledger,
    /// Held while a project's suggestions are read, changed and saved, so
    /// evaluations finishing together do not overwrite one another.
    notes_lock: Mutex<()>,
}

#[derive(Debug, Default)]
struct State {
    workers: BTreeMap<usize, Activity>,
    last_error: Option<String>,
    usage: UsageMap,
    picker: Picker,
    limited: Option<String>,
}

impl Supervisor {
    /// Creates a stopped pool over `queue`. Items left `inprogress` by an
    /// earlier run, which no worker can own any more, go back to `open`.
    pub fn new(queue: Queue, config: PoolConfig) -> Result<Self, PoolError> {
        review::recover_orphans(&queue)?;
        let usage_file = usage::path(&queue);
        let ledger = Ledger::new(&queue);
        let state = State {
            usage: usage::load(&usage_file),
            ..State::default()
        };
        Ok(Self {
            shared: Arc::new(Shared {
                queue,
                config: watch::Sender::new(config),
                running: watch::Sender::new(false),
                version: watch::Sender::new(0),
                state: Mutex::new(state),
                wake: Notify::new(),
                usage_file,
                ledger,
                notes_lock: Mutex::new(()),
            }),
        })
    }

    /// Starts taking items. Must be called inside a Tokio runtime.
    pub fn start(&self) {
        self.shared.running.send_replace(true);
        self.shared.spawn_missing();
        self.shared.changed();
    }

    /// Stops taking new items. Workers finish the item they are on.
    pub fn stop(&self) {
        self.shared.running.send_replace(false);
        self.shared.changed();
    }

    /// Replaces the settings. Raising the worker count starts workers at
    /// once; lowering it retires the surplus as they finish their items.
    pub fn set_config(&self, config: PoolConfig) {
        self.shared.config.send_replace(config);
        if *self.shared.running.borrow() {
            self.shared.spawn_missing();
        }
        self.shared.changed();
    }

    /// Lets runs start on `provider` again although its plan usage has
    /// reached the stop level, until the usage drops below it.
    pub fn override_stop(&self, provider: Provider) {
        self.shared.lock().picker.overrides.insert(provider);
        self.shared.changed();
    }

    /// Asks each configured provider that can report its plan usage
    /// between runs (Antigravity) for it, and records the answers. Other
    /// providers report usage only while they run. Returns the reports that
    /// could not be read.
    pub async fn refresh_usage(&self) -> Vec<PoolError> {
        let config = self.config();
        let mut errors = Vec::new();
        for provider in std::iter::once(&config.provider).chain(&config.fallbacks) {
            let Some(run) = provider.usage_invocation() else {
                continue;
            };
            let label = provider.provider.label();
            let failed = |reason: &str| PoolError::UsageReport {
                provider: label,
                reason: reason.to_string(),
            };
            match agent::report(&run, config.cli_dir.as_deref()).await {
                Ok(Some(out)) => match provider.parse_usage_report(&out) {
                    Some(usage) => {
                        let mut state = self.shared.lock();
                        self.shared
                            .record_usage(&mut state, provider.provider, usage);
                    }
                    None => errors.push(failed("unexpected output")),
                },
                Ok(None) => errors.push(failed("the report failed or timed out")),
                Err(e) => errors.push(e),
            }
        }
        self.shared.changed();
        errors
    }

    /// The current settings.
    pub fn config(&self) -> PoolConfig {
        self.shared.config.borrow().clone()
    }

    /// What the pool has recorded about `item` while it is in `state`.
    pub fn meta(&self, state: ItemState, item: &WorkItem) -> Result<ItemMeta, PoolError> {
        ItemMeta::load(&self.shared.queue, state, item)
    }

    /// Every item the pool has run, summarized from the work history, most
    /// recently started first: its runs, providers, models, time, tokens and
    /// plan usage. Items still in the queue also say where they are.
    pub fn history(&self) -> Result<Vec<ItemSummary>, PoolError> {
        let entries = self.shared.ledger.load()?;
        let mut items = history::summarize(&entries, unix_now());
        for summary in &mut items {
            for state in ItemState::ALL {
                let item = WorkItem::new(&summary.project, &summary.item);
                if self.queue().path(state, &item).exists() {
                    summary.state = Some(state.dir_name().to_string());
                    summary.pr_url = ItemMeta::load(self.queue(), state, &item)
                        .ok()
                        .and_then(|m| m.pr_url);
                    break;
                }
            }
        }
        Ok(items)
    }

    /// The work queue the pool takes items from.
    pub fn queue(&self) -> &Queue {
        &self.shared.queue
    }

    /// A view of the pool as it is now.
    pub fn snapshot(&self) -> PoolSnapshot {
        let running = *self.shared.running.borrow();
        let target_workers = self.shared.config.borrow().workers;
        let state = self.shared.lock();
        PoolSnapshot {
            running,
            target_workers,
            workers: state
                .workers
                .iter()
                .map(|(id, activity)| WorkerStatus {
                    id: *id,
                    activity: activity.clone(),
                })
                .collect(),
            last_error: state.last_error.clone(),
            usage: usage::listed(&state.usage),
            limited: state.limited.clone(),
            overridden: Provider::ALL
                .into_iter()
                .filter(|p| state.picker.overrides.contains(p))
                .collect(),
            suspended: Provider::ALL
                .into_iter()
                .filter_map(|p| {
                    let suspension = state.picker.suspended.get(&p)?;
                    Some((p, suspension.error.clone()))
                })
                .collect(),
        }
    }

    /// A receiver that is marked changed whenever the snapshot changes.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.shared.version.subscribe()
    }
}

/// Seconds since the Unix epoch, as plan usage reports reset times.
pub(crate) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        // The state stays consistent at every await-free step, so a panic
        // elsewhere leaves nothing half-written to guard against.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn changed(&self) {
        self.version.send_modify(|v| *v = v.wrapping_add(1));
        self.wake.notify_waiters();
    }

    fn spawn_missing(self: &Arc<Self>) {
        let target = self.config.borrow().workers;
        let mut state = self.lock();
        for id in 0..target {
            if let Entry::Vacant(slot) = state.workers.entry(id) {
                slot.insert(Activity::Idle);
                tokio::spawn(Arc::clone(self).worker(id));
            }
        }
    }
}

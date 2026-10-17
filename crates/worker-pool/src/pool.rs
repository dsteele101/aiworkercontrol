//! The supervisor: owns the shared pool state and spawns the workers.

mod cleanup;
mod review;
mod run;
mod worker;

pub use cleanup::{BranchOutcome, Cleanup};
pub use review::ReviewChange;

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agent_backend::{Provider, ProviderConfig};
use tokio::sync::{Notify, watch};
use work_queue::{Queue, State as ItemState, WorkItem};

use crate::usage::{self, UsageMap};
use crate::{Activity, ItemMeta, PoolError, PoolSnapshot, Project, UsageLimits, WorkerStatus};

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
    /// Providers to fail over to, in order, when `provider` reaches the stop
    /// level and [`UsageLimits::failover`] is on.
    pub fallbacks: Vec<ProviderConfig>,
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
}

#[derive(Debug, Default)]
struct State {
    workers: BTreeMap<usize, Activity>,
    last_error: Option<String>,
    usage: UsageMap,
    overrides: HashSet<Provider>,
    limited: Option<String>,
}

impl Supervisor {
    /// Creates a stopped pool over `queue`. Items left `inprogress` by an
    /// earlier run, which no worker can own any more, go back to `open`.
    pub fn new(queue: Queue, config: PoolConfig) -> Result<Self, PoolError> {
        review::recover_orphans(&queue)?;
        let usage_file = usage::path(&queue);
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
        self.shared.lock().overrides.insert(provider);
        self.shared.changed();
    }

    /// The current settings.
    pub fn config(&self) -> PoolConfig {
        self.shared.config.borrow().clone()
    }

    /// What the pool has recorded about `item` while it is in `state`.
    pub fn meta(&self, state: ItemState, item: &WorkItem) -> Result<ItemMeta, PoolError> {
        ItemMeta::load(&self.shared.queue, state, item)
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
                .filter(|p| state.overrides.contains(p))
                .collect(),
        }
    }

    /// A receiver that is marked changed whenever the snapshot changes.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.shared.version.subscribe()
    }
}

/// Seconds since the Unix epoch, as plan usage reports reset times.
fn unix_now() -> u64 {
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

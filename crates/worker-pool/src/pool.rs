//! The supervisor: owns the shared pool state and spawns the workers.

mod worker;

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use agent_backend::ProviderConfig;
use tokio::sync::{Notify, watch};
use work_queue::{Queue, WorkItem};

use crate::{Activity, PoolError, PoolSnapshot, Project, WorkerStatus};

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
}

#[derive(Debug, Default)]
struct State {
    workers: BTreeMap<usize, Activity>,
    failed: BTreeMap<WorkItem, String>,
    last_error: Option<String>,
}

impl Supervisor {
    /// Creates a stopped pool over `queue`. Items left `inprogress` by an
    /// earlier run, which no worker can own any more, go back to `open`.
    pub fn new(queue: Queue, config: PoolConfig) -> Result<Self, PoolError> {
        queue.recover_orphans()?;
        Ok(Self {
            shared: Arc::new(Shared {
                queue,
                config: watch::Sender::new(config),
                running: watch::Sender::new(false),
                version: watch::Sender::new(0),
                state: Mutex::new(State::default()),
                wake: Notify::new(),
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

    /// The current settings.
    pub fn config(&self) -> PoolConfig {
        self.shared.config.borrow().clone()
    }

    /// Makes a failed item eligible to be picked up again.
    pub fn retry(&self, item: &WorkItem) {
        self.shared.lock().failed.remove(item);
        self.shared.changed();
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
            failed: state
                .failed
                .iter()
                .map(|(item, reason)| (item.clone(), reason.clone()))
                .collect(),
            last_error: state.last_error.clone(),
        }
    }

    /// A receiver that is marked changed whenever the snapshot changes.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.shared.version.subscribe()
    }
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

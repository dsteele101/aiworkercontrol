//! What the pool is doing, for display.

use std::collections::VecDeque;
use std::time::SystemTime;

use agent_backend::{AgentEvent, Provider};
use work_queue::WorkItem;

use crate::ProviderUsage;

/// Most recent events kept per running job.
pub(crate) const EVENT_TAIL: usize = 50;

/// A point-in-time view of the whole pool.
#[derive(Debug, Clone)]
pub struct PoolSnapshot {
    /// Whether workers are taking new items.
    pub running: bool,
    /// The configured number of workers.
    pub target_workers: usize,
    /// The workers that currently exist, by id. While shrinking there can be
    /// more than `target_workers`, each finishing its current item.
    pub workers: Vec<WorkerStatus>,
    /// The last error that kept workers from claiming items, if any.
    pub last_error: Option<String>,
    /// Each provider's latest reported plan usage; providers that never
    /// reported any are left out.
    pub usage: Vec<ProviderUsage>,
    /// Why workers are waiting instead of taking items: every provider they
    /// may use has reached the stop level.
    pub limited: Option<String>,
    /// Providers whose stop level the user has overridden until their
    /// usage drops back below it.
    pub overridden: Vec<Provider>,
    /// Fallbacks set aside after a failed run, with its error, until the
    /// active provider can run again.
    pub suspended: Vec<(Provider, String)>,
}

/// One worker.
#[derive(Debug, Clone)]
pub struct WorkerStatus {
    /// The worker's id, from 0.
    pub id: usize,
    /// What it is doing.
    pub activity: Activity,
}

/// What a worker is doing.
#[derive(Debug, Clone)]
pub enum Activity {
    /// Waiting for an item.
    Idle,
    /// Carrying out an item.
    Working(ActiveJob),
}

/// An item a worker is carrying out.
#[derive(Debug, Clone)]
pub struct ActiveJob {
    /// The item.
    pub item: WorkItem,
    /// The provider running it.
    pub provider: Provider,
    /// When the worker picked it up.
    pub started: SystemTime,
    /// The agent's most recent events, oldest first.
    pub events: VecDeque<AgentEvent>,
    /// When the newest of `events` arrived: the step it started (a command,
    /// a reply) has been going since.
    pub last_event: SystemTime,
    /// The agent's latest account of where it stands.
    pub status: Option<String>,
    /// Commands the agent left running in the background, by description.
    pub background: Vec<String>,
    /// Since when some background command has been running.
    pub background_since: Option<SystemTime>,
}

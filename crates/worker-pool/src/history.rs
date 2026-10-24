//! The work history: an append-only ledger of everything the pool did, kept
//! in `history.jsonl` in the work directory.
//!
//! Item sidecars hold only an item's current bookkeeping and are reset or
//! moved as it changes state. The ledger is what remains: one [`TurnRecord`]
//! per agent run and one [`ItemEvent`] per change in an item's life, from
//! which [`summarize`] builds each item's [`ItemSummary`].

mod summary;

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use agent_backend::{PlanUsage, Provider, RunMetrics};
use serde::{Deserialize, Serialize};
use work_queue::Queue;

pub use summary::{ItemSummary, QuotaSpend, summarize};

use crate::PoolError;

/// The file's name, in the queue's root.
const HISTORY_FILE: &str = "history.jsonl";

/// Why an agent was run on an item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnReason {
    /// The first attempt.
    First,
    /// Another attempt after a failed one.
    Retry,
    /// Addressing feedback on the item's pull request.
    Rework,
}

/// How a turn ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnOutcome {
    /// Pushed to a pull request, which now waits for review.
    Review,
    /// Done; the project does not publish pull requests.
    Closed,
    /// Committed but not published: needs a person.
    Hold,
    /// The turn failed.
    Failed,
}

/// One agent run on an item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnRecord {
    /// The item's project.
    pub project: String,
    /// The item's file name.
    pub item: String,
    /// The turn's number in the item's current turn count, from 1. The count
    /// restarts when an item on hold is returned to an agent.
    pub turn: u32,
    /// Why the turn ran.
    pub reason: TurnReason,
    /// For a rework, what the feedback was made of: `ci`, `review`,
    /// `comments`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub feedback: Vec<String>,
    /// The provider that ran it.
    pub provider: Provider,
    /// Whether the provider was a fallback rather than the active one.
    pub fallback: bool,
    /// The model the settings asked for; blank means the CLI's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_requested: Option<String>,
    /// The models the CLI reported using.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,
    /// The effort level the settings asked for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// When the item was added to the queue, as far as the file's age tells;
    /// recorded on a first turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queued_at: Option<u64>,
    /// When the turn started, in Unix seconds.
    pub started_at: u64,
    /// When it ended, in Unix seconds.
    pub ended_at: u64,
    /// How long the whole turn took, setup and publishing included.
    pub duration_ms: u64,
    /// How long the agent itself ran.
    pub agent_ms: u64,
    /// How the turn ended.
    pub outcome: TurnOutcome,
    /// For a failed turn, what kind of failure: `agent`, `stalled`,
    /// `no_commits`, `setup`, `spawn`, `git`, `github`, `other`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
    /// For a failed turn, the reason given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Whether the watchdog stopped the agent as stalled.
    #[serde(default)]
    pub stalled: bool,
    /// Whether the turn was given back, not counting against the item.
    #[serde(default)]
    pub refunded: bool,
    /// Tokens and cost the CLI reported, summed over the turn.
    pub metrics: RunMetrics,
    /// Tool calls by tool name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tool_calls: BTreeMap<String, u32>,
    /// Commits the turn added.
    pub commits: u32,
    /// Lines the turn added, if it got as far as counting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines_added: Option<u64>,
    /// Lines the turn removed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines_removed: Option<u64>,
    /// Files the turn touched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files_changed: Option<u64>,
    /// The provider's plan usage when the turn started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota_before: Option<PlanUsage>,
    /// The provider's plan usage when it ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota_after: Option<PlanUsage>,
    /// Made up afterwards from the item's sidecars, not recorded as it
    /// ran: its times, and whatever else is blank, are unknown, not zero.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub backfilled: bool,
}

/// What happened to an item outside a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    /// A pull request opened elsewhere joined the queue.
    Imported,
    /// Feedback on the pull request was queued for a rework.
    FeedbackQueued,
    /// The pull request was merged.
    Merged,
    /// The pull request was closed without merging.
    ClosedUnmerged,
    /// An item on hold was returned to an agent.
    Resumed,
    /// An item on hold was closed by hand.
    Closed,
}

/// A change in an item's life that no turn caused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemEvent {
    /// The item's project.
    pub project: String,
    /// The item's file name.
    pub item: String,
    /// When it happened, in Unix seconds.
    pub at: u64,
    /// What happened.
    pub event: EventKind,
    /// More about it, such as the kinds of feedback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// One line of the ledger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Entry {
    /// An agent run.
    Turn(Box<TurnRecord>),
    /// A change outside a turn.
    Event(ItemEvent),
}

/// The ledger file.
#[derive(Debug, Clone)]
pub(crate) struct Ledger {
    path: PathBuf,
}

impl Ledger {
    pub(crate) fn new(queue: &Queue) -> Self {
        Self {
            path: queue.root().join(HISTORY_FILE),
        }
    }

    /// Adds `entry` as a line at the end. Written with one call on a file
    /// opened for appending, so concurrent workers' lines do not mix.
    pub(crate) fn append(&self, entry: &Entry) -> Result<(), PoolError> {
        // Serializing these plain structs cannot fail.
        let mut line = serde_json::to_string(entry).unwrap_or_default();
        line.push('\n');
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .and_then(|mut file| file.write_all(line.as_bytes()))
            .map_err(PoolError::io("write", self.path.clone()))
    }

    /// Records `event` on the item now.
    pub(crate) fn event(
        &self,
        item: &work_queue::WorkItem,
        event: EventKind,
        detail: Option<String>,
    ) -> Result<(), PoolError> {
        self.append(&Entry::Event(ItemEvent {
            project: item.project.clone(),
            item: item.file.clone(),
            at: crate::pool::unix_now(),
            event,
            detail,
        }))
    }

    /// Every entry, oldest first. A missing file is an empty history; a line
    /// that does not parse, such as one cut short by a crash, is skipped.
    pub(crate) fn load(&self) -> Result<Vec<Entry>, PoolError> {
        read(&self.path)
    }
}

fn read(path: &Path) -> Result<Vec<Entry>, PoolError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(PoolError::io("read", path.to_path_buf())(e)),
    }
}

/// What kinds of feedback a rework prompt's feedback text holds.
pub(crate) fn feedback_kinds(feedback: &str) -> Vec<String> {
    [
        ("ci", "### Failing CI checks"),
        ("review", "CHANGES_REQUESTED"),
        ("comments", "### Inline comments"),
    ]
    .into_iter()
    .filter(|(_, marker)| feedback.contains(marker))
    .map(|(kind, _)| kind.to_string())
    .collect()
}

#[cfg(test)]
mod tests {
    // Test code asserts rather than propagating; see Rust code review.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]

    use super::*;

    #[test]
    fn feedback_kinds_follow_the_feedback_headings() {
        let text = "### Review by a (CHANGES_REQUESTED)\n\nfix\n\n### Failing CI checks\n\n- x";
        assert_eq!(feedback_kinds(text), vec!["ci", "review"]);
        assert!(feedback_kinds("nothing").is_empty());
    }

    #[test]
    fn unreadable_lines_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.jsonl");
        let event = Entry::Event(ItemEvent {
            project: "p".into(),
            item: "a.txt".into(),
            at: 5,
            event: EventKind::Merged,
            detail: None,
        });
        let line = serde_json::to_string(&event).unwrap();
        std::fs::write(&path, format!("{line}\n{{\"kind\":\"tur\n{line}\n")).unwrap();
        assert_eq!(read(&path).unwrap(), vec![event.clone(), event]);
        assert!(read(&dir.path().join("none")).unwrap().is_empty());
    }
}

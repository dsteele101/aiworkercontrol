//! Filling the work history in for runs made before it was kept, from what
//! the item sidecars and the agent's log still hold.
//!
//! What can be known is thin, so every record made here says
//! [`backfilled`](crate::TurnRecord::backfilled) and leaves out what it can't
//! know: the metadata's turn count gives the runs, the result report the
//! provider, the pull request and (for a one-run item) the commits, and the
//! item's files' ages the times. The log of the item's latest run, if it is
//! still in its worktree directory, gives that run's model, tokens, cost and
//! tools.

use std::collections::HashSet;
use std::io::BufRead as _;
use std::path::Path;
use std::time::UNIX_EPOCH;

use agent_backend::{Cli, Provider};
use work_queue::{RESULT_SUFFIX, State as ItemState, WorkItem};

use super::Supervisor;
use super::trace::Trace;
use crate::history::{Entry, EventKind, ItemEvent, TurnOutcome, TurnReason, TurnRecord};
use crate::{ItemMeta, PoolError};

/// What the sidecars say about one item.
#[derive(Debug)]
struct Facts {
    state: ItemState,
    meta: ItemMeta,
    /// The result report, if there is one.
    report: Option<String>,
    /// The age of the item's own file: when it was queued, near enough.
    queued_at: Option<u64>,
    /// When the result report was last written: the end of the latest run.
    result_at: Option<u64>,
    /// When the metadata was last written: for a closed item, the merge.
    meta_at: Option<u64>,
    /// What the latest run's log holds.
    log: Option<Trace>,
}

impl Supervisor {
    /// Adds to the work history the runs of items in review, on hold and
    /// closed that it does not have yet, as far as their sidecars tell. Runs
    /// already in the history are left as they are, so this can be called
    /// whenever, and again. Returns how many runs it added.
    pub fn backfill_history(&self) -> Result<usize, PoolError> {
        let queue = self.queue();
        let existing = self.shared.ledger.load()?;
        let mut added = 0;
        for state in [ItemState::InReview, ItemState::Hold, ItemState::Closed] {
            for item in queue.list(state)? {
                let Ok(meta) = ItemMeta::load(queue, state, &item) else {
                    continue;
                };
                let report =
                    std::fs::read_to_string(queue.sidecar_path(state, &item, RESULT_SUFFIX)).ok();
                let log = report
                    .as_deref()
                    .and_then(|r| line_value(r, "- Log: "))
                    .zip(report.as_deref().and_then(provider_of).or(meta.provider))
                    .and_then(|(path, provider)| {
                        read_log(Path::new(path.trim_matches('`')), provider.cli())
                    });
                let facts = Facts {
                    state,
                    meta,
                    report,
                    queued_at: modified(&queue.path(state, &item)),
                    result_at: modified(&queue.sidecar_path(state, &item, RESULT_SUFFIX)),
                    meta_at: modified(&queue.sidecar_path(state, &item, work_queue::META_SUFFIX)),
                    log,
                };
                let known: Vec<&Entry> = existing
                    .iter()
                    .filter(|e| match e {
                        Entry::Turn(t) => t.project == item.project && t.item == item.file,
                        Entry::Event(e) => e.project == item.project && e.item == item.file,
                    })
                    .collect();
                for entry in plan(&item, &facts, &known) {
                    added += usize::from(matches!(entry, Entry::Turn(_)));
                    self.shared.ledger.append(&entry)?;
                }
            }
        }
        if added > 0 {
            self.shared.changed();
        }
        Ok(added)
    }
}

fn modified(path: &Path) -> Option<u64> {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
}

/// The agent's events in a log, as a trace. `None` if there is no log.
fn read_log(path: &Path, cli: Cli) -> Option<Trace> {
    let file = std::fs::File::open(path).ok()?;
    let mut trace = Trace::default();
    for line in std::io::BufReader::new(file).lines() {
        // A line that is not text is not an event either.
        let Ok(line) = line else { continue };
        for event in agent_backend::parse_line(cli, &line) {
            trace.observe(&event);
        }
    }
    Some(trace)
}

/// The text after `prefix` on the first line of `report` that has it.
fn line_value<'a>(report: &'a str, prefix: &str) -> Option<&'a str> {
    report
        .lines()
        .find_map(|l| l.strip_prefix(prefix))
        .map(str::trim)
}

fn provider_of(report: &str) -> Option<Provider> {
    let label = line_value(report, "- Provider: ")?;
    Provider::ALL.into_iter().find(|p| p.label() == label)
}

/// How many commits the report lists.
fn commits_in(report: &str) -> u32 {
    let listed = report
        .split("## Commits")
        .nth(1)
        .and_then(|rest| rest.split("\n## ").next())
        .unwrap_or_default();
    let count = listed.lines().filter(|l| l.starts_with("- ")).count();
    u32::try_from(count).unwrap_or(u32::MAX)
}

/// The entries to add for `item`: a run for each turn its metadata counts
/// that `known` entries do not have, and a merge if it was merged and the
/// history does not say so.
fn plan(item: &WorkItem, facts: &Facts, known: &[&Entry]) -> Vec<Entry> {
    let meta = &facts.meta;
    // A result report means at least one run, even if the count was reset.
    let turns = meta.turns.max(u32::from(facts.report.is_some()));
    let Some(provider) = facts
        .report
        .as_deref()
        .and_then(provider_of)
        .or(meta.provider)
    else {
        return Vec::new();
    };
    let present: HashSet<u32> = known
        .iter()
        .filter_map(|e| match e {
            Entry::Turn(t) => Some(t.turn),
            Entry::Event(_) => None,
        })
        .collect();
    let first_real = known
        .iter()
        .filter_map(|e| match e {
            Entry::Turn(t) if !t.backfilled => Some(t.started_at),
            _ => None,
        })
        .min();
    // The earliest moment the item is known to have existed, and never after
    // a run that was really recorded.
    let floor = facts.queued_at.or(facts.result_at).unwrap_or(0);
    let early = first_real.map_or(floor, |real| floor.min(real));
    let has_pr = meta.pr_url.is_some();
    let last_outcome = if has_pr {
        TurnOutcome::Review
    } else if facts.state == ItemState::Closed {
        TurnOutcome::Closed
    } else {
        TurnOutcome::Hold
    };

    let mut out = Vec::new();
    for turn in (1..=turns).filter(|t| !present.contains(t)) {
        let latest = turn == turns;
        let log = facts.log.as_ref().filter(|_| latest);
        let agent_ms = log.and_then(|l| l.metrics.duration_ms).unwrap_or(0);
        let ended_at = if latest {
            facts.result_at.unwrap_or(early).max(early)
        } else {
            early
        };
        let mut metrics = log.map(|l| l.metrics.clone()).unwrap_or_default();
        metrics.models.clear();
        out.push(Entry::Turn(Box::new(TurnRecord {
            project: item.project.clone(),
            item: item.file.clone(),
            turn,
            reason: if turn == 1 {
                TurnReason::First
            } else if has_pr {
                TurnReason::Rework
            } else {
                TurnReason::Retry
            },
            feedback: Vec::new(),
            provider,
            fallback: false,
            model_requested: None,
            models: log
                .map(|l| l.models.iter().cloned().collect())
                .unwrap_or_default(),
            effort: None,
            queued_at: None,
            // Never before the earlier runs, whatever the files' ages say.
            started_at: ended_at.saturating_sub(agent_ms / 1000).max(early),
            ended_at,
            duration_ms: agent_ms,
            agent_ms,
            outcome: if latest {
                last_outcome
            } else if has_pr {
                TurnOutcome::Review
            } else {
                TurnOutcome::Failed
            },
            failure: None,
            error: None,
            stalled: false,
            refunded: false,
            metrics,
            tool_calls: log.map(|l| l.tool_calls.clone()).unwrap_or_default(),
            // With one run, the branch's commits are all that run's.
            commits: match (&facts.report, turns) {
                (Some(report), 1) => commits_in(report),
                _ => 0,
            },
            lines_added: None,
            lines_removed: None,
            files_changed: None,
            quota_before: None,
            quota_after: None,
            backfilled: true,
        })));
    }

    let merged_known = known
        .iter()
        .any(|e| matches!(e, Entry::Event(e) if e.event == EventKind::Merged));
    if facts.state == ItemState::Closed
        && has_pr
        && !merged_known
        && let Some(at) = facts.meta_at.or(facts.result_at)
    {
        out.push(Entry::Event(ItemEvent {
            project: item.project.clone(),
            item: item.file.clone(),
            at,
            event: EventKind::Merged,
            detail: Some("backfilled".into()),
        }));
    }
    out
}

#[cfg(test)]
mod tests {
    // Test code asserts rather than propagating; see Rust code review.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]

    use agent_backend::RunMetrics;

    use super::*;

    const REPORT: &str = "# demo/a.txt\n\n- Project: demo\n- Provider: Anthropic (Claude)\n\
        - Log: `/wt/a.log`\n- Pull request: https://github.com/o/r/pull/1\n\n## Commits\n\n\
        - abc fix: a\n- def fix: b\n\n## Agent summary\n\nDone.\n";

    fn facts(state: ItemState, turns: u32, pr: bool) -> Facts {
        Facts {
            state,
            meta: ItemMeta {
                turns,
                pr_url: pr.then(|| "https://github.com/o/r/pull/1".to_string()),
                ..ItemMeta::default()
            },
            report: Some(REPORT.to_string()),
            queued_at: Some(1000),
            result_at: Some(5000),
            meta_at: Some(9000),
            log: None,
        }
    }

    fn turns(entries: &[Entry]) -> Vec<&TurnRecord> {
        entries
            .iter()
            .filter_map(|e| match e {
                Entry::Turn(t) => Some(&**t),
                Entry::Event(_) => None,
            })
            .collect()
    }

    #[test]
    fn a_merged_item_gets_its_turns_and_its_merge() {
        let item = WorkItem::new("demo", "a.txt");
        let mut f = facts(ItemState::Closed, 3, true);
        f.log = Some(Trace {
            metrics: RunMetrics {
                output_tokens: Some(9),
                duration_ms: Some(120_000),
                ..RunMetrics::default()
            },
            models: ["claude-x".to_string()].into(),
            ..Trace::default()
        });
        let entries = plan(&item, &f, &[]);
        let ts = turns(&entries);
        assert_eq!(ts.len(), 3);
        assert!(
            ts.iter()
                .all(|t| t.backfilled && t.provider == Provider::Anthropic)
        );
        assert_eq!(
            ts.iter().map(|t| t.reason).collect::<Vec<_>>(),
            [TurnReason::First, TurnReason::Rework, TurnReason::Rework]
        );
        // Only the latest run has a measured length, and the log's facts.
        assert_eq!((ts[0].duration_ms, ts[0].started_at), (0, 1000));
        let last = ts[2];
        assert_eq!(
            (last.ended_at, last.started_at, last.duration_ms),
            (5000, 4880, 120_000)
        );
        assert_eq!(last.models, vec!["claude-x"]);
        assert_eq!(last.metrics.output_tokens, Some(9));
        assert_eq!(last.outcome, TurnOutcome::Review);
        assert_eq!(ts[0].commits, 0);
        assert!(matches!(
            entries.last(),
            Some(Entry::Event(e)) if e.event == EventKind::Merged && e.at == 9000
        ));
    }

    #[test]
    fn a_single_run_counts_its_commits_and_a_pull_request_free_item_closes() {
        let item = WorkItem::new("demo", "a.txt");
        let entries = plan(&item, &facts(ItemState::Closed, 1, false), &[]);
        let [only] = turns(&entries)[..] else {
            panic!("one turn");
        };
        assert_eq!((only.commits, only.outcome), (2, TurnOutcome::Closed));
        // No pull request, so no merge to record.
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn only_what_the_history_lacks_is_added() {
        let item = WorkItem::new("demo", "a.txt");
        let first = plan(&item, &facts(ItemState::InReview, 2, true), &[]);
        assert_eq!(turns(&first).len(), 2);
        let known: Vec<&Entry> = first.iter().collect();
        // Nothing more on a second pass.
        assert!(plan(&item, &facts(ItemState::InReview, 2, true), &known).is_empty());
        // A third run, really recorded, is not copied; one missing earlier is.
        let real = Entry::Turn(Box::new(TurnRecord {
            turn: 3,
            started_at: 7000,
            backfilled: false,
            ..turns(&first)[0].clone()
        }));
        let only_real = [&real];
        let more = plan(&item, &facts(ItemState::InReview, 3, true), &only_real);
        assert_eq!(
            turns(&more).iter().map(|t| t.turn).collect::<Vec<_>>(),
            vec![1, 2]
        );
        // Placed before the real one even though its files are younger.
        let mut young = facts(ItemState::InReview, 3, true);
        young.queued_at = Some(8000);
        let more = plan(&item, &young, &only_real);
        assert!(turns(&more).iter().all(|t| t.started_at <= 7000));
    }

    #[test]
    fn an_item_whose_provider_is_unknown_is_left_alone() {
        let item = WorkItem::new("demo", "a.txt");
        let mut f = facts(ItemState::Closed, 1, true);
        f.report = None;
        assert!(plan(&item, &f, &[]).is_empty());
    }

    #[test]
    fn logs_give_models_tokens_and_tools() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("a.log");
        std::fs::write(
            &log,
            concat!(
                "{\"type\":\"system\",\"subtype\":\"init\",\"model\":\"claude-x\"}\n",
                "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"name\":\"Bash\",\"input\":{}}]}}\n",
                "{\"type\":\"result\",\"is_error\":false,\"result\":\"ok\",\"usage\":{\"output_tokens\":4},\"duration_ms\":50}\n",
                "\n--- stderr ---\nwarning\n"
            ),
        )
        .unwrap();
        let trace = read_log(&log, Cli::Claude).unwrap();
        assert_eq!(trace.models.len(), 1);
        assert_eq!(trace.tool_calls["Bash"], 1);
        assert_eq!(trace.metrics.output_tokens, Some(4));
        assert!(read_log(&dir.path().join("none"), Cli::Claude).is_none());
    }
}

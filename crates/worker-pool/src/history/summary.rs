//! One item's whole story, built from its ledger entries.

use std::collections::BTreeMap;

use agent_backend::{Provider, UsageWindow};
use serde::Serialize;

use super::{Entry, EventKind, TurnOutcome, TurnReason, TurnRecord};

/// Plan usage a provider's runs on an item took, in percentage points of
/// the provider's session and weekly allowances.
///
/// Providers report usage for the whole plan, not per model, and a run
/// sees the usage of every other run going on at the same time, so with
/// several workers on one provider these are upper bounds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct QuotaSpend {
    /// The provider.
    pub provider: Provider,
    /// Points of the session (short) window used.
    pub session_points: u32,
    /// Points of the weekly window used.
    pub weekly_points: u32,
}

/// Everything the ledger knows about one item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ItemSummary {
    /// The item's project.
    pub project: String,
    /// The item's file name.
    pub item: String,
    /// Where the item is now (`open`, `inprogress`, `inreview`, `closed`,
    /// `hold`), when it still exists in the queue.
    pub state: Option<String>,
    /// Its pull request, if it has one.
    pub pr_url: Option<String>,
    /// `merged` or `closed` once the item is finished.
    pub finished: Option<&'static str>,
    /// Agent runs on the item over its whole life.
    pub turns: u32,
    /// Runs that failed.
    pub failed_turns: u32,
    /// Runs that addressed feedback.
    pub rework_turns: u32,
    /// Runs on a fallback provider.
    pub fallback_turns: u32,
    /// Runs the watchdog stopped as stalled.
    pub stalled_turns: u32,
    /// Finished after a single run.
    pub first_pass: bool,
    /// Some of its runs were made up from its sidecars afterwards, so its
    /// times (counted from when it was queued) and sizes are only estimates.
    pub backfilled: bool,
    /// Providers used, in order of first use.
    pub providers: Vec<Provider>,
    /// Models the CLIs reported, in order of first use.
    pub models: Vec<String>,
    /// Effort levels requested.
    pub efforts: Vec<String>,
    /// When the item was queued, as far as known.
    pub queued_at: Option<u64>,
    /// When its first run started.
    pub started_at: u64,
    /// When it finished, if it has.
    pub finished_at: Option<u64>,
    /// From the first run's start to the finish, or to now if unfinished.
    pub elapsed_s: u64,
    /// Time agents spent running.
    pub active_s: u64,
    /// From being queued to the first run starting.
    pub queue_wait_s: Option<u64>,
    /// Time spent waiting in review, so far if it is waiting now.
    pub review_wait_s: u64,
    /// From the first push for review to the merge.
    pub time_to_merge_s: Option<u64>,
    /// Input tokens, as the CLIs count them.
    pub input_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
    /// Input tokens read from the prompt cache.
    pub cache_read_tokens: u64,
    /// Input tokens written to the prompt cache.
    pub cache_write_tokens: u64,
    /// Cost the CLIs reported, in millionths of a US dollar.
    pub cost_micro_usd: u64,
    /// Commits added.
    pub commits: u64,
    /// Lines added.
    pub lines_added: u64,
    /// Lines removed.
    pub lines_removed: u64,
    /// Files touched, summed over runs.
    pub files_changed: u64,
    /// Tool calls by tool.
    pub tool_calls: BTreeMap<String, u32>,
    /// Failed runs by kind of failure.
    pub failures: BTreeMap<String, u32>,
    /// Plan usage the runs took, per provider.
    pub quota: Vec<QuotaSpend>,
    /// Every run, oldest first.
    pub records: Vec<TurnRecord>,
}

/// Builds each item's summary from the ledger's `entries` (oldest first),
/// as of `now` (Unix seconds), most recently started first. An item with
/// events but no run yet is left out.
pub fn summarize(entries: &[Entry], now: u64) -> Vec<ItemSummary> {
    let mut by_item: BTreeMap<(&str, &str), Vec<&Entry>> = BTreeMap::new();
    for entry in entries {
        let key = match entry {
            Entry::Turn(t) => (t.project.as_str(), t.item.as_str()),
            Entry::Event(e) => (e.project.as_str(), e.item.as_str()),
        };
        by_item.entry(key).or_default().push(entry);
    }
    let mut out: Vec<ItemSummary> = by_item
        .into_iter()
        .filter_map(|((project, item), mut entries)| {
            // Entries added later, such as a backfill, can be older than
            // ones before them in the file.
            entries.sort_by_key(|e| match e {
                Entry::Turn(t) => t.started_at,
                Entry::Event(e) => e.at,
            });
            one(project, item, &entries, now)
        })
        .collect();
    out.sort_by_key(|s| std::cmp::Reverse(s.started_at));
    out
}

fn one(project: &str, item: &str, entries: &[&Entry], now: u64) -> Option<ItemSummary> {
    let mut records: Vec<TurnRecord> = Vec::new();
    let mut finished: Option<(&'static str, u64)> = None;
    let mut review_wait_s = 0;
    let mut waiting_since: Option<u64> = None;
    let mut first_review: Option<u64> = None;
    let mut merged_at = None;
    for entry in entries {
        match entry {
            Entry::Turn(t) => {
                if let Some(since) = waiting_since.take() {
                    review_wait_s += t.started_at.saturating_sub(since);
                }
                match t.outcome {
                    TurnOutcome::Review => {
                        waiting_since = Some(t.ended_at);
                        first_review.get_or_insert(t.ended_at);
                    }
                    TurnOutcome::Closed => finished = Some(("closed", t.ended_at)),
                    TurnOutcome::Hold | TurnOutcome::Failed => {}
                }
                records.push((**t).clone());
            }
            Entry::Event(e) => match e.event {
                EventKind::Merged => {
                    if let Some(since) = waiting_since.take() {
                        review_wait_s += e.at.saturating_sub(since);
                    }
                    finished = Some(("merged", e.at));
                    merged_at = Some(e.at);
                }
                EventKind::ClosedUnmerged => {
                    if let Some(since) = waiting_since.take() {
                        review_wait_s += e.at.saturating_sub(since);
                    }
                }
                EventKind::Resumed => finished = None,
                EventKind::Closed => finished = Some(("closed", e.at)),
                EventKind::Imported | EventKind::FeedbackQueued => {}
            },
        }
    }
    if finished.is_none()
        && let Some(since) = waiting_since
    {
        review_wait_s += now.saturating_sub(since);
    }
    let first = records.first()?;
    let started_at = first.started_at;
    let queued_at = records.iter().find_map(|r| r.queued_at);
    let end = finished.map_or(now, |(_, at)| at);

    let mut s = ItemSummary {
        project: project.to_string(),
        item: item.to_string(),
        state: None,
        pr_url: None,
        finished: finished.map(|(how, _)| how),
        turns: u32::try_from(records.len()).unwrap_or(u32::MAX),
        failed_turns: 0,
        rework_turns: 0,
        fallback_turns: 0,
        stalled_turns: 0,
        first_pass: finished.is_some() && records.len() == 1,
        backfilled: records.iter().any(|r| r.backfilled),
        providers: Vec::new(),
        models: Vec::new(),
        efforts: Vec::new(),
        queued_at,
        started_at,
        finished_at: finished.map(|(_, at)| at),
        elapsed_s: end.saturating_sub(started_at),
        active_s: 0,
        queue_wait_s: queued_at.map(|q| started_at.saturating_sub(q)),
        review_wait_s,
        time_to_merge_s: first_review
            .zip(merged_at)
            .map(|(pushed, merged)| merged.saturating_sub(pushed)),
        input_tokens: 0,
        output_tokens: 0,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        cost_micro_usd: 0,
        commits: 0,
        lines_added: 0,
        lines_removed: 0,
        files_changed: 0,
        tool_calls: BTreeMap::new(),
        failures: BTreeMap::new(),
        quota: Vec::new(),
        records: Vec::new(),
    };
    let mut active_ms = 0;
    for r in &records {
        s.failed_turns += u32::from(r.outcome == TurnOutcome::Failed);
        s.rework_turns += u32::from(r.reason == TurnReason::Rework);
        s.fallback_turns += u32::from(r.fallback);
        s.stalled_turns += u32::from(r.stalled);
        if !s.providers.contains(&r.provider) {
            s.providers.push(r.provider);
        }
        for model in &r.models {
            if !s.models.contains(model) {
                s.models.push(model.clone());
            }
        }
        if let Some(effort) = &r.effort
            && !s.efforts.contains(effort)
        {
            s.efforts.push(effort.clone());
        }
        active_ms += r.duration_ms;
        let m = &r.metrics;
        s.input_tokens += m.input_tokens.unwrap_or(0);
        s.output_tokens += m.output_tokens.unwrap_or(0);
        s.cache_read_tokens += m.cache_read_tokens.unwrap_or(0);
        s.cache_write_tokens += m.cache_write_tokens.unwrap_or(0);
        s.cost_micro_usd += m.cost_micro_usd.unwrap_or(0);
        s.commits += u64::from(r.commits);
        s.lines_added += r.lines_added.unwrap_or(0);
        s.lines_removed += r.lines_removed.unwrap_or(0);
        s.files_changed += r.files_changed.unwrap_or(0);
        for (tool, n) in &r.tool_calls {
            *s.tool_calls.entry(tool.clone()).or_default() += n;
        }
        if let Some(kind) = &r.failure {
            *s.failures.entry(kind.clone()).or_default() += 1;
        }
        let session = window_points(
            r.quota_before.as_ref().and_then(|u| u.session),
            r.quota_after.as_ref().and_then(|u| u.session),
        );
        let weekly = window_points(
            r.quota_before.as_ref().and_then(|u| u.weekly),
            r.quota_after.as_ref().and_then(|u| u.weekly),
        );
        match s.quota.iter_mut().find(|q| q.provider == r.provider) {
            Some(q) => {
                q.session_points += session;
                q.weekly_points += weekly;
            }
            None => s.quota.push(QuotaSpend {
                provider: r.provider,
                session_points: session,
                weekly_points: weekly,
            }),
        }
    }
    s.active_s = active_ms / 1000;
    s.records = records;
    Some(s)
}

/// Points a window's usage rose over a run. A window that reset during the
/// run (its reset time changed) counts what it has used since.
fn window_points(before: Option<UsageWindow>, after: Option<UsageWindow>) -> u32 {
    let (Some(before), Some(after)) = (before, after) else {
        return 0;
    };
    let reset = matches!((before.resets_at, after.resets_at), (Some(b), Some(a)) if a != b);
    if reset {
        u32::from(after.percent)
    } else {
        u32::from(after.percent.saturating_sub(before.percent))
    }
}

#[cfg(test)]
mod tests {
    // Test code asserts rather than propagating; see Rust code review.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]

    use agent_backend::{PlanUsage, RunMetrics};

    use super::*;
    use crate::history::ItemEvent;

    fn window(percent: u8, resets_at: u64) -> Option<UsageWindow> {
        Some(UsageWindow {
            percent,
            resets_at: Some(resets_at),
        })
    }

    fn turn(started_at: u64, secs: u64, outcome: TurnOutcome, reason: TurnReason) -> TurnRecord {
        TurnRecord {
            project: "p".into(),
            item: "a.txt".into(),
            turn: 1,
            reason,
            feedback: Vec::new(),
            provider: Provider::Anthropic,
            fallback: false,
            model_requested: None,
            model_reason: None,
            models: vec!["opus".into()],
            effort: Some("medium".into()),
            queued_at: (reason == TurnReason::First).then_some(started_at - 30),
            started_at,
            ended_at: started_at + secs,
            duration_ms: secs * 1000,
            agent_ms: secs * 900,
            outcome,
            failure: None,
            error: None,
            stalled: false,
            refunded: false,
            metrics: RunMetrics {
                input_tokens: Some(100),
                output_tokens: Some(10),
                ..RunMetrics::default()
            },
            tool_calls: BTreeMap::from([("Bash".to_string(), 2)]),
            commits: 1,
            lines_added: Some(5),
            lines_removed: Some(1),
            files_changed: Some(2),
            quota_before: Some(PlanUsage {
                session: window(10, 500),
                weekly: window(40, 9000),
            }),
            quota_after: Some(PlanUsage {
                session: window(14, 500),
                weekly: window(41, 9000),
            }),
            backfilled: false,
        }
    }

    fn event(at: u64, event: EventKind) -> Entry {
        Entry::Event(ItemEvent {
            project: "p".into(),
            item: "a.txt".into(),
            at,
            event,
            detail: None,
        })
    }

    #[test]
    fn a_merged_item_after_a_rework() {
        let entries = [
            Entry::Turn(Box::new(turn(
                100,
                60,
                TurnOutcome::Review,
                TurnReason::First,
            ))),
            event(200, EventKind::FeedbackQueued),
            Entry::Turn(Box::new(turn(
                300,
                40,
                TurnOutcome::Review,
                TurnReason::Rework,
            ))),
            event(1000, EventKind::Merged),
        ];
        let [s] = summarize(&entries, 5000).try_into().unwrap();
        assert_eq!(s.finished, Some("merged"));
        assert_eq!((s.turns, s.rework_turns, s.failed_turns), (2, 1, 0));
        assert!(!s.first_pass);
        // First run's start to the merge.
        assert_eq!(s.elapsed_s, 900);
        assert_eq!(s.active_s, 100);
        assert_eq!(s.queue_wait_s, Some(30));
        // 160 to 300 waiting on the first review, 340 to 1000 on the second.
        assert_eq!(s.review_wait_s, 140 + 660);
        assert_eq!(s.time_to_merge_s, Some(1000 - 160));
        assert_eq!((s.input_tokens, s.output_tokens), (200, 20));
        assert_eq!(s.models, vec!["opus"]);
        assert_eq!(s.tool_calls["Bash"], 4);
        assert_eq!(
            s.quota,
            vec![QuotaSpend {
                provider: Provider::Anthropic,
                session_points: 8,
                weekly_points: 2
            }]
        );
    }

    #[test]
    fn an_unfinished_item_counts_up_to_now() {
        let entries = [Entry::Turn(Box::new(turn(
            100,
            60,
            TurnOutcome::Review,
            TurnReason::First,
        )))];
        let [s] = summarize(&entries, 400).try_into().unwrap();
        assert_eq!(s.finished, None);
        assert_eq!(s.elapsed_s, 300);
        assert_eq!(s.review_wait_s, 240);
    }

    #[test]
    fn a_single_run_that_closes_is_a_first_pass_and_a_resume_reopens() {
        let closed = [Entry::Turn(Box::new(turn(
            100,
            10,
            TurnOutcome::Closed,
            TurnReason::First,
        )))];
        let [s] = summarize(&closed, 999).try_into().unwrap();
        assert!(s.first_pass);
        assert_eq!((s.finished, s.elapsed_s), (Some("closed"), 10));

        let resumed = [
            Entry::Turn(Box::new(turn(
                100,
                10,
                TurnOutcome::Hold,
                TurnReason::First,
            ))),
            event(200, EventKind::Resumed),
        ];
        let [s] = summarize(&resumed, 300).try_into().unwrap();
        assert_eq!(s.finished, None);
    }

    #[test]
    fn a_window_that_reset_counts_what_it_has_used_since() {
        assert_eq!(window_points(window(90, 1), window(3, 2)), 3);
        assert_eq!(window_points(window(10, 1), window(14, 1)), 4);
        assert_eq!(window_points(window(14, 1), window(10, 1)), 0);
        assert_eq!(window_points(None, window(10, 1)), 0);
    }

    #[test]
    fn events_without_a_run_are_left_out() {
        assert!(summarize(&[event(1, EventKind::Imported)], 10).is_empty());
    }
}

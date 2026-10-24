//! The pool end to end, with a fake `claude` CLI against a temporary repo.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use agent_backend::{Auth, PlanUsage, Provider, ProviderConfig, UsageWindow};
use work_queue::{Queue, State, WorkItem};
use worker_pool::{
    Activity, Author, Balancing, BranchOutcome, Cleanup, DEFAULT_STALL_AFTER,
    DEFAULT_SUGGESTION_RETENTION, NOTES_FILE, PoolConfig, PoolError, PoolSnapshot, Project,
    ProviderUsage, PullRequests, ReviewChange, Suggestion, SuggestionStatus, Supervisor,
    TurnOutcome, TurnReason, UsageLimits,
};

/// Stands in for `gh`: records its arguments, and answers `pr list`,
/// `pr create` and `pr view` the way the marker files beside it say.
const FAKE_GH: &str = r#"#!/bin/sh
here=$(dirname "$0")
echo "$@" >> "$here/gh.calls"
case "$1 $2" in
  "pr list") echo '[]' ;;
  "pr create")
    if [ -f "$here/gh.fail" ]; then echo 'boom from gh' >&2; exit 1; fi
    echo 'https://github.com/up/repo/pull/7' ;;
  "pr view")
    if [ -f "$here/gh.view.json" ]; then cat "$here/gh.view.json"
    elif [ -f "$here/gh.merged" ]; then echo '{"state":"MERGED","headRefOid":"","reviews":[]}'
    else echo '{"state":"OPEN","headRefOid":"","reviews":[]}'; fi ;;
  "pr checks")
    case "$*" in *--required*) echo 'no required checks reported on the branch' >&2; exit 1 ;; esac
    if [ -f "$here/gh.checks.json" ]; then cat "$here/gh.checks.json"; exit 1; fi
    echo 'no checks reported on the branch' >&2; exit 1 ;;
  "api --paginate")
    if [ -f "$here/gh.comments.json" ]; then echo "[$(cat "$here/gh.comments.json")]"; else echo '[[]]'; fi ;;
esac
"#;

/// Stands in for `claude -p …` with the prompt on stdin: commits a file in its working
/// directory and reports success, unless the prompt asks it to fail.
const FAKE_CLAUDE: &str = r#"#!/bin/sh
prompt=$(cat)
echo '{"type":"system","subtype":"init","model":"claude-test-1"}'
case "$prompt" in
  *"An AI coding agent failed a work item"*)
    echo '{"type":"result","is_error":false,"result":"```json\n{\"verdict\":\"suggest\",\"suggestion\":\"Run the tests before committing.\"}\n```"}'
    exit 0 ;;
  *"You suggested a lesson"*)
    case "$prompt" in
      *REVISE*) echo '{"type":"result","is_error":false,"result":"{\"verdict\":\"revise\",\"suggestion\":\"Run just ci before committing.\",\"reply\":\"Good point.\"}"}' ;;
      *WITHDRAW*) echo '{"type":"result","is_error":false,"result":"{\"verdict\":\"withdraw\",\"reply\":\"Too narrow.\"}"}' ;;
      *) echo '{"type":"result","is_error":false,"result":"{\"verdict\":\"keep\",\"reply\":\"It stands.\"}"}' ;;
    esac
    exit 0 ;;
  *NETWORK-FAIL*)
    echo '{"type":"result","is_error":true,"result":"API Error: connection reset by peer"}'
    exit 1 ;;
  *"<review-feedback>"*)
    printf '%s' "$prompt" > "$(dirname "$0")/rework-prompt.txt"
    case "$prompt" in *FAIL-REWORK*)
      echo '{"type":"result","is_error":true,"result":"rework failed"}'
      exit 1 ;;
    esac
    case "$prompt" in *ALREADY-ADDRESSED*)
      echo '{"type":"result","is_error":false,"result":"Nothing left to change"}'
      exit 0 ;;
    esac
    echo reviewed >> fixed.txt
    git add fixed.txt
    git -c user.name=t -c user.email=t@example.com commit -qm "fix: address review" >&2
    echo '{"type":"result","is_error":false,"result":"Addressed the review"}'
    exit 0 ;;
  *PLEASE-FAIL*)
    echo 'something went wrong' >&2
    echo '{"type":"result","is_error":true,"result":"could not do it"}'
    exit 1 ;;
  *NEEDS-DATA*)
    if [ ! -f data.json ]; then
      echo '{"type":"result","is_error":true,"result":"no data.json"}'
      exit 1
    fi
    echo "$CARGO_TARGET_DIR $CARGO_BUILD_JOBS" > cache.txt
    git add cache.txt
    git -c user.name=t -c user.email=t@example.com commit -qm "chore: record cache" >&2
    echo '{"type":"result","is_error":false,"result":"used the data"}'
    exit 0 ;;
  *LEAVES-CHILD*)
    sleep 600 &
    echo $! > "$AIWC_CHILD_PID_FILE"
    echo '{"type":"result","is_error":false,"result":"left a child"}'
    exit 0 ;;
  *NO-COMMIT*)
    echo '{"type":"result","is_error":false,"result":"looked around"}'
    exit 0 ;;
  *WAITS-IN-BACKGROUND*)
    echo '{"type":"system","subtype":"background_tasks_changed","tasks":[{"task_id":"b1","description":"Run the engine suite"}]}'
    echo '{"type":"system","subtype":"post_turn_summary","status_detail":"Fix written; waiting on the suite"}'
    sleep 1
    echo '{"type":"system","subtype":"background_tasks_changed","tasks":[]}' ;;
  *HANGS-AFTER-RESULT*)
    echo hung > fixed.txt
    git add fixed.txt
    git -c user.name=t -c user.email=t@example.com commit -qm "fix: hung after" >&2
    echo '{"type":"result","is_error":false,"result":"Done; waiting on the suite"}'
    # A wait that never ends, in a process group of its own, as Claude
    # Code runs background commands.
    setsid sh -c 'echo $$ > "$AIWC_CHILD_PID_FILE"; while :; do sleep 1; done' &
    wait
    exit 0 ;;
  *HANGS-BEFORE-RESULT*)
    while :; do sleep 1; done ;;
  *BUSY-SILENT*)
    end=$(( $(date +%s) + 4 ))
    while [ "$(date +%s)" -lt "$end" ]; do :; done ;;
  *CROSSES-LIMIT*)
    echo '{"type":"rate_limit_event","rate_limit_info":{"unifiedWindows":{"five_hour":{"utilization":0.9,"resetsAt":4000000000}}}}'
    while :; do sleep 1; done ;;
  *REPORTS-USAGE*)
    echo '{"type":"rate_limit_event","rate_limit_info":{"unifiedWindows":{"five_hour":{"utilization":0.5,"resetsAt":4000000000}}}}'
    echo '{"type":"rate_limit_event","rate_limit_info":{"unifiedWindows":{"five_hour":{"utilization":0.64,"resetsAt":4000000000},"seven_day":{"utilization":0.09,"resetsAt":4100000000}}}}' ;;
esac
draft=$(printf '%s\n' "$prompt" | sed -n 's/.* to \(\/[^:]*\/\.pr\/[^:]*\.md\):.*/\1/p' | head -n 1)
if [ -n "$draft" ]; then printf 'feat: fix the thing\n\nBody text.\n' > "$draft"; fi
echo '{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"git commit"}}]}}'
echo fixed > fixed.txt
git add fixed.txt
git -c user.name=t -c user.email=t@example.com commit -qm "fix: the thing" >&2
echo '{"type":"result","is_error":false,"result":"Fixed the thing","total_cost_usd":0.5,"usage":{"input_tokens":100,"output_tokens":20}}'
"#;

fn sh(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

struct Fixture {
    _dir: tempfile::TempDir,
    root: std::path::PathBuf,
    config: PoolConfig,
}

fn fixture(items: &[(&str, &str)], workers: usize) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();

    let repo = root.join("repo");
    fs::create_dir_all(&repo).unwrap();
    sh(&repo, &["init", "-q", "-b", "main"]);
    fs::write(repo.join("README"), "hi").unwrap();
    sh(&repo, &["add", "README"]);
    sh(
        &repo,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-qm",
            "init",
        ],
    );

    let spec = root.join("spec/demo");
    fs::create_dir_all(&spec).unwrap();
    fs::write(spec.join("SPEC.md"), "Be careful.").unwrap();

    for (file, body) in items {
        let path = root.join("work/open/demo").join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    let bin = root.join("bin");
    fs::create_dir_all(&bin).unwrap();
    for (name, script) in [("claude", FAKE_CLAUDE), ("gh", FAKE_GH)] {
        let fake = bin.join(name);
        fs::write(&fake, script).unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
    }

    let config = PoolConfig {
        workers,
        provider: ProviderConfig::new(Provider::Anthropic, Auth::Plan),
        projects: vec![Project::new("demo", &spec, &repo)],
        cli_dir: Some(bin),
        max_turns: 1,
        limits: UsageLimits::default(),
        fallbacks: Vec::new(),
        balancing: Balancing::default(),
        stall_after: DEFAULT_STALL_AFTER,
        suggestion_retention: DEFAULT_SUGGESTION_RETENTION,
    };
    Fixture {
        _dir: dir,
        root,
        config,
    }
}

async fn wait_for(pool: &Supervisor, done: impl Fn(&PoolSnapshot) -> bool) -> PoolSnapshot {
    let mut changes = pool.subscribe();
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let snapshot = pool.snapshot();
            if done(&snapshot) {
                return snapshot;
            }
            changes.changed().await.unwrap();
        }
    })
    .await
    .expect("pool did not reach the expected state")
}

/// Items on hold with the error of their last run, as `(file, error)`.
fn held(pool: &Supervisor) -> Vec<(String, String)> {
    let queue = pool.queue();
    queue
        .list(State::Hold)
        .unwrap()
        .into_iter()
        .map(|item| {
            let meta = pool.meta(State::Hold, &item).unwrap();
            (item.file, meta.last_error.unwrap_or_default())
        })
        .collect()
}

fn idle(s: &PoolSnapshot) -> bool {
    s.workers
        .iter()
        .all(|w| matches!(w.activity, Activity::Idle))
}

#[tokio::test(flavor = "multi_thread")]
async fn closes_successful_items_and_holds_failed_ones() {
    let fx = fixture(
        &[
            ("good.txt", "Fix the thing"),
            ("bad.txt", "PLEASE-FAIL"),
            ("idle.txt", "NO-COMMIT"),
        ],
        2,
    );
    let queue = Queue::new(fx.root.join("work"));
    let pool = Supervisor::new(queue.clone(), fx.config.clone()).unwrap();
    pool.start();

    wait_for(&pool, |s| {
        queue.list(State::Hold).unwrap().len() == 2 && idle(s)
    })
    .await;
    pool.stop();

    let good = WorkItem::new("demo", "good.txt");
    assert_eq!(queue.list(State::Closed).unwrap(), vec![good]);
    let result = fs::read_to_string(fx.root.join("work/closed/demo/good.result.md")).unwrap();
    assert!(result.contains("fix: the thing"), "{result}");
    assert!(result.contains("Fixed the thing"), "{result}");
    assert!(result.contains("`ai/good` (from `main`)"), "{result}");

    let repo = fx.root.join("repo");
    let worktree = fx.root.join("repo-worktrees/good");
    assert!(worktree.join("fixed.txt").is_file());
    sh(&repo, &["rev-parse", "--verify", "-q", "ai/good"]);
    assert!(
        !repo.join("fixed.txt").exists(),
        "user's checkout was touched"
    );
    assert!(fx.root.join("repo-worktrees/good.log").is_file());

    assert_eq!(
        held(&pool),
        vec![
            ("bad.txt".into(), "agent failed: could not do it".into()),
            (
                "idle.txt".into(),
                "agent finished without committing to ai/idle".into()
            ),
        ]
    );
    let bad = pool
        .meta(State::Hold, &WorkItem::new("demo", "bad.txt"))
        .unwrap();
    assert_eq!(bad.turns, 1);
    assert_eq!(
        bad.hold_reason.as_deref(),
        Some("used 1 of 1 turns; the last one failed: agent failed: could not do it")
    );
    assert!(queue.list(State::Open).unwrap().is_empty());

    let history = pool.history().unwrap();
    let summary = |file: &str| history.iter().find(|s| s.item == file).unwrap();
    let good = summary("good.txt");
    assert_eq!((good.turns, good.finished), (1, Some("closed")));
    assert_eq!(good.state.as_deref(), Some("closed"));
    assert!(good.first_pass);
    assert_eq!(good.models, vec!["claude-test-1"]);
    assert_eq!(good.providers, vec![Provider::Anthropic]);
    assert_eq!((good.input_tokens, good.output_tokens), (100, 20));
    assert_eq!(good.cost_micro_usd, 500_000);
    assert_eq!(
        (good.commits, good.lines_added, good.files_changed),
        (1, 1, 1)
    );
    assert_eq!(good.tool_calls["Bash"], 1);
    assert!(good.queued_at.is_some());
    let turn = &good.records[0];
    assert_eq!(turn.reason, TurnReason::First);
    assert_eq!(turn.outcome, TurnOutcome::Closed);
    assert!(turn.ended_at >= turn.started_at);

    let bad = summary("bad.txt");
    assert_eq!((bad.turns, bad.failed_turns, bad.finished), (1, 1, None));
    assert_eq!(bad.state.as_deref(), Some("hold"));
    assert_eq!(bad.failures["agent"], 1);
    assert_eq!(
        bad.records[0].error.as_deref(),
        Some("agent failed: could not do it")
    );
    assert_eq!(summary("idle.txt").failures["no_commits"], 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_run_that_hangs_after_its_work_is_stopped_and_kept() {
    let mut fx = fixture(&[("hang.txt", "HANGS-AFTER-RESULT")], 1);
    fx.config.stall_after = Duration::from_secs(2);
    let pid_file = record_child_pid(&fx);
    let queue = Queue::new(fx.root.join("work"));
    let pool = Supervisor::new(queue.clone(), fx.config.clone()).unwrap();
    pool.start();
    wait_for(&pool, |s| {
        queue.list(State::Closed).unwrap().len() == 1 && idle(s)
    })
    .await;
    pool.stop();

    let result = fs::read_to_string(fx.root.join("work/closed/demo/hang.result.md")).unwrap();
    assert!(result.contains("was stopped"), "{result}");
    assert!(result.contains("Done; waiting on the suite"), "{result}");
    let log = fs::read_to_string(fx.root.join("repo-worktrees/hang.log")).unwrap();
    assert!(log.contains("--- stopped: no output"), "{log}");
    // The wait loop, in its own process group, was stopped too.
    let child = fs::read_to_string(&pid_file).unwrap();
    assert!(
        !Path::new(&format!("/proc/{}", child.trim())).exists(),
        "background wait {child} still running"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_run_that_hangs_before_finishing_fails_as_stalled() {
    let mut fx = fixture(&[("hang.txt", "HANGS-BEFORE-RESULT")], 1);
    fx.config.stall_after = Duration::from_secs(2);
    let queue = Queue::new(fx.root.join("work"));
    let pool = Supervisor::new(queue.clone(), fx.config.clone()).unwrap();
    pool.start();
    wait_for(&pool, |s| {
        queue.list(State::Hold).unwrap().len() == 1 && idle(s)
    })
    .await;
    pool.stop();
    assert_eq!(
        held(&pool),
        vec![(
            "hang.txt".into(),
            "agent failed: stalled: no output and next to no CPU use for 0 minutes".into()
        )]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_silent_but_busy_run_is_left_alone() {
    let mut fx = fixture(&[("busy.txt", "BUSY-SILENT")], 1);
    fx.config.stall_after = Duration::from_secs(2);
    let queue = Queue::new(fx.root.join("work"));
    let pool = Supervisor::new(queue.clone(), fx.config.clone()).unwrap();
    pool.start();
    wait_for(&pool, |s| {
        queue.list(State::Closed).unwrap().len() == 1 && idle(s)
    })
    .await;
    pool.stop();
    let result = fs::read_to_string(fx.root.join("work/closed/demo/busy.result.md")).unwrap();
    assert!(!result.contains("was stopped"), "{result}");
}

#[tokio::test(flavor = "multi_thread")]
async fn workers_show_status_and_background_commands() {
    let fx = fixture(&[("wait.txt", "WAITS-IN-BACKGROUND")], 1);
    let queue = Queue::new(fx.root.join("work"));
    let pool = Supervisor::new(queue.clone(), fx.config.clone()).unwrap();
    pool.start();
    let job = |s: &PoolSnapshot| {
        s.workers.iter().find_map(|w| match &w.activity {
            Activity::Working(job) => Some(job.clone()),
            Activity::Idle => None,
        })
    };
    let snapshot = wait_for(&pool, |s| job(s).is_some_and(|j| j.status.is_some())).await;
    let waiting = job(&snapshot).unwrap();
    assert_eq!(waiting.background, vec!["Run the engine suite".to_string()]);
    assert!(waiting.background_since.is_some());
    assert_eq!(
        waiting.status.as_deref(),
        Some("Fix written; waiting on the suite")
    );
    // They describe the job rather than being steps of it.
    assert!(waiting.events.is_empty(), "{:?}", waiting.events);

    let snapshot = wait_for(&pool, |s| job(s).is_some_and(|j| j.background.is_empty())).await;
    assert_eq!(job(&snapshot).unwrap().background_since, None);
    wait_for(&pool, |s| {
        queue.list(State::Closed).unwrap().len() == 1 && idle(s)
    })
    .await;
    pool.stop();
}

#[tokio::test(flavor = "multi_thread")]
async fn reported_plan_usage_is_kept_across_restarts() {
    let fx = fixture(&[("usage.txt", "REPORTS-USAGE")], 1);
    let queue = Queue::new(fx.root.join("work"));
    let pool = Supervisor::new(queue.clone(), fx.config.clone()).unwrap();
    pool.start();
    wait_for(&pool, |s| {
        queue.list(State::Closed).unwrap().len() == 1 && idle(s)
    })
    .await;
    pool.stop();

    let expected = vec![ProviderUsage {
        provider: Provider::Anthropic,
        usage: PlanUsage {
            session: Some(UsageWindow {
                percent: 64,
                resets_at: Some(4_000_000_000),
            }),
            weekly: Some(UsageWindow {
                percent: 9,
                resets_at: Some(4_100_000_000),
            }),
        },
    }];
    assert_eq!(pool.snapshot().usage, expected);
    assert!(fx.root.join("work/usage.json").is_file());

    let restarted = Supervisor::new(queue, fx.config.clone()).unwrap();
    assert_eq!(restarted.snapshot().usage, expected);
}

/// Records Claude's plan as `percent` used in a window that resets far in
/// the future, as an earlier run would have.
fn record_claude_usage(fx: &Fixture, percent: u8) {
    fs::write(
        fx.root.join("work/usage.json"),
        format!(r#"{{"anthropic":{{"session":{{"percent":{percent},"resets_at":4000000000}}}}}}"#),
    )
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_level_pauses_workers_until_overridden() {
    let fx = fixture(&[("good.txt", "Fix the thing")], 1);
    record_claude_usage(&fx, 86);
    let queue = Queue::new(fx.root.join("work"));
    let pool = Supervisor::new(queue.clone(), fx.config.clone()).unwrap();
    pool.start();

    let snapshot = wait_for(&pool, |s| s.limited.is_some()).await;
    assert_eq!(
        snapshot.limited.as_deref(),
        Some("no provider can run: Anthropic (Claude) at 86% of its plan, past the 85% stop level")
    );
    assert_eq!(queue.list(State::Open).unwrap().len(), 1);

    pool.override_stop(Provider::Anthropic);
    wait_for(&pool, |s| {
        queue.list(State::Closed).unwrap().len() == 1 && idle(s)
    })
    .await;
    pool.stop();
    let snapshot = pool.snapshot();
    assert_eq!(snapshot.limited, None);
    assert_eq!(snapshot.overridden, vec![Provider::Anthropic]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_run_is_stopped_when_its_provider_goes_past_the_stop_level() {
    let fx = fixture(&[("busy.txt", "CROSSES-LIMIT")], 1);
    let queue = Queue::new(fx.root.join("work"));
    let pool = Supervisor::new(queue.clone(), fx.config.clone()).unwrap();
    pool.start();

    // The agent would hang forever; the limit stops it and frees the item.
    let snapshot = wait_for(&pool, |s| s.limited.is_some() && idle(s)).await;
    pool.stop();
    assert!(snapshot.limited.unwrap().contains("90% of its plan"));
    assert_eq!(queue.list(State::Open).unwrap().len(), 1);
    assert!(queue.list(State::Hold).unwrap().is_empty());
    let log = fs::read_to_string(fx.root.join("repo-worktrees/busy.log")).unwrap();
    assert!(log.contains("--- stopped: its provider went past"), "{log}");
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_level_fails_over_to_the_next_provider() {
    let mut fx = fixture(&[("good.txt", "Fix the thing")], 1);
    record_claude_usage(&fx, 90);
    // Deepseek runs through `claude` too, so the fake CLI serves it.
    fx.config.fallbacks = vec![ProviderConfig::new(
        Provider::Deepseek,
        Auth::ApiKey { key: "k".into() },
    )];
    let queue = Queue::new(fx.root.join("work"));
    let pool = Supervisor::new(queue.clone(), fx.config.clone()).unwrap();
    pool.start();
    wait_for(&pool, |s| {
        queue.list(State::Closed).unwrap().len() == 1 && idle(s)
    })
    .await;
    pool.stop();

    let result = fs::read_to_string(fx.root.join("work/closed/demo/good.result.md")).unwrap();
    assert!(result.contains("- Provider: Deepseek"), "{result}");
    assert_eq!(pool.snapshot().limited, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failing_fallback_is_suspended_without_costing_a_turn() {
    let mut fx = fixture(&[("bad.txt", "PLEASE-FAIL")], 1);
    record_claude_usage(&fx, 100);
    fx.config.fallbacks = vec![ProviderConfig::new(
        Provider::Deepseek,
        Auth::ApiKey { key: "k".into() },
    )];
    let queue = Queue::new(fx.root.join("work"));
    let bad = WorkItem::new("demo", "bad.txt");
    let pool = Supervisor::new(queue.clone(), fx.config.clone()).unwrap();
    pool.start();

    // Deepseek fails the run: it is set aside, and the item waits in
    // `open` with its turn given back instead of going on hold.
    let snapshot = wait_for(&pool, |s| s.limited.is_some() && idle(s)).await;
    assert_eq!(
        snapshot.suspended,
        vec![(Provider::Deepseek, "agent failed: could not do it".into())]
    );
    assert!(
        snapshot
            .limited
            .as_deref()
            .unwrap()
            .ends_with("Deepseek failed its last run"),
        "{:?}",
        snapshot.limited
    );
    assert_eq!(queue.list(State::Open).unwrap(), vec![bad.clone()]);
    let meta = pool.meta(State::Open, &bad).unwrap();
    assert_eq!(meta.turns, 0);
    assert_eq!(
        meta.last_error.as_deref(),
        Some("agent failed: could not do it")
    );

    // Claude runs again (here by override): the suspension lifts, and its
    // own failure counts as usual.
    pool.override_stop(Provider::Anthropic);
    wait_for(&pool, |s| {
        queue.list(State::Hold).unwrap().len() == 1 && idle(s)
    })
    .await;
    pool.stop();
    assert!(pool.snapshot().suspended.is_empty());
    assert_eq!(pool.meta(State::Hold, &bad).unwrap().turns, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn antigravity_usage_is_polled_between_runs() {
    let mut fx = fixture(&[], 1);
    fx.config.fallbacks = vec![ProviderConfig::new(Provider::Gemini, Auth::Plan)];
    // Stands in for `agy -p /usage --output-format json`, trimmed.
    let agy = fx.config.cli_dir.clone().unwrap().join("agy");
    fs::write(
        &agy,
        r#"#!/bin/sh
[ "$2" = "/usage" ] || exit 1
echo '{"status":"SUCCESS","command":{"name":"usage","data":{"groups":[{"name":"Gemini Models","buckets":[{"window":"weekly","remaining_fraction":0.25,"reset_time":"2096-10-23T21:59:39Z"},{"window":"5h","remaining_fraction":0.9,"reset_time":"2096-10-18T17:44:13Z"}]}]}}}'
"#,
    )
    .unwrap();
    fs::set_permissions(&agy, fs::Permissions::from_mode(0o755)).unwrap();
    let pool = Supervisor::new(Queue::new(fx.root.join("work")), fx.config.clone()).unwrap();

    assert!(pool.refresh_usage().await.is_empty());
    let usage = pool.snapshot().usage;
    assert_eq!(usage.len(), 1);
    assert_eq!(usage[0].provider, Provider::Gemini);
    assert_eq!(usage[0].usage.weekly.unwrap().percent, 75);
    assert_eq!(usage[0].usage.session.unwrap().percent, 10);

    fs::write(&agy, "#!/bin/sh\nexit 1\n").unwrap();
    let errors = pool.refresh_usage().await;
    assert_eq!(
        errors.iter().map(ToString::to_string).collect::<Vec<_>>(),
        vec!["could not read Gemini (Antigravity)'s plan usage: the report failed or timed out"]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn codex_usage_is_read_from_its_app_server() {
    let mut fx = fixture(&[], 1);
    fx.config.fallbacks = vec![ProviderConfig::new(Provider::OpenAi, Auth::Plan)];
    // Stands in for `codex app-server`: answers each request it is sent, and
    // like the real one stays up until stdin closes or it is killed.
    let codex = fx.config.cli_dir.clone().unwrap().join("codex");
    fs::write(
        &codex,
        r#"#!/bin/sh
[ "$1" = "app-server" ] || exit 1
while read -r line; do
  case "$line" in
    *'"id":1,'*) echo '{"id":1,"result":{}}' ;;
    *rateLimits*) echo '{"id":2,"result":{"rateLimits":{"primary":{"usedPercent":30,"windowDurationMins":300,"resetsAt":4000000000},"secondary":{"usedPercent":55,"windowDurationMins":10080,"resetsAt":4000100000}}}}' ;;
  esac
done
sleep 60
"#,
    )
    .unwrap();
    fs::set_permissions(&codex, fs::Permissions::from_mode(0o755)).unwrap();
    let pool = Supervisor::new(Queue::new(fx.root.join("work")), fx.config.clone()).unwrap();

    assert!(pool.refresh_usage().await.is_empty());
    let usage = pool.snapshot().usage;
    assert_eq!(usage.len(), 1);
    assert_eq!(usage[0].provider, Provider::OpenAi);
    assert_eq!(usage[0].usage.session.unwrap().percent, 30);
    assert_eq!(usage[0].usage.weekly.unwrap().percent, 55);
}

#[tokio::test(flavor = "multi_thread")]
async fn failures_are_retried_until_the_turns_run_out() {
    let mut fx = fixture(&[("bad.txt", "PLEASE-FAIL")], 1);
    fx.config.max_turns = 3;
    let queue = Queue::new(fx.root.join("work"));
    let pool = Supervisor::new(queue.clone(), fx.config.clone()).unwrap();
    pool.start();
    wait_for(&pool, |_| queue.list(State::Hold).unwrap().len() == 1).await;
    pool.stop();

    let meta = pool
        .meta(State::Hold, &WorkItem::new("demo", "bad.txt"))
        .unwrap();
    assert_eq!(meta.turns, 3);
    assert!(meta.hold_reason.unwrap().starts_with("used 3 of 3 turns"));
}

#[tokio::test(flavor = "multi_thread")]
async fn resuming_a_held_item_resets_its_turns() {
    let fx = fixture(&[("bad.txt", "PLEASE-FAIL")], 1);
    let queue = Queue::new(fx.root.join("work"));
    let pool = Supervisor::new(queue.clone(), fx.config.clone()).unwrap();
    pool.start();
    wait_for(&pool, |_| queue.list(State::Hold).unwrap().len() == 1).await;

    // Fix the item, then return it: it runs again in a fresh worktree.
    let item = WorkItem::new("demo", "bad.txt");
    fs::write(queue.path(State::Hold, &item), "now fine").unwrap();
    pool.resume(&item).await.unwrap();
    wait_for(&pool, |_| queue.list(State::Closed).unwrap().len() == 1).await;
    pool.stop();
    let meta = pool.meta(State::Closed, &item).unwrap();
    assert_eq!(meta.turns, 1, "turns restart from zero");
    assert_eq!(meta.last_error, None);
    assert_eq!(meta.hold_reason, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn only_held_items_can_be_resumed() {
    let fx = fixture(&[("a.txt", "Fix it")], 1);
    let pool = Supervisor::new(Queue::new(fx.root.join("work")), fx.config.clone()).unwrap();
    let err = pool
        .resume(&WorkItem::new("demo", "a.txt"))
        .await
        .unwrap_err();
    assert!(matches!(err, PoolError::NotOnHold(_)), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn resizing_spawns_and_retires_workers() {
    let fx = fixture(&[], 3);
    let pool = Supervisor::new(Queue::new(fx.root.join("work")), fx.config.clone()).unwrap();
    pool.start();
    let s = wait_for(&pool, |s| s.workers.len() == 3).await;
    assert_eq!(s.target_workers, 3);

    let mut smaller = fx.config.clone();
    smaller.workers = 1;
    pool.set_config(smaller);
    let s = wait_for(&pool, |s| s.workers.len() == 1).await;
    assert_eq!(s.workers[0].id, 0);

    pool.stop();
    wait_for(&pool, |s| s.workers.is_empty()).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn items_of_unknown_projects_stay_open() {
    let fx = fixture(&[], 1);
    let stray = fx.root.join("work/open/other/x.txt");
    fs::create_dir_all(stray.parent().unwrap()).unwrap();
    fs::write(&stray, "?").unwrap();
    let pool = Supervisor::new(Queue::new(fx.root.join("work")), fx.config.clone()).unwrap();
    pool.start();
    wait_for(&pool, |s| s.workers.len() == 1).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    pool.stop();
    assert!(stray.is_file());
}

#[tokio::test(flavor = "multi_thread")]
async fn new_recovers_orphaned_items() {
    let fx = fixture(&[], 1);
    let orphan = fx.root.join("work/inprogress/demo/o.txt");
    fs::create_dir_all(orphan.parent().unwrap()).unwrap();
    fs::write(&orphan, "").unwrap();
    let queue = Queue::new(fx.root.join("work"));
    Supervisor::new(queue.clone(), fx.config.clone()).unwrap();
    assert_eq!(
        queue.list(State::Open).unwrap(),
        vec![WorkItem::new("demo", "o.txt")]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn setup_runs_in_the_worktree_and_builds_share_a_cache() {
    let mut fx = fixture(&[("data.txt", "NEEDS-DATA")], 1);
    // An untracked, generated file: absent from a fresh worktree.
    fs::write(fx.root.join("repo/data.json"), "{}").unwrap();
    fx.config.projects[0].setup = Some(r#"cp "$AIWC_REPO/data.json" "$AIWC_WORKTREE/""#.into());
    let queue = Queue::new(fx.root.join("work"));
    let pool = Supervisor::new(queue.clone(), fx.config.clone()).unwrap();
    pool.start();
    wait_for(&pool, |_| {
        queue.list(State::Hold).unwrap().len() + queue.list(State::Closed).unwrap().len() == 1
    })
    .await;
    pool.stop();
    assert!(held(&pool).is_empty(), "{:?}", held(&pool));

    let worktrees = fx.root.join("repo-worktrees");
    let cache = fs::read_to_string(worktrees.join("data/cache.txt")).unwrap();
    assert_eq!(
        cache.trim(),
        format!("{} 4", worktrees.join(".target-1").display())
    );
    assert!(worktrees.join(".target-1").is_dir());
    let log = fs::read_to_string(worktrees.join("data.log")).unwrap();
    assert!(log.starts_with("--- setup: cp"), "{log}");
    assert!(log.contains(r#""result":"used the data""#), "{log}");
}

#[tokio::test(flavor = "multi_thread")]
async fn failing_setup_fails_the_item_before_the_agent_runs() {
    let mut fx = fixture(&[("a.txt", "Fix it")], 1);
    fx.config.projects[0].setup = Some("echo boom >&2; exit 3".into());
    let queue = Queue::new(fx.root.join("work"));
    let pool = Supervisor::new(queue.clone(), fx.config.clone()).unwrap();
    pool.start();
    wait_for(&pool, |_| queue.list(State::Hold).unwrap().len() == 1).await;
    pool.stop();

    let (_, reason) = &held(&pool)[0];
    assert!(reason.starts_with("setup command failed"), "{reason}");
    assert!(reason.ends_with("boom"), "{reason}");
    let log = fs::read_to_string(fx.root.join("repo-worktrees/a.log")).unwrap();
    assert!(!log.contains("system"), "agent ran: {log}");
}

/// Runs one successful item to `closed` and returns the pool, stopped.
async fn closed_item(fx: &Fixture) -> (Supervisor, WorkItem) {
    let queue = Queue::new(fx.root.join("work"));
    let pool = Supervisor::new(queue.clone(), fx.config.clone()).unwrap();
    pool.start();
    wait_for(&pool, |_| queue.list(State::Closed).unwrap().len() == 1).await;
    pool.stop();
    (pool, WorkItem::new("demo", "good.txt"))
}

fn branch_exists(repo: &Path, branch: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ])
        .status()
        .unwrap()
        .success()
}

#[tokio::test(flavor = "multi_thread")]
async fn merged_branches_are_cleaned_up_and_unmerged_ones_are_not() {
    let fx = fixture(&[("good.txt", "Fix it")], 1);
    let (pool, item) = closed_item(&fx).await;
    let repo = fx.root.join("repo");
    let worktree = fx.root.join("repo-worktrees/good");

    // Not merged yet: nothing to do.
    assert!(pool.clean_up_merged().await.unwrap().is_empty());
    assert!(worktree.is_dir());

    sh(&repo, &["merge", "-q", "--ff-only", "ai/good"]);
    assert_eq!(pool.clean_up_merged().await.unwrap(), vec![item]);
    assert!(!worktree.exists());
    assert!(!branch_exists(&repo, "ai/good"));
    assert!(
        fx.root.join("repo-worktrees/good.log").is_file(),
        "log is kept"
    );

    // Nothing left to clean.
    assert!(pool.clean_up_merged().await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn cherry_picked_branches_count_as_merged() {
    let fx = fixture(&[("good.txt", "Fix it")], 1);
    let (pool, _item) = closed_item(&fx).await;
    let repo = fx.root.join("repo");
    // A new commit on main first, so the pick cannot fast-forward.
    fs::write(repo.join("other"), "x").unwrap();
    sh(&repo, &["add", "other"]);
    sh(
        &repo,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-qm",
            "other",
        ],
    );
    sh(
        &repo,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "cherry-pick",
            "ai/good",
        ],
    );

    assert_eq!(pool.clean_up_merged().await.unwrap().len(), 1);
    assert!(!branch_exists(&repo, "ai/good"));
}

#[tokio::test(flavor = "multi_thread")]
async fn manual_clean_up_keeps_an_unmerged_branch() {
    let fx = fixture(&[("good.txt", "Fix it")], 1);
    let (pool, item) = closed_item(&fx).await;
    let repo = fx.root.join("repo");

    let done = pool.clean_up(&item).await.unwrap();
    assert_eq!(
        done,
        Cleanup {
            worktree_removed: true,
            branch: BranchOutcome::KeptUnmerged
        }
    );
    assert!(!fx.root.join("repo-worktrees/good").exists());
    assert!(
        branch_exists(&repo, "ai/good"),
        "unmerged work must survive"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn only_closed_items_can_be_cleaned_up() {
    let fx = fixture(&[("good.txt", "Fix it")], 1);
    let pool = Supervisor::new(Queue::new(fx.root.join("work")), fx.config.clone()).unwrap();
    let err = pool
        .clean_up(&WorkItem::new("demo", "good.txt"))
        .await
        .unwrap_err();
    assert!(matches!(err, PoolError::NotClosed(_)), "{err}");
}

/// Wraps the fake agent to tell it where to record the pid of a child it
/// leaves running, and returns that file.
fn record_child_pid(fx: &Fixture) -> std::path::PathBuf {
    let pid_file = fx.root.join("child.pid");
    let bin = fx.root.join("bin");
    let real = bin.join("claude-real");
    fs::rename(bin.join("claude"), &real).unwrap();
    fs::write(
        bin.join("claude"),
        format!(
            "#!/bin/sh\nAIWC_CHILD_PID_FILE={} exec {} \"$@\"\n",
            pid_file.display(),
            real.display()
        ),
    )
    .unwrap();
    fs::set_permissions(bin.join("claude"), fs::Permissions::from_mode(0o755)).unwrap();
    pid_file
}

#[tokio::test(flavor = "multi_thread")]
async fn background_processes_of_an_agent_are_stopped_with_it() {
    let fx = fixture(&[("child.txt", "LEAVES-CHILD")], 1);
    let pid_file = record_child_pid(&fx);

    let queue = Queue::new(fx.root.join("work"));
    let pool = Supervisor::new(queue.clone(), fx.config.clone()).unwrap();
    pool.start();
    // The agent commits nothing, so the item fails; the run has ended.
    wait_for(&pool, |_| queue.list(State::Hold).unwrap().len() == 1).await;
    pool.stop();

    let pid = fs::read_to_string(&pid_file).unwrap();
    let alive = Path::new("/proc").join(pid.trim()).join("stat");
    // A killed child may linger briefly as a zombie until reaped.
    let state = fs::read_to_string(&alive).unwrap_or_default();
    assert!(
        state.is_empty() || state.split_whitespace().nth(2) == Some("Z"),
        "background child {} still running: {state}",
        pid.trim()
    );
}

/// Publishes the fixture's project to a `fork` remote whose URL names the
/// GitHub owner `me` but whose pushes land in a local bare repo.
fn with_pull_requests(fx: &mut Fixture) -> std::path::PathBuf {
    let bare = fx.root.join("fork.git");
    let status = Command::new("git")
        .args(["init", "-q", "--bare"])
        .arg(&bare)
        .status()
        .unwrap();
    assert!(status.success());
    let repo = fx.root.join("repo");
    sh(
        &repo,
        &["remote", "add", "fork", "https://github.com/me/repo.git"],
    );
    sh(
        &repo,
        &[
            "remote",
            "set-url",
            "--push",
            "fork",
            &bare.display().to_string(),
        ],
    );
    fx.config.projects[0].pr = Some(PullRequests {
        push_remote: "fork".into(),
        repo: "up/repo".into(),
        base: "main".into(),
        draft: false,
    });
    bare
}

/// Runs one successful item of a publishing project into review and
/// returns the pool, stopped, with the item's pushed head.
async fn reviewed_item(fx: &Fixture) -> (Supervisor, WorkItem, String) {
    let queue = Queue::new(fx.root.join("work"));
    let pool = Supervisor::new(queue.clone(), fx.config.clone()).unwrap();
    pool.start();
    wait_for(&pool, |s| {
        queue.list(State::InReview).unwrap().len() == 1 && idle(s)
    })
    .await;
    pool.stop();
    let item = WorkItem::new("demo", "good.txt");
    let head = pool
        .meta(State::InReview, &item)
        .unwrap()
        .pushed_head
        .unwrap();
    (pool, item, head)
}

fn git_out(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn published_work_waits_in_review_with_its_pull_request() {
    let mut fx = fixture(&[("good.txt", "Fix it")], 1);
    let bare = with_pull_requests(&mut fx);
    let (pool, item, head) = reviewed_item(&fx).await;

    let meta = pool.meta(State::InReview, &item).unwrap();
    assert_eq!(
        meta.pr_url.as_deref(),
        Some("https://github.com/up/repo/pull/7")
    );
    assert_eq!(meta.pr_title.as_deref(), Some("feat: fix the thing"));
    assert_eq!(meta.turns, 1);
    assert_eq!(
        head,
        git_out(&fx.root.join("repo"), &["rev-parse", "ai/good"])
    );
    assert_eq!(
        pool.pull_request(State::InReview, &item).as_deref(),
        Some("https://github.com/up/repo/pull/7")
    );
    let result = fs::read_to_string(fx.root.join("work/inreview/demo/good.result.md")).unwrap();
    assert!(
        result.contains("- Pull request: https://github.com/up/repo/pull/7"),
        "{result}"
    );

    let calls = fs::read_to_string(fx.root.join("bin/gh.calls")).unwrap();
    assert!(
        calls.contains(
            "pr create --repo up/repo --base main --head me:ai/good \
             --title feat: fix the thing --body Body text."
        ),
        "{calls}"
    );
    assert!(!calls.contains("--draft"), "{calls}");
    let pushed = Command::new("git")
        .arg("--git-dir")
        .arg(&bare)
        .args(["show-ref", "--verify", "--quiet", "refs/heads/ai/good"])
        .status()
        .unwrap();
    assert!(pushed.success(), "branch was not pushed");
    // The draft lives outside the worktree, so it was never committed.
    let tracked = git_out(&fx.root.join("repo-worktrees/good"), &["ls-files"]);
    assert!(!tracked.contains(".pr"));
}

#[tokio::test(flavor = "multi_thread")]
async fn unpublished_work_goes_on_hold() {
    let mut fx = fixture(&[("good.txt", "Fix it")], 1);
    with_pull_requests(&mut fx);
    fs::write(fx.root.join("bin/gh.fail"), "").unwrap();
    let queue = Queue::new(fx.root.join("work"));
    let pool = Supervisor::new(queue.clone(), fx.config.clone()).unwrap();
    pool.start();
    wait_for(&pool, |_| queue.list(State::Hold).unwrap().len() == 1).await;
    pool.stop();

    let item = WorkItem::new("demo", "good.txt");
    let reason = pool.meta(State::Hold, &item).unwrap().hold_reason.unwrap();
    assert!(
        reason.starts_with("its work is committed, but was not published"),
        "{reason}"
    );
    assert!(reason.contains("boom from gh"), "{reason}");
    let result = fs::read_to_string(fx.root.join("work/hold/demo/good.result.md")).unwrap();
    assert!(result.contains("- Not published: "), "{result}");
}

#[tokio::test(flavor = "multi_thread")]
async fn requested_changes_are_reworked_in_the_same_worktree() {
    let mut fx = fixture(&[("good.txt", "Fix it")], 1);
    let bare = with_pull_requests(&mut fx);
    let (pool, item, head) = reviewed_item(&fx).await;
    let bin = fx.root.join("bin");
    fs::write(
        bin.join("gh.view.json"),
        format!(
            r#"{{"state":"OPEN","headRefOid":"{head}","reviews":[
                {{"state":"COMMENTED","author":{{"login":"bot"}},"body":"Old note.","commit":{{"oid":"0000"}}}},
                {{"state":"CHANGES_REQUESTED","author":{{"login":"maint"}},"body":"Please rename it.","commit":{{"oid":"{head}"}}}}]}}"#
        ),
    )
    .unwrap();
    fs::write(
        bin.join("gh.comments.json"),
        format!(
            r#"[{{"commit_id":"{head}","path":"fixed.txt","line":1,"body":"Use a better word","user":{{"login":"maint"}}}},
                {{"commit_id":"0000","path":"old.txt","line":9,"body":"stale","user":{{"login":"maint"}}}}]"#
        ),
    )
    .unwrap();

    let changes = pool.check_reviews().await.unwrap();
    assert_eq!(
        changes,
        vec![(item.clone(), ReviewChange::ChangesRequested)]
    );
    pool.start();
    let queue = pool.queue().clone();
    wait_for(&pool, |s| {
        idle(s)
            && queue.list(State::InReview).unwrap().len() == 1
            && pool.meta(State::InReview, &item).unwrap().turns == 2
    })
    .await;
    pool.stop();

    let prompt = fs::read_to_string(bin.join("rework-prompt.txt")).unwrap();
    assert!(
        prompt.contains("pull request https://github.com/up/repo/pull/7"),
        "{prompt}"
    );
    assert!(prompt.contains("Please rename it."), "{prompt}");
    assert!(
        prompt.contains("`fixed.txt:1` (maint): Use a better word"),
        "{prompt}"
    );
    assert!(
        !prompt.contains("Old note.") && !prompt.contains("stale"),
        "{prompt}"
    );

    let worktree = fx.root.join("repo-worktrees/good");
    let log = git_out(&worktree, &["log", "--format=%s", "main..HEAD"]);
    assert_eq!(
        log, "fix: address review\nfix: the thing",
        "kept the first commit"
    );
    let meta = pool.meta(State::InReview, &item).unwrap();
    let new_head = git_out(&worktree, &["rev-parse", "HEAD"]);
    assert_eq!(meta.pushed_head.as_deref(), Some(new_head.as_str()));
    assert_eq!(meta.answered_head.as_deref(), Some(head.as_str()));
    assert_eq!(meta.needs_rework, None);
    let pushed = Command::new("git")
        .arg("--git-dir")
        .arg(&bare)
        .args(["rev-parse", "ai/good"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&pushed.stdout).trim(), new_head);

    // The same feedback on the same commit is not acted on twice.
    assert!(pool.check_reviews().await.unwrap().is_empty());

    let [summary] = pool.history().unwrap().try_into().unwrap();
    assert_eq!((summary.turns, summary.rework_turns), (2, 1));
    assert_eq!(summary.state.as_deref(), Some("inreview"));
    assert_eq!(
        summary.pr_url.as_deref(),
        Some("https://github.com/up/repo/pull/7")
    );
    let [first, rework] = &summary.records[..] else {
        panic!("expected two turns");
    };
    assert_eq!(
        (first.reason, first.outcome),
        (TurnReason::First, TurnOutcome::Review)
    );
    assert_eq!(
        (rework.reason, rework.outcome),
        (TurnReason::Rework, TurnOutcome::Review)
    );
    assert_eq!(rework.feedback, vec!["review", "comments"]);
    assert_eq!(rework.turn, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn one_item_can_be_checked_on_demand() {
    let mut fx = fixture(&[("good.txt", "Fix it")], 1);
    with_pull_requests(&mut fx);
    let (pool, item, head) = reviewed_item(&fx).await;
    let bin = fx.root.join("bin");

    assert_eq!(pool.check_review(&item).await.unwrap(), None);
    fs::write(
        bin.join("gh.view.json"),
        format!(
            r#"{{"state":"OPEN","headRefOid":"{head}","reviews":[
                {{"state":"CHANGES_REQUESTED","author":{{"login":"maint"}},"body":"Rename it.","commit":{{"oid":"{head}"}}}}]}}"#
        ),
    )
    .unwrap();
    assert_eq!(
        pool.check_review(&item).await.unwrap(),
        Some(ReviewChange::ChangesRequested)
    );
    let meta = pool.meta(State::InReview, &item).unwrap();
    assert!(meta.needs_rework.unwrap().contains("Rename it."));

    let other = WorkItem::new("demo", "missing.txt");
    assert!(matches!(
        pool.check_review(&other).await,
        Err(PoolError::NotInReview(_))
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_merge_conflict_queues_a_rework_to_resolve_it() {
    let mut fx = fixture(&[("good.txt", "Fix it")], 1);
    with_pull_requests(&mut fx);
    let (pool, item, head) = reviewed_item(&fx).await;
    fs::write(
        fx.root.join("bin/gh.view.json"),
        format!(
            r#"{{"state":"OPEN","headRefOid":"{head}","reviews":[],
                "mergeable":"CONFLICTING","baseRefName":"main"}}"#
        ),
    )
    .unwrap();
    assert_eq!(
        pool.check_review(&item).await.unwrap(),
        Some(ReviewChange::ChangesRequested)
    );
    let feedback = pool
        .meta(State::InReview, &item)
        .unwrap()
        .needs_rework
        .unwrap();
    assert!(feedback.contains("### Merge conflicts"), "{feedback}");
    assert!(feedback.contains("`main`"), "{feedback}");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_on_demand_check_acts_on_inline_comments_but_the_watcher_does_not() {
    let mut fx = fixture(&[("good.txt", "Fix it")], 1);
    with_pull_requests(&mut fx);
    let (pool, item, head) = reviewed_item(&fx).await;
    let bin = fx.root.join("bin");
    // A bot's comment on the latest commit; the review that brought it is
    // only COMMENTED, and the earlier request for changes was on an old commit.
    fs::write(
        bin.join("gh.view.json"),
        format!(
            r#"{{"state":"OPEN","headRefOid":"{head}","reviews":[
                {{"state":"CHANGES_REQUESTED","author":{{"login":"maint"}},"body":"Old.","commit":{{"oid":"0000"}}}},
                {{"state":"COMMENTED","author":{{"login":"bot"}},"body":"","commit":{{"oid":"{head}"}}}}]}}"#
        ),
    )
    .unwrap();
    fs::write(
        bin.join("gh.comments.json"),
        format!(
            r#"[{{"commit_id":"{head}","path":"fixed.txt","line":1,"body":"Handle the empty case","user":{{"login":"bot"}}}}]"#
        ),
    )
    .unwrap();

    assert!(pool.check_reviews().await.unwrap().is_empty());
    assert_eq!(
        pool.check_review(&item).await.unwrap(),
        Some(ReviewChange::ChangesRequested)
    );
    let meta = pool.meta(State::InReview, &item).unwrap();
    assert!(meta.needs_rework.unwrap().contains("Handle the empty case"));
}

#[tokio::test(flavor = "multi_thread")]
async fn bumping_pushes_an_empty_commit_to_the_pull_request() {
    let mut fx = fixture(&[("good.txt", "Fix it")], 1);
    let bare = with_pull_requests(&mut fx);
    let (pool, item, head) = reviewed_item(&fx).await;
    let worktree = fx.root.join("repo-worktrees/good");
    sh(&worktree, &["config", "user.name", "t"]);
    sh(&worktree, &["config", "user.email", "t@example.com"]);

    pool.bump(&item).await.unwrap();

    let new_head = git_out(&worktree, &["rev-parse", "HEAD"]);
    assert_ne!(new_head, head);
    assert_eq!(git_out(&worktree, &["rev-parse", "HEAD^"]), head);
    assert_eq!(
        git_out(&worktree, &["log", "-1", "--format=%s"]),
        "chore: re-run checks"
    );
    let pushed = Command::new("git")
        .arg("--git-dir")
        .arg(&bare)
        .args(["rev-parse", "ai/good"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&pushed.stdout).trim(), new_head);
    let meta = pool.meta(State::InReview, &item).unwrap();
    assert_eq!(meta.pushed_head.as_deref(), Some(new_head.as_str()));
}

#[tokio::test(flavor = "multi_thread")]
async fn bumping_waits_while_feedback_is_queued() {
    let mut fx = fixture(&[("good.txt", "Fix it")], 1);
    with_pull_requests(&mut fx);
    let (pool, item, head) = reviewed_item(&fx).await;
    fs::write(
        fx.root.join("bin/gh.view.json"),
        format!(
            r#"{{"state":"OPEN","headRefOid":"{head}","reviews":[
                {{"state":"CHANGES_REQUESTED","author":{{"login":"maint"}},"body":"Rename it.","commit":{{"oid":"{head}"}}}}]}}"#
        ),
    )
    .unwrap();
    pool.check_review(&item).await.unwrap();
    assert!(matches!(
        pool.bump(&item).await,
        Err(PoolError::ReworkQueued(_))
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn comments_that_need_no_change_send_the_item_back_to_review() {
    let mut fx = fixture(&[("good.txt", "Fix it")], 1);
    with_pull_requests(&mut fx);
    let (pool, item, head) = reviewed_item(&fx).await;
    let bin = fx.root.join("bin");
    fs::write(
        bin.join("gh.view.json"),
        format!(r#"{{"state":"OPEN","headRefOid":"{head}","reviews":[]}}"#),
    )
    .unwrap();
    fs::write(
        bin.join("gh.comments.json"),
        format!(
            r#"[{{"commit_id":"{head}","path":"fixed.txt","line":1,"body":"ALREADY-ADDRESSED","user":{{"login":"bot"}}}}]"#
        ),
    )
    .unwrap();

    assert_eq!(
        pool.check_review(&item).await.unwrap(),
        Some(ReviewChange::ChangesRequested)
    );
    pool.start();
    let queue = pool.queue().clone();
    wait_for(&pool, |s| {
        idle(s)
            && queue.list(State::InReview).unwrap().len() == 1
            && pool.meta(State::InReview, &item).unwrap().turns == 2
    })
    .await;
    pool.stop();

    let meta = pool.meta(State::InReview, &item).unwrap();
    assert_eq!(meta.pushed_head.as_deref(), Some(head.as_str()));
    assert_eq!(meta.needs_rework, None);
    assert_eq!(meta.last_error, None);
    // The answered head keeps the same comments from queueing another turn.
    assert_eq!(pool.check_review(&item).await.unwrap(), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn reworking_again_acts_on_feedback_a_rework_left_standing() {
    let mut fx = fixture(&[("good.txt", "Fix it")], 1);
    with_pull_requests(&mut fx);
    let (pool, item, head) = reviewed_item(&fx).await;
    let bin = fx.root.join("bin");
    fs::write(
        bin.join("gh.view.json"),
        format!(r#"{{"state":"OPEN","headRefOid":"{head}","reviews":[]}}"#),
    )
    .unwrap();
    fs::write(
        bin.join("gh.comments.json"),
        format!(
            r#"[{{"commit_id":"{head}","path":"fixed.txt","line":1,"body":"ALREADY-ADDRESSED","user":{{"login":"bot"}}}}]"#
        ),
    )
    .unwrap();

    assert_eq!(
        pool.check_review(&item).await.unwrap(),
        Some(ReviewChange::ChangesRequested)
    );
    pool.start();
    let queue = pool.queue().clone();
    wait_for(&pool, |s| {
        idle(s)
            && queue.list(State::InReview).unwrap().len() == 1
            && pool.meta(State::InReview, &item).unwrap().turns == 2
    })
    .await;
    pool.stop();

    let meta = pool.meta(State::InReview, &item).unwrap();
    assert_eq!(meta.pushed_head.as_deref(), Some(head.as_str()));
    assert_eq!(meta.needs_rework, None);
    assert_eq!(meta.last_error, None);
    // The answered head keeps the same comments from queueing another turn.
    assert_eq!(pool.check_review(&item).await.unwrap(), None);

    // Reworking again is the way past it.
    assert_eq!(
        pool.rework_again(&item).await.unwrap(),
        Some(ReviewChange::ChangesRequested)
    );
    assert!(
        pool.meta(State::InReview, &item)
            .unwrap()
            .needs_rework
            .is_some()
    );
    assert!(matches!(
        pool.rework_again(&item).await,
        Err(PoolError::ReworkQueued(_))
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_request_for_changes_after_a_rework_that_changed_nothing_is_acted_on() {
    let mut fx = fixture(&[("good.txt", "Fix it")], 1);
    with_pull_requests(&mut fx);
    let (pool, item, head) = reviewed_item(&fx).await;
    let bin = fx.root.join("bin");
    let view = format!(r#"{{"state":"OPEN","headRefOid":"{head}","reviews":[]}}"#);
    fs::write(bin.join("gh.view.json"), view).unwrap();
    // Inline comments the agent finds nothing to change for.
    fs::write(
        bin.join("gh.comments.json"),
        format!(
            r#"[{{"commit_id":"{head}","path":"fixed.txt","line":1,"body":"ALREADY-ADDRESSED","user":{{"login":"bot"}}}}]"#
        ),
    )
    .unwrap();
    assert_eq!(
        pool.check_review(&item).await.unwrap(),
        Some(ReviewChange::ChangesRequested)
    );
    pool.start();
    let queue = pool.queue().clone();
    wait_for(&pool, |s| {
        idle(s)
            && queue.list(State::InReview).unwrap().len() == 1
            && pool.meta(State::InReview, &item).unwrap().turns == 2
    })
    .await;
    pool.stop();
    assert!(pool.check_reviews().await.unwrap().is_empty());

    // A reviewer then requests changes on the same commit.
    fs::write(
        bin.join("gh.view.json"),
        format!(
            r#"{{"state":"OPEN","headRefOid":"{head}","reviews":[
                {{"state":"CHANGES_REQUESTED","author":{{"login":"maint"}},"body":"Rename it.","commit":{{"oid":"{head}"}},"submittedAt":"2026-01-01T10:00:00Z"}}]}}"#
        ),
    )
    .unwrap();
    assert_eq!(
        pool.check_reviews().await.unwrap(),
        vec![(item.clone(), ReviewChange::ChangesRequested)]
    );
    let meta = pool.meta(State::InReview, &item).unwrap();
    assert_eq!(
        meta.answered_review_at.as_deref(),
        Some("2026-01-01T10:00:00Z")
    );
    // Once queued, the same request is not queued again.
    assert!(pool.check_reviews().await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rework_kept_to_its_provider_waits_for_it() {
    let mut fx = fixture(&[("good.txt", "Fix it")], 1);
    with_pull_requests(&mut fx);
    fx.config.max_turns = 3;
    let (pool, item, _) = reviewed_item(&fx).await;
    let mut meta = pool.meta(State::InReview, &item).unwrap();
    assert_eq!(meta.provider, Some(Provider::Anthropic));
    meta.needs_rework = Some("Please rename it.".into());
    let meta_path = fx.root.join("work/inreview/demo/good.meta.json");
    fs::write(&meta_path, serde_json::to_string(&meta).unwrap()).unwrap();
    drop(pool);

    // Claude is stopped; Deepseek could run, but the rework is Claude's.
    record_claude_usage(&fx, 90);
    let mut config = fx.config.clone();
    config.fallbacks = vec![ProviderConfig::new(
        Provider::Deepseek,
        Auth::ApiKey { key: "k".into() },
    )];
    config.balancing.rework_any_provider = false;
    let pool = Supervisor::new(Queue::new(fx.root.join("work")), config).unwrap();
    pool.start();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let waiting = pool.meta(State::InReview, &item).unwrap();
    assert_eq!(waiting.needs_rework.as_deref(), Some("Please rename it."));
    assert_eq!(waiting.turns, 1);

    pool.override_stop(Provider::Anthropic);
    wait_for(&pool, |s| {
        idle(s)
            && pool
                .meta(State::InReview, &item)
                .is_ok_and(|m| m.needs_rework.is_none())
    })
    .await;
    pool.stop();
    let reworked = pool.meta(State::InReview, &item).unwrap();
    assert_eq!(reworked.provider, Some(Provider::Anthropic));
    assert_eq!(reworked.turns, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn failing_ci_queues_a_rework() {
    let mut fx = fixture(&[("good.txt", "Fix it")], 1);
    with_pull_requests(&mut fx);
    let (pool, item, head) = reviewed_item(&fx).await;
    let bin = fx.root.join("bin");
    fs::write(
        bin.join("gh.view.json"),
        format!(r#"{{"state":"OPEN","headRefOid":"{head}","reviews":[]}}"#),
    )
    .unwrap();
    fs::write(
        bin.join("gh.checks.json"),
        r#"[{"name":"lint","bucket":"pass","link":"https://ci/0"},{"name":"build","bucket":"fail","link":"https://ci/1"}]"#,
    )
    .unwrap();

    let changes = pool.check_reviews().await.unwrap();
    assert_eq!(
        changes,
        vec![(item.clone(), ReviewChange::ChangesRequested)]
    );
    let feedback = pool
        .meta(State::InReview, &item)
        .unwrap()
        .needs_rework
        .unwrap();
    assert!(feedback.contains("- build: https://ci/1"), "{feedback}");
    assert!(!feedback.contains("lint"), "{feedback}");
}

#[tokio::test(flavor = "multi_thread")]
async fn merged_pull_requests_close_and_closed_ones_hold() {
    let mut fx = fixture(&[("good.txt", "Fix it")], 1);
    with_pull_requests(&mut fx);
    let (pool, item, _) = reviewed_item(&fx).await;
    let bin = fx.root.join("bin");

    fs::write(
        bin.join("gh.view.json"),
        r#"{"state":"CLOSED","headRefOid":"","reviews":[]}"#,
    )
    .unwrap();
    assert_eq!(
        pool.check_reviews().await.unwrap(),
        vec![(item.clone(), ReviewChange::ClosedUnmerged)]
    );
    let reason = pool.meta(State::Hold, &item).unwrap().hold_reason.unwrap();
    assert!(reason.contains("closed without merging"), "{reason}");

    // Returned while its pull request is closed: it starts over.
    pool.resume(&item).await.unwrap();
    assert_eq!(pool.queue().list(State::Open).unwrap(), vec![item.clone()]);
    assert_eq!(pool.meta(State::Open, &item).unwrap(), Default::default());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_merged_pull_request_closes_the_item_and_frees_the_worktree() {
    let mut fx = fixture(&[("good.txt", "Fix it")], 1);
    with_pull_requests(&mut fx);
    let (pool, item, _) = reviewed_item(&fx).await;

    // Open, and not in main: nothing to clean.
    assert!(pool.check_reviews().await.unwrap().is_empty());
    assert!(pool.clean_up_merged().await.unwrap().is_empty());
    // Squash-merged upstream: git cannot tell, the pull request can.
    fs::write(fx.root.join("bin/gh.merged"), "").unwrap();
    assert_eq!(
        pool.check_reviews().await.unwrap(),
        vec![(item.clone(), ReviewChange::Merged)]
    );
    assert_eq!(
        pool.queue().list(State::Closed).unwrap(),
        vec![item.clone()]
    );
    assert_eq!(pool.clean_up_merged().await.unwrap(), vec![item]);
    assert!(!fx.root.join("repo-worktrees/good").exists());
    assert!(!branch_exists(&fx.root.join("repo"), "ai/good"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failing_rework_retries_from_review_then_holds_and_resumes_there() {
    let mut fx = fixture(&[("good.txt", "Fix it")], 1);
    fx.config.max_turns = 3;
    with_pull_requests(&mut fx);
    let (pool, item, head) = reviewed_item(&fx).await;
    let view = format!(
        r#"{{"state":"OPEN","headRefOid":"{head}","reviews":[{{"state":"CHANGES_REQUESTED","author":{{"login":"m"}},"body":"FAIL-REWORK","commit":{{"oid":"{head}"}}}}]}}"#
    );
    fs::write(fx.root.join("bin/gh.view.json"), view).unwrap();
    pool.check_reviews().await.unwrap();

    pool.start();
    let queue = pool.queue().clone();
    wait_for(&pool, |s| {
        idle(s) && queue.list(State::Hold).unwrap().len() == 1
    })
    .await;
    pool.stop();
    let held = pool.meta(State::Hold, &item).unwrap();
    assert_eq!(held.turns, 3, "first run, then two failed reworks");
    assert!(held.needs_rework.is_some(), "the feedback is kept");

    // Its pull request is still open: back to review, feedback intact.
    pool.resume(&item).await.unwrap();
    assert_eq!(queue.list(State::InReview).unwrap(), vec![item.clone()]);
    let meta = pool.meta(State::InReview, &item).unwrap();
    assert_eq!(meta.turns, 0);
    assert!(meta.needs_rework.unwrap().contains("FAIL-REWORK"));
    assert_eq!(
        meta.pr_url.as_deref(),
        Some("https://github.com/up/repo/pull/7")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn orphans_with_a_pull_request_return_to_review() {
    let fx = fixture(&[], 1);
    let inprogress = fx.root.join("work/inprogress/demo");
    fs::create_dir_all(&inprogress).unwrap();
    fs::write(inprogress.join("a.txt"), "").unwrap();
    fs::write(
        inprogress.join("a.meta.json"),
        r#"{"turns":2,"pr_url":"https://github.com/up/repo/pull/7"}"#,
    )
    .unwrap();
    fs::write(inprogress.join("b.txt"), "").unwrap();
    let queue = Queue::new(fx.root.join("work"));
    Supervisor::new(queue.clone(), fx.config.clone()).unwrap();
    assert_eq!(
        queue.list(State::InReview).unwrap(),
        vec![WorkItem::new("demo", "a.txt")]
    );
    assert_eq!(
        queue.list(State::Open).unwrap(),
        vec![WorkItem::new("demo", "b.txt")]
    );
}

/// Puts a branch with one commit on the fixture's fork, as a contributor
/// would have before opening a pull request, and returns its head.
fn fork_branch(fx: &Fixture, bare: &Path, branch: &str) -> String {
    let repo = fx.root.join("repo");
    sh(&repo, &["switch", "-q", "-c", branch]);
    fs::write(repo.join("feature.txt"), "feature").unwrap();
    sh(&repo, &["add", "feature.txt"]);
    sh(
        &repo,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-qm",
            "feat: the feature",
        ],
    );
    sh(&repo, &["push", "-q", &bare.display().to_string(), branch]);
    sh(&repo, &["switch", "-q", "main"]);
    sh(&repo, &["branch", "-q", "-D", branch]);
    git_out(bare, &["rev-parse", branch])
}

fn pull_request_view(fx: &Fixture, state: &str, owner: &str, head: &str, reviews: &str) {
    fs::write(
        fx.root.join("bin/gh.view.json"),
        format!(
            r#"{{"state":"{state}","title":"feat: the feature","headRefName":"feature/x",
                "headRepositoryOwner":{{"login":"{owner}"}},"headRefOid":"{head}","reviews":[{reviews}]}}"#
        ),
    )
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn an_imported_pull_request_waits_in_review_and_is_reworked_on_its_own_branch() {
    let mut fx = fixture(&[], 1);
    let bare = with_pull_requests(&mut fx);
    let head = fork_branch(&fx, &bare, "feature/x");
    pull_request_view(&fx, "OPEN", "me", &head, "");
    let queue = Queue::new(fx.root.join("work"));
    let pool = Supervisor::new(queue.clone(), fx.config.clone()).unwrap();

    let item = pool
        .import_pull_request(" https://github.com/up/repo/pull/9/files ")
        .await
        .unwrap();
    assert_eq!(item, WorkItem::new("demo", "pr-9.txt"));
    assert_eq!(queue.list(State::InReview).unwrap(), vec![item.clone()]);
    let text = queue.read(State::InReview, &item).unwrap();
    assert!(
        text.contains("feat: the feature") && text.contains("pull/9"),
        "{text}"
    );
    let meta = pool.meta(State::InReview, &item).unwrap();
    assert_eq!(
        meta.pr_url.as_deref(),
        Some("https://github.com/up/repo/pull/9")
    );
    assert_eq!(meta.pushed_head.as_deref(), Some(head.as_str()));
    assert_eq!(meta.pr_title.as_deref(), Some("feat: the feature"));
    assert_eq!(meta.remote_branch.as_deref(), Some("feature/x"));
    assert_eq!(meta.turns, 0);
    assert_eq!(
        git_out(&fx.root.join("repo"), &["rev-parse", "ai/pr-9"]),
        head
    );

    // The same pull request, by any link to it, is not imported twice.
    let err = pool
        .import_pull_request("https://github.com/up/repo/pull/9")
        .await
        .unwrap_err();
    assert!(matches!(err, PoolError::AlreadyImported { .. }), "{err}");

    // A reviewer asks for changes; a worker adds to the contributor's branch.
    pull_request_view(
        &fx,
        "OPEN",
        "me",
        &head,
        &format!(
            r#"{{"state":"CHANGES_REQUESTED","author":{{"login":"maint"}},"body":"Please rename it.","commit":{{"oid":"{head}"}}}}"#
        ),
    );
    assert_eq!(
        pool.check_reviews().await.unwrap(),
        vec![(item.clone(), ReviewChange::ChangesRequested)]
    );
    pool.start();
    wait_for(&pool, |s| {
        idle(s)
            && queue.list(State::InReview).unwrap().len() == 1
            && pool.meta(State::InReview, &item).unwrap().turns == 1
    })
    .await;
    pool.stop();

    let worktree = fx.root.join("repo-worktrees/pr-9");
    let log = git_out(&worktree, &["log", "--format=%s", "main..HEAD"]);
    assert_eq!(log, "fix: address review\nfeat: the feature");
    let new_head = git_out(&worktree, &["rev-parse", "HEAD"]);
    assert_eq!(git_out(&bare, &["rev-parse", "feature/x"]), new_head);
    let ours = Command::new("git")
        .arg("--git-dir")
        .arg(&bare)
        .args(["show-ref", "--verify", "--quiet", "refs/heads/ai/pr-9"])
        .status()
        .unwrap();
    assert!(
        !ours.success(),
        "the contributor's branch is pushed to, not an ai/ copy"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rework_starts_from_what_the_contributor_pushed_since() {
    let mut fx = fixture(&[], 1);
    let bare = with_pull_requests(&mut fx);
    let head = fork_branch(&fx, &bare, "feature/x");
    pull_request_view(&fx, "OPEN", "me", &head, "");
    let queue = Queue::new(fx.root.join("work"));
    let pool = Supervisor::new(queue.clone(), fx.config.clone()).unwrap();
    let item = pool
        .import_pull_request("https://github.com/up/repo/pull/9")
        .await
        .unwrap();
    // The branch moves on after the import.
    let newer = {
        let repo = fx.root.join("repo");
        sh(
            &repo,
            &["fetch", "-q", &bare.display().to_string(), "feature/x"],
        );
        sh(&repo, &["switch", "-q", "--detach", "FETCH_HEAD"]);
        sh(
            &repo,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.com",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "chore: contributor follow-up",
            ],
        );
        let newer = git_out(&repo, &["rev-parse", "HEAD"]);
        sh(
            &repo,
            &[
                "push",
                "-q",
                &bare.display().to_string(),
                "HEAD:refs/heads/feature/x",
            ],
        );
        sh(&repo, &["switch", "-q", "main"]);
        newer
    };
    pull_request_view(
        &fx,
        "OPEN",
        "me",
        &newer,
        &format!(
            r#"{{"state":"CHANGES_REQUESTED","author":{{"login":"maint"}},"body":"Rename.","commit":{{"oid":"{newer}"}}}}"#
        ),
    );
    pool.check_reviews().await.unwrap();
    pool.start();
    wait_for(&pool, |s| {
        idle(s) && pool.meta(State::InReview, &item).unwrap().turns == 1
    })
    .await;
    pool.stop();

    let log = git_out(
        &fx.root.join("repo-worktrees/pr-9"),
        &["log", "--format=%s", "main..HEAD"],
    );
    assert_eq!(
        log,
        "fix: address review\nchore: contributor follow-up\nfeat: the feature"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn pull_requests_that_cannot_be_reworked_are_not_imported() {
    let mut fx = fixture(&[], 1);
    let bare = with_pull_requests(&mut fx);
    let head = fork_branch(&fx, &bare, "feature/x");
    let queue = Queue::new(fx.root.join("work"));
    let pool = Supervisor::new(queue.clone(), fx.config.clone()).unwrap();
    let pr = "https://github.com/up/repo/pull/9";

    let err = pool.import_pull_request("not a link").await.unwrap_err();
    assert!(matches!(err, PoolError::NotPullRequestLink(_)), "{err}");
    let err = pool
        .import_pull_request("https://github.com/other/repo/pull/9")
        .await
        .unwrap_err();
    assert!(matches!(err, PoolError::NoProjectForRepo(_)), "{err}");

    pull_request_view(&fx, "MERGED", "me", &head, "");
    let err = pool.import_pull_request(pr).await.unwrap_err();
    assert!(matches!(err, PoolError::PullRequestNotOpen(_)), "{err}");

    // From someone's fork: the push remote cannot take a push to it.
    pull_request_view(&fx, "OPEN", "stranger", &head, "");
    let err = pool.import_pull_request(pr).await.unwrap_err();
    assert!(matches!(err, PoolError::ForeignBranch { .. }), "{err}");

    // A branch that is gone fails the fetch and leaves nothing queued.
    pull_request_view(&fx, "OPEN", "me", &head, "");
    sh(&bare, &["update-ref", "-d", "refs/heads/feature/x"]);
    pool.import_pull_request(pr).await.unwrap_err();

    for state in State::ALL {
        assert!(queue.list(state).unwrap().is_empty(), "{state:?}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn earlier_runs_are_filled_in_from_sidecars_once() {
    let fx = fixture(&[], 1);
    let work = fx.root.join("work");
    for state in ["open", "inprogress", "inreview", "closed", "hold"] {
        fs::create_dir_all(work.join(state).join("demo")).unwrap();
    }
    let log = fx.root.join("old.log");
    fs::write(
        &log,
        concat!(
            "{\"type\":\"system\",\"subtype\":\"init\",\"model\":\"claude-old\"}\n",
            "{\"type\":\"result\",\"is_error\":false,\"result\":\"ok\",\"total_cost_usd\":1.5,",
            "\"duration_ms\":90000,\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}\n"
        ),
    )
    .unwrap();
    let closed = work.join("closed/demo");
    fs::write(closed.join("old.txt"), "Fix it").unwrap();
    fs::write(
        closed.join("old.meta.json"),
        r#"{"turns":2,"pr_url":"https://github.com/up/repo/pull/9"}"#,
    )
    .unwrap();
    fs::write(
        closed.join("old.result.md"),
        format!(
            "# demo/old.txt\n\n- Provider: Anthropic (Claude)\n- Log: `{}`\n\n## Commits\n\n- a fix\n",
            log.display()
        ),
    )
    .unwrap();
    // A closed item that was never run has nothing to fill in.
    fs::write(closed.join("never.txt"), "x").unwrap();

    let pool = Supervisor::new(Queue::new(&work), fx.config.clone()).unwrap();
    assert_eq!(pool.backfill_history().unwrap(), 2);
    assert_eq!(pool.backfill_history().unwrap(), 0);

    let [old] = pool.history().unwrap().try_into().unwrap();
    assert_eq!(
        (old.item.as_str(), old.state.as_deref()),
        ("old.txt", Some("closed"))
    );
    assert_eq!(
        (old.turns, old.rework_turns, old.finished),
        (2, 1, Some("merged"))
    );
    assert!(old.backfilled);
    assert_eq!(old.models, vec!["claude-old"]);
    assert_eq!((old.input_tokens, old.output_tokens), (10, 5));
    assert_eq!(old.cost_micro_usd, 1_500_000);
    assert!(old.records.iter().all(|r| r.backfilled));
    assert_eq!(old.records[1].duration_ms, 90_000);
}

/// Waits until the pool's suggestions satisfy `done`.
async fn wait_for_suggestions(
    pool: &Supervisor,
    done: impl Fn(&[Suggestion]) -> bool,
) -> Vec<Suggestion> {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let open: Vec<_> = pool
                .suggestions()
                .unwrap()
                .into_iter()
                .flat_map(|p| p.open.into_iter().chain(p.rejected))
                .collect();
            if done(&open) {
                return open;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("suggestions did not reach the expected state")
}

/// Runs the failing `items` to hold and returns the pool, stopped.
async fn failed_pool(fx: &Fixture) -> Supervisor {
    let queue = Queue::new(fx.root.join("work"));
    let pool = Supervisor::new(queue.clone(), fx.config.clone()).unwrap();
    let failing = queue.list(State::Open).unwrap().len();
    pool.start();
    wait_for(&pool, |s| {
        queue.list(State::Hold).unwrap().len() == failing && idle(s)
    })
    .await;
    pool.stop();
    pool
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failure_the_agent_could_avoid_yields_a_suggestion_that_is_kept_once_accepted() {
    let fx = fixture(
        &[("bad.txt", "PLEASE-FAIL"), ("net.txt", "NETWORK-FAIL")],
        1,
    );
    let pool = failed_pool(&fx).await;

    let found = wait_for_suggestions(&pool, |s| !s.is_empty()).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let found = wait_for_suggestions(&pool, |s| s.len() >= found.len()).await;
    assert_eq!(
        found.len(),
        1,
        "the network failure was evaluated: {found:?}"
    );
    let s = &found[0];
    assert_eq!((s.item.as_str(), s.failure.as_str()), ("bad.txt", "agent"));
    assert_eq!(s.text, "Run the tests before committing.");
    assert_eq!(s.status, SuggestionStatus::Pending);

    let notes = fx.root.join("spec/demo").join(NOTES_FILE);
    assert!(!notes.exists());
    pool.accept_suggestion("demo", s.id).unwrap();
    let text = fs::read_to_string(&notes).unwrap();
    assert!(
        text.contains("- Run the tests before committing.\n"),
        "{text}"
    );
    assert!(pool.suggestions().unwrap()[0].open.is_empty());
    assert!(matches!(
        pool.accept_suggestion("demo", s.id),
        Err(PoolError::SuggestionState(_))
    ));

    let versions = pool.notes_versions("demo").unwrap();
    assert_eq!(versions.len(), 2);
    assert_eq!(versions[0].note, format!("accepted suggestion {}", s.id));
    pool.revert_notes("demo", versions[1].id).unwrap();
    assert_eq!(fs::read_to_string(&notes).unwrap(), "");
    let versions = pool.notes_versions("demo").unwrap();
    assert_eq!(versions.len(), 3);
    pool.revert_notes("demo", versions[1].id).unwrap();
    assert_eq!(fs::read_to_string(&notes).unwrap(), text);
}

#[tokio::test(flavor = "multi_thread")]
async fn rejected_suggestions_can_be_restored_until_they_expire() {
    let mut fx = fixture(&[("bad.txt", "PLEASE-FAIL")], 1);
    let pool = failed_pool(&fx).await;
    let id = wait_for_suggestions(&pool, |s| !s.is_empty()).await[0].id;

    pool.reject_suggestion("demo", id).unwrap();
    let listed = &pool.suggestions().unwrap()[0];
    assert!(listed.open.is_empty());
    assert_eq!(listed.rejected[0].status, SuggestionStatus::Rejected);
    pool.restore_suggestion("demo", id).unwrap();
    assert_eq!(pool.suggestions().unwrap()[0].open[0].id, id);

    // With nothing to keep rejections for, they go at the next look.
    fx.config.suggestion_retention = Duration::ZERO;
    pool.set_config(fx.config.clone());
    pool.reject_suggestion("demo", id).unwrap();
    assert!(pool.suggestions().unwrap().is_empty());
    assert!(matches!(
        pool.restore_suggestion("demo", id),
        Err(PoolError::NoSuggestion(_))
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_comment_goes_to_the_agent_or_is_added_to_the_suggestion() {
    let fx = fixture(
        &[
            ("a.txt", "PLEASE-FAIL"),
            ("b.txt", "PLEASE-FAIL"),
            ("c.txt", "PLEASE-FAIL"),
        ],
        1,
    );
    let pool = failed_pool(&fx).await;
    let found = wait_for_suggestions(&pool, |s| s.len() == 3).await;
    let [a, b, c] = [&found[0], &found[1], &found[2]].map(|s| s.id);

    // Asked to evaluate, the agent revises, keeps or withdraws.
    pool.evaluate_suggestion("demo", a, "REVISE it").unwrap();
    let after = wait_for_suggestions(&pool, |s| {
        s.iter().all(|s| s.status != SuggestionStatus::Evaluating)
            && s.iter().any(|s| s.text == "Run just ci before committing.")
    })
    .await;
    let revised = after.iter().find(|s| s.id == a).unwrap();
    assert_eq!(revised.status, SuggestionStatus::Pending);
    let who: Vec<_> = revised.remarks.iter().map(|r| r.by).collect();
    assert_eq!(who, [Author::User, Author::Agent]);
    assert_eq!(revised.remarks[1].text, "Good point.");

    pool.evaluate_suggestion("demo", b, "WITHDRAW it").unwrap();
    let after = wait_for_suggestions(&pool, |s| {
        s.iter()
            .any(|s| s.id == b && s.status == SuggestionStatus::Rejected)
    })
    .await;
    assert_eq!(
        after.iter().find(|s| s.id == b).unwrap().remarks[1].text,
        "Too narrow."
    );

    // The notes file gets the revised text, and the comment on the other.
    pool.accept_suggestion("demo", a).unwrap();
    pool.amend_suggestion("demo", c, "Only the changed crates.")
        .unwrap();
    let text = fs::read_to_string(fx.root.join("spec/demo").join(NOTES_FILE)).unwrap();
    assert!(
        text.contains("- Run just ci before committing.\n"),
        "{text}"
    );
    assert!(
        text.contains("- Run the tests before committing.\n  Only the changed crates.\n"),
        "{text}"
    );
    let notes = pool.notes_versions("demo").unwrap();
    assert_eq!(notes[0].note, format!("accepted suggestion {c}"));
}

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
    Activity, Balancing, BranchOutcome, Cleanup, PoolConfig, PoolError, PoolSnapshot, Project,
    ProviderUsage, PullRequests, ReviewChange, Supervisor, UsageLimits,
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

/// Stands in for `claude -p <prompt> …`: commits a file in its working
/// directory and reports success, unless the prompt asks it to fail.
const FAKE_CLAUDE: &str = r#"#!/bin/sh
prompt="$2"
echo '{"type":"system","subtype":"init"}'
case "$prompt" in
  *"<review-feedback>"*)
    printf '%s' "$prompt" > "$(dirname "$0")/rework-prompt.txt"
    case "$prompt" in *FAIL-REWORK*)
      echo '{"type":"result","is_error":true,"result":"rework failed"}'
      exit 1 ;;
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
    echo "$CARGO_TARGET_DIR" > cache.txt
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
echo '{"type":"result","is_error":false,"result":"Fixed the thing"}'
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
        worktrees.join(".target-1").display().to_string()
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

#[tokio::test(flavor = "multi_thread")]
async fn background_processes_of_an_agent_are_stopped_with_it() {
    let fx = fixture(&[("child.txt", "LEAVES-CHILD")], 1);
    let pid_file = fx.root.join("child.pid");
    // Wrap the fake agent to tell it where to record its child's pid.
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

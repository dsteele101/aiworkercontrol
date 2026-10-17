//! The pool end to end, with a fake `claude` CLI against a temporary repo.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use agent_backend::{Auth, Provider, ProviderConfig};
use work_queue::{Queue, State, WorkItem};
use worker_pool::{
    Activity, BranchOutcome, Cleanup, PoolConfig, PoolError, PoolSnapshot, Project, PullRequests,
    Supervisor,
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
    if [ -f "$here/gh.merged" ]; then echo '{"state":"MERGED"}'; else echo '{"state":"OPEN"}'; fi ;;
esac
"#;

/// Stands in for `claude -p <prompt> …`: commits a file in its working
/// directory and reports success, unless the prompt asks it to fail.
const FAKE_CLAUDE: &str = r#"#!/bin/sh
prompt="$2"
echo '{"type":"system","subtype":"init"}'
case "$prompt" in
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
    pool.resume(&item).unwrap();
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
    let err = pool.resume(&WorkItem::new("demo", "a.txt")).unwrap_err();
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

#[tokio::test(flavor = "multi_thread")]
async fn opens_a_pull_request_from_the_agents_draft() {
    let mut fx = fixture(&[("good.txt", "Fix it")], 1);
    let bare = with_pull_requests(&mut fx);
    let (pool, item) = closed_item(&fx).await;

    let result = fs::read_to_string(fx.root.join("work/closed/demo/good.result.md")).unwrap();
    assert!(
        result.contains("- Pull request: https://github.com/up/repo/pull/7"),
        "{result}"
    );
    assert_eq!(
        pool.pull_request(&item).as_deref(),
        Some("https://github.com/up/repo/pull/7")
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
    let tracked = Command::new("git")
        .arg("-C")
        .arg(fx.root.join("repo-worktrees/good"))
        .args(["ls-files"])
        .output()
        .unwrap();
    assert!(!String::from_utf8_lossy(&tracked.stdout).contains(".pr"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_pull_request_still_closes_the_item() {
    let mut fx = fixture(&[("good.txt", "Fix it")], 1);
    with_pull_requests(&mut fx);
    fs::write(fx.root.join("bin/gh.fail"), "").unwrap();
    let (pool, item) = closed_item(&fx).await;

    let result = fs::read_to_string(fx.root.join("work/closed/demo/good.result.md")).unwrap();
    assert!(result.contains("- Pull request not opened: "), "{result}");
    assert!(result.contains("boom from gh"), "{result}");
    assert_eq!(pool.pull_request(&item), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_merged_pull_request_frees_the_worktree() {
    let mut fx = fixture(&[("good.txt", "Fix it")], 1);
    with_pull_requests(&mut fx);
    let (pool, item) = closed_item(&fx).await;

    // Open, and not in main: kept.
    assert!(pool.clean_up_merged().await.unwrap().is_empty());
    // Squash-merged upstream: git cannot tell, the pull request can.
    fs::write(fx.root.join("bin/gh.merged"), "").unwrap();
    assert_eq!(pool.clean_up_merged().await.unwrap(), vec![item]);
    assert!(!fx.root.join("repo-worktrees/good").exists());
    assert!(!branch_exists(&fx.root.join("repo"), "ai/good"));
}

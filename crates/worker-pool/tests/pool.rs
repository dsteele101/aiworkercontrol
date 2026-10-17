//! The pool end to end, with a fake `claude` CLI against a temporary repo.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use agent_backend::{Auth, Provider, ProviderConfig};
use work_queue::{Queue, State, WorkItem};
use worker_pool::{Activity, PoolConfig, PoolSnapshot, Project, Supervisor};

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
  *NO-COMMIT*)
    echo '{"type":"result","is_error":false,"result":"looked around"}'
    exit 0 ;;
esac
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
    let fake = bin.join("claude");
    fs::write(&fake, FAKE_CLAUDE).unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();

    let config = PoolConfig {
        workers,
        provider: ProviderConfig::new(Provider::Anthropic, Auth::Plan),
        projects: vec![Project::new("demo", &spec, &repo)],
        cli_dir: Some(bin),
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

#[tokio::test(flavor = "multi_thread")]
async fn closes_successful_items_and_reopens_failed_ones() {
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

    let snapshot = wait_for(&pool, |s| {
        s.failed.len() == 2
            && s.workers
                .iter()
                .all(|w| matches!(w.activity, Activity::Idle))
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

    let reasons: Vec<_> = snapshot
        .failed
        .iter()
        .map(|(i, r)| (i.file.as_str(), r.as_str()))
        .collect();
    assert_eq!(
        reasons,
        vec![
            ("bad.txt", "agent failed: could not do it"),
            ("idle.txt", "agent finished without committing to ai/idle"),
        ]
    );
    let open = queue.list(State::Open).unwrap();
    assert_eq!(open.len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn retry_reruns_a_failed_item_in_a_fresh_worktree() {
    let fx = fixture(&[("bad.txt", "PLEASE-FAIL")], 1);
    let queue = Queue::new(fx.root.join("work"));
    let pool = Supervisor::new(queue.clone(), fx.config.clone()).unwrap();
    pool.start();
    wait_for(&pool, |s| s.failed.len() == 1).await;

    // Fix the item, then retry it: the old worktree is replaced.
    let item = WorkItem::new("demo", "bad.txt");
    fs::write(queue.path(State::Open, &item), "now fine").unwrap();
    pool.retry(&item);
    wait_for(&pool, |_| queue.list(State::Closed).unwrap().len() == 1).await;
    pool.stop();
    assert!(pool.snapshot().failed.is_empty());
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
    let s = wait_for(&pool, |s| {
        !s.failed.is_empty() || queue.list(State::Closed).unwrap().len() == 1
    })
    .await;
    pool.stop();
    assert!(s.failed.is_empty(), "{:?}", s.failed);

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
    let s = wait_for(&pool, |s| !s.failed.is_empty()).await;
    pool.stop();

    let reason = &s.failed[0].1;
    assert!(reason.starts_with("setup command failed"), "{reason}");
    assert!(reason.ends_with("boom"), "{reason}");
    let log = fs::read_to_string(fx.root.join("repo-worktrees/a.log")).unwrap();
    assert!(!log.contains("system"), "agent ran: {log}");
    assert_eq!(queue.list(State::Open).unwrap().len(), 1);
}

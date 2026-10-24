//! The few git operations a worker needs.

use std::path::Path;

use tokio::process::Command;

use crate::PoolError;

/// Checks out a fresh worktree of `repo` at `path` on `branch`, reset to
/// `base`. A worktree left at `path` by an earlier, failed attempt is
/// removed first.
pub(crate) async fn prepare_worktree(
    repo: &Path,
    base: &str,
    path: &Path,
    branch: &str,
) -> Result<(), PoolError> {
    remove_worktree(repo, path).await?;
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(PoolError::io("create", parent))?;
    }
    let path = path.to_string_lossy();
    git(repo, &["worktree", "add", "-B", branch, &path, base])
        .await
        .map(drop)
}

/// Removes the worktree at `path` if there is one, discarding anything
/// uncommitted in it, and forgets any worktree whose folder is gone.
/// Returns whether a worktree folder was removed.
pub(crate) async fn remove_worktree(repo: &Path, path: &Path) -> Result<bool, PoolError> {
    let existed = path.exists();
    if existed {
        let path = path.to_string_lossy();
        git(repo, &["worktree", "remove", "--force", &path]).await?;
    }
    git(repo, &["worktree", "prune"]).await?;
    Ok(existed)
}

/// Whether `repo` has a local branch named `branch`.
pub(crate) async fn branch_exists(repo: &Path, branch: &str) -> Result<bool, PoolError> {
    let reference = format!("refs/heads/{branch}");
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["show-ref", "--verify", "--quiet", &reference])
        .status()
        .await
        .map_err(|source| PoolError::Spawn {
            program: "git".into(),
            source,
        })?;
    Ok(status.success())
}

/// Whether every commit on `branch` is in `base`, either itself (a merge or
/// fast-forward) or as an identical change (a rebase or cherry-pick). A
/// squash of several commits is not recognized.
pub(crate) async fn is_merged(repo: &Path, base: &str, branch: &str) -> Result<bool, PoolError> {
    let out = git(repo, &["cherry", base, branch]).await?;
    Ok(!out.lines().any(|line| line.starts_with('+')))
}

/// Deletes `branch`. Callers check [`is_merged`] first: `git branch -d`
/// would judge against the repo's current branch rather than the base.
pub(crate) async fn delete_branch(repo: &Path, branch: &str) -> Result<(), PoolError> {
    git(repo, &["branch", "-D", branch]).await.map(drop)
}

/// The fetch URL of `remote` in `repo`.
pub(crate) async fn remote_url(repo: &Path, remote: &str) -> Result<String, PoolError> {
    Ok(git(repo, &["remote", "get-url", remote])
        .await?
        .trim()
        .to_string())
}

/// Pushes `branch` from `worktree` to `remote`, replacing what is there:
/// `ai/*` branches belong to the app, and a retry rewrites them.
pub(crate) async fn push(worktree: &Path, remote: &str, branch: &str) -> Result<(), PoolError> {
    let refspec = format!("{branch}:refs/heads/{branch}");
    git(worktree, &["push", "--force", remote, &refspec])
        .await
        .map(drop)
}

/// Pushes `branch` from `worktree` to `remote_branch` on `remote` without
/// replacing anything: it is refused if the remote branch has commits the
/// worktree lacks.
pub(crate) async fn push_ahead(
    worktree: &Path,
    remote: &str,
    branch: &str,
    remote_branch: &str,
) -> Result<(), PoolError> {
    let refspec = format!("{branch}:refs/heads/{remote_branch}");
    git(worktree, &["push", remote, &refspec]).await.map(drop)
}

/// The URL `remote` is pushed to, which is also where an imported pull
/// request's branch is read from: the place it will be pushed back to.
pub(crate) async fn push_url(repo: &Path, remote: &str) -> Result<String, PoolError> {
    Ok(git(repo, &["remote", "get-url", "--push", remote])
        .await?
        .trim()
        .to_string())
}

/// Fetches `remote_branch` from `source` (a remote or URL) into the local
/// `branch`, replacing it. `branch` must not be checked out.
pub(crate) async fn fetch_into(
    repo: &Path,
    source: &str,
    remote_branch: &str,
    branch: &str,
) -> Result<(), PoolError> {
    let refspec = format!("+refs/heads/{remote_branch}:refs/heads/{branch}");
    git(repo, &["fetch", source, &refspec]).await.map(drop)
}

/// Brings the worktree's branch up to `remote_branch` at `source` (a remote
/// or URL), which must contain it: the remote may have moved on since the
/// branch was last fetched.
pub(crate) async fn fast_forward(
    worktree: &Path,
    source: &str,
    remote_branch: &str,
) -> Result<(), PoolError> {
    git(worktree, &["fetch", source, remote_branch]).await?;
    git(worktree, &["merge", "--ff-only", "FETCH_HEAD"])
        .await
        .map(drop)
}

/// Commits nothing on the worktree's branch, to give it a new head.
pub(crate) async fn commit_empty(worktree: &Path, message: &str) -> Result<(), PoolError> {
    git(worktree, &["commit", "--allow-empty", "-m", message])
        .await
        .map(drop)
}

/// Makes sure the worktree at `path` exists, checking out the existing
/// `branch` there if it does not. Returns whether it had to be created.
pub(crate) async fn ensure_worktree(
    repo: &Path,
    path: &Path,
    branch: &str,
) -> Result<bool, PoolError> {
    if path.exists() {
        return Ok(false);
    }
    git(repo, &["worktree", "prune"]).await?;
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(PoolError::io("create", parent))?;
    }
    let path = path.to_string_lossy();
    git(repo, &["worktree", "add", &path, branch]).await?;
    Ok(true)
}

/// The commit the worktree has checked out.
pub(crate) async fn head(worktree: &Path) -> Result<String, PoolError> {
    Ok(git(worktree, &["rev-parse", "HEAD"])
        .await?
        .trim()
        .to_string())
}

/// The commits on the worktree's branch that `base` does not have, newest
/// first, as `<short hash> <subject>`.
pub(crate) async fn commits_since(worktree: &Path, base: &str) -> Result<Vec<String>, PoolError> {
    let range = format!("{base}..HEAD");
    let out = git(worktree, &["log", "--format=%h %s", &range]).await?;
    Ok(out.lines().map(str::to_string).collect())
}

/// What the worktree's `HEAD` changed since `base`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct DiffStat {
    pub added: u64,
    pub removed: u64,
    pub files: u64,
}

/// Lines added and removed, and files touched, between `base` and `HEAD`.
/// Binary files count as touched, with no lines.
pub(crate) async fn diff_stat(worktree: &Path, base: &str) -> Result<DiffStat, PoolError> {
    let range = format!("{base}..HEAD");
    let out = git(worktree, &["diff", "--numstat", &range]).await?;
    let mut stat = DiffStat::default();
    for line in out.lines() {
        let mut cols = line.split('\t');
        stat.added += cols.next().and_then(|n| n.parse::<u64>().ok()).unwrap_or(0);
        stat.removed += cols.next().and_then(|n| n.parse::<u64>().ok()).unwrap_or(0);
        stat.files += 1;
    }
    Ok(stat)
}

/// Makes `dir` a git repository if it is not one, for runs that need no
/// repository but whose CLI refuses to start outside one.
pub(crate) async fn ensure_repo(dir: &Path) -> Result<(), PoolError> {
    if dir.join(".git").exists() {
        return Ok(());
    }
    git(dir, &["init", "-q"]).await.map(|_| ())
}

async fn git(dir: &Path, args: &[&str]) -> Result<String, PoolError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .await
        .map_err(|source| PoolError::Spawn {
            program: "git".into(),
            source,
        })?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(PoolError::Git {
            args: args.join(" "),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        })
    }
}

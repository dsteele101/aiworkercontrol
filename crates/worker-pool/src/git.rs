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

/// The commits on the worktree's branch that `base` does not have, newest
/// first, as `<short hash> <subject>`.
pub(crate) async fn commits_since(worktree: &Path, base: &str) -> Result<Vec<String>, PoolError> {
    let range = format!("{base}..HEAD");
    let out = git(worktree, &["log", "--format=%h %s", &range]).await?;
    Ok(out.lines().map(str::to_string).collect())
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

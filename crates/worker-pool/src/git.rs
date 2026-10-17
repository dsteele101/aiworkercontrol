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
    if path.exists() {
        let path = path.to_string_lossy();
        git(repo, &["worktree", "remove", "--force", &path]).await?;
    }
    git(repo, &["worktree", "prune"]).await?;
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

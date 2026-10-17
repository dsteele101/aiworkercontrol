//! Publishing finished work as a GitHub pull request, through the `gh` CLI
//! and the user's own GitHub login.

use std::path::{Path, PathBuf};

use tokio::process::Command;

use crate::PoolError;

/// A pull request's title and description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Draft {
    pub title: String,
    pub body: String,
}

impl Draft {
    /// Reads a draft written by an agent: the title on the first non-empty
    /// line (a leading `#` heading marker is dropped), the description after.
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim_start();
        let (first, rest) = text.split_once('\n').unwrap_or((text, ""));
        let title = first.trim().trim_start_matches('#').trim();
        if title.is_empty() {
            return None;
        }
        Some(Self {
            title: title.to_string(),
            body: rest.trim().to_string(),
        })
    }
}

/// The GitHub owner in a remote URL: `https://github.com/OWNER/REPO(.git)`,
/// `git@github.com:OWNER/REPO(.git)` or `ssh://git@github.com/OWNER/REPO`.
pub(crate) fn owner_from_url(url: &str) -> Option<String> {
    let path = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("http://github.com/"))
        .or_else(|| url.strip_prefix("git@github.com:"))
        .or_else(|| url.strip_prefix("ssh://git@github.com/"))?;
    let owner = path.split('/').next()?;
    (!owner.is_empty()).then(|| owner.to_string())
}

/// The `gh` program: in `cli_dir` when given, else on `PATH`.
pub(crate) fn program(cli_dir: Option<&Path>) -> PathBuf {
    cli_dir.map_or_else(|| PathBuf::from("gh"), |dir| dir.join("gh"))
}

/// Opens a pull request from `owner:branch` into `repo`'s `base`, or returns
/// the open one that already exists for that branch (a retried item).
pub(crate) async fn open_pr(
    gh: &Path,
    repo: &str,
    base: &str,
    owner: &str,
    branch: &str,
    draft: &Draft,
    as_draft: bool,
) -> Result<String, PoolError> {
    let listed = run(
        gh,
        &[
            "pr",
            "list",
            "--repo",
            repo,
            "--head",
            branch,
            "--state",
            "open",
            "--json",
            "url,headRepositoryOwner",
        ],
    )
    .await?;
    let existing: Vec<serde_json::Value> = serde_json::from_str(&listed).unwrap_or_default();
    let ours = existing.iter().find(|pr| {
        pr.pointer("/headRepositoryOwner/login")
            .and_then(serde_json::Value::as_str)
            == Some(owner)
    });
    if let Some(url) = ours
        .and_then(|pr| pr.get("url"))
        .and_then(serde_json::Value::as_str)
    {
        return Ok(url.to_string());
    }

    let head = format!("{owner}:{branch}");
    let mut args = vec![
        "pr",
        "create",
        "--repo",
        repo,
        "--base",
        base,
        "--head",
        &head,
        "--title",
        &draft.title,
        "--body",
        &draft.body,
    ];
    if as_draft {
        args.push("--draft");
    }
    let out = run(gh, &args).await?;
    // gh prints the new pull request's URL last.
    Ok(out.lines().last().unwrap_or_default().trim().to_string())
}

/// Whether the pull request at `url` has been merged.
pub(crate) async fn is_merged(gh: &Path, url: &str) -> Result<bool, PoolError> {
    let out = run(gh, &["pr", "view", url, "--json", "state"]).await?;
    let state: serde_json::Value = serde_json::from_str(&out).map_err(|e| PoolError::GitHub {
        args: format!("pr view {url}"),
        stderr: format!("unexpected output: {e}"),
    })?;
    Ok(state.get("state").and_then(serde_json::Value::as_str) == Some("MERGED"))
}

async fn run(gh: &Path, args: &[&str]) -> Result<String, PoolError> {
    let output = Command::new(gh)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .map_err(|source| PoolError::Spawn {
            program: gh.display().to_string(),
            source,
        })?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        // Leave the body out of the error; it can be long.
        let shown: Vec<&str> = args
            .iter()
            .copied()
            .take_while(|a| *a != "--body")
            .collect();
        Err(PoolError::GitHub {
            args: shown.join(" "),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    // Test code asserts rather than propagating; see Rust code review.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]

    use super::*;

    #[test]
    fn owner_from_github_urls() {
        for url in [
            "https://github.com/dsteele101/phase.git",
            "https://github.com/dsteele101/phase",
            "git@github.com:dsteele101/phase.git",
            "ssh://git@github.com/dsteele101/phase",
        ] {
            assert_eq!(owner_from_url(url).as_deref(), Some("dsteele101"), "{url}");
        }
        assert_eq!(owner_from_url("/srv/git/phase.git"), None);
        assert_eq!(owner_from_url("https://gitlab.com/a/b"), None);
    }

    #[test]
    fn draft_title_and_body() {
        let draft =
            Draft::parse("\n# fix(engine): give extra turns\n\n## Summary\nDone.\n").unwrap();
        assert_eq!(draft.title, "fix(engine): give extra turns");
        assert_eq!(draft.body, "## Summary\nDone.");
        assert_eq!(Draft::parse("   \n"), None);
    }
}

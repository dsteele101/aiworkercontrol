//! Publishing finished work as a GitHub pull request, through the `gh` CLI
//! and the user's own GitHub login.

use std::path::{Path, PathBuf};

use serde_json::Value;
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

/// Where a pull request stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PrState {
    Open,
    Merged,
    Closed,
}

/// A pull request's state, latest commit, and the feedback asking for
/// changes on that commit, if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReviewStatus {
    pub state: PrState,
    pub head: String,
    pub feedback: Option<String>,
}

/// Reads where the pull request at `url` stands. Feedback is gathered when
/// its latest commit has a review requesting changes, or failing CI: the
/// required checks, or every check when the repository requires none. With
/// `any_comments`, inline review comments on the latest commit count too,
/// whatever kind of review they came in.
pub(crate) async fn review_status(
    gh: &Path,
    url: &str,
    any_comments: bool,
) -> Result<ReviewStatus, PoolError> {
    let view = json(
        &run(
            gh,
            &["pr", "view", url, "--json", "state,headRefOid,reviews"],
        )
        .await?,
        url,
    )?;
    let state = match view.get("state").and_then(Value::as_str) {
        Some("MERGED") => PrState::Merged,
        Some("CLOSED") => PrState::Closed,
        _ => PrState::Open,
    };
    let head = str_at(&view, "headRefOid").to_string();
    if state != PrState::Open {
        return Ok(ReviewStatus {
            state,
            head,
            feedback: None,
        });
    }

    let on_head: Vec<&Value> = view
        .get("reviews")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|r| r.pointer("/commit/oid").and_then(Value::as_str) == Some(head.as_str()))
        .collect();
    let changes_requested = on_head
        .iter()
        .any(|r| str_at(r, "state") == "CHANGES_REQUESTED");
    let failing = failing_checks(gh, url).await?;
    let comments = inline_comments(gh, url, &head).await?;
    if !changes_requested && failing.is_empty() && !(any_comments && !comments.is_empty()) {
        return Ok(ReviewStatus {
            state,
            head,
            feedback: None,
        });
    }

    let mut text = String::new();
    for review in &on_head {
        let body = str_at(review, "body").trim();
        if !body.is_empty() {
            text.push_str(&format!(
                "### Review by {} ({})\n\n{body}\n\n",
                review
                    .pointer("/author/login")
                    .and_then(Value::as_str)
                    .unwrap_or("someone"),
                str_at(review, "state"),
            ));
        }
    }
    if !comments.is_empty() {
        text.push_str("### Inline comments\n\n");
        text.push_str(&comments);
        text.push('\n');
    }
    if !failing.is_empty() {
        text.push_str("### Failing CI checks\n\n");
        for (name, link) in &failing {
            text.push_str(&format!("- {name}: {link}\n"));
        }
    }
    Ok(ReviewStatus {
        state,
        head,
        feedback: Some(text.trim().to_string()),
    })
}

/// Names and links of the failing checks on the pull request's latest
/// commit: the required ones, or all of them if none are required.
async fn failing_checks(gh: &Path, url: &str) -> Result<Vec<(String, String)>, PoolError> {
    let fields = "name,bucket,link";
    let mut out = run_output(gh, &["pr", "checks", url, "--required", "--json", fields]).await?;
    if serde_json::from_slice::<Value>(&out.stdout).is_err() {
        // `gh` reports a repository without required checks as an error.
        out = run_output(gh, &["pr", "checks", url, "--json", fields]).await?;
    }
    // `gh pr checks` exits non-zero while checks fail or are pending; its
    // JSON is what matters. No JSON at all means there are no checks.
    let Ok(checks) = serde_json::from_slice::<Vec<Value>>(&out.stdout) else {
        return Ok(Vec::new());
    };
    Ok(checks
        .iter()
        .filter(|c| str_at(c, "bucket") == "fail")
        .map(|c| (str_at(c, "name").to_string(), str_at(c, "link").to_string()))
        .collect())
}

/// The inline review comments made on `head`, one bullet each.
async fn inline_comments(gh: &Path, url: &str, head: &str) -> Result<String, PoolError> {
    let Some((owner, repo, number)) = parse_pr_url(url) else {
        return Ok(String::new());
    };
    let endpoint = format!("repos/{owner}/{repo}/pulls/{number}/comments");
    // `--slurp` wraps the pages in one array, so many pages still parse.
    let pages = json(
        &run(gh, &["api", "--paginate", "--slurp", &endpoint]).await?,
        url,
    )?;
    let comments = pages
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_array)
        .flatten();
    let mut text = String::new();
    for c in comments {
        if str_at(c, "commit_id") != head {
            continue;
        }
        let line = c
            .get("line")
            .and_then(Value::as_u64)
            .map_or_else(String::new, |l| format!(":{l}"));
        text.push_str(&format!(
            "- `{}{line}` ({}): {}\n",
            str_at(c, "path"),
            c.pointer("/user/login")
                .and_then(Value::as_str)
                .unwrap_or("someone"),
            str_at(c, "body").trim().replace('\n', "\n  "),
        ));
    }
    Ok(text)
}

/// `(owner, repo, number)` from `https://github.com/OWNER/REPO/pull/N`.
fn parse_pr_url(url: &str) -> Option<(&str, &str, &str)> {
    let mut parts = url.strip_prefix("https://github.com/")?.split('/');
    let (owner, repo, pull, number) = (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
    (pull == "pull").then_some((owner, repo, number))
}

fn str_at<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or_default()
}

fn json(text: &str, url: &str) -> Result<Value, PoolError> {
    serde_json::from_str(text).map_err(|e| PoolError::GitHub {
        args: format!("(reading {url})"),
        stderr: format!("unexpected output: {e}"),
    })
}

async fn run_output(gh: &Path, args: &[&str]) -> Result<std::process::Output, PoolError> {
    Command::new(gh)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .map_err(|source| PoolError::Spawn {
            program: gh.display().to_string(),
            source,
        })
}

async fn run(gh: &Path, args: &[&str]) -> Result<String, PoolError> {
    let output = run_output(gh, args).await?;
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

    #[test]
    fn pr_url_parts() {
        assert_eq!(
            parse_pr_url("https://github.com/phase-rs/phase/pull/10058"),
            Some(("phase-rs", "phase", "10058"))
        );
        assert_eq!(
            parse_pr_url("https://github.com/phase-rs/phase/issues/1"),
            None
        );
    }
}

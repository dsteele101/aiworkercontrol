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
) -> Result<(String, String), PoolError> {
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
            "url,title,headRepositoryOwner",
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
        let title = ours.map_or("", |pr| str_at(pr, "title"));
        return Ok((url.to_string(), title.to_string()));
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
    let url = out.lines().last().unwrap_or_default().trim().to_string();
    Ok((url, draft.title.clone()))
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
/// changes on that commit, if any (including a merge conflict with its base).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReviewStatus {
    pub state: PrState,
    pub head: String,
    pub feedback: Option<String>,
    /// When the latest review requesting changes on `head` was submitted.
    pub reviewed_at: Option<String>,
    /// Whether checks are still running on `head`. Feedback is held back
    /// until they finish, so it can include what they report.
    pub ci_running: bool,
}

/// Reads where the pull request at `url` stands. While checks are running on
/// its latest commit there is no feedback yet; see [`ReviewStatus::ci_running`].
/// Feedback is gathered when
/// its latest commit has a review requesting changes, or failing CI: the
/// required checks, or every check when the repository requires none. With
/// `any_comments`, inline review comments on the latest commit count too,
/// whatever kind of review they came in, as does a comment-only review with
/// a body, and a non-bot conversation comment naming the head. A merge conflict with the base
/// branch is feedback too.
pub(crate) async fn review_status(
    gh: &Path,
    url: &str,
    any_comments: bool,
    ci_logs: bool,
) -> Result<ReviewStatus, PoolError> {
    let view = json(
        &run(
            gh,
            &[
                "pr",
                "view",
                url,
                "--json",
                "state,headRefOid,reviews,mergeable,baseRefName",
            ],
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
            reviewed_at: None,
            ci_running: false,
        });
    }

    let on_head: Vec<&Value> = view
        .get("reviews")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|r| r.pointer("/commit/oid").and_then(Value::as_str) == Some(head.as_str()))
        .collect();
    let requests: Vec<&&Value> = on_head
        .iter()
        .filter(|r| str_at(r, "state") == "CHANGES_REQUESTED")
        .collect();
    let changes_requested = !requests.is_empty();
    let reviewed_at = requests
        .iter()
        .map(|r| str_at(r, "submittedAt"))
        .filter(|at| !at.is_empty())
        .max()
        .map(str::to_string);
    let checks = checks(gh, url).await?;
    if checks.iter().any(|c| str_at(c, "bucket") == "pending") {
        return Ok(ReviewStatus {
            state,
            head,
            feedback: None,
            reviewed_at: None,
            ci_running: true,
        });
    }
    let failing = failing_checks(&checks);
    let comments = inline_comments(gh, url, &head).await?;
    let notes = if any_comments {
        conversation_comments(gh, url, &head).await?
    } else {
        String::new()
    };
    let conflicts = str_at(&view, "mergeable") == "CONFLICTING";
    // A reviewer without write access cannot request changes; their review
    // is a comment whose body carries the next steps.
    let commented = on_head
        .iter()
        .any(|r| str_at(r, "state") == "COMMENTED" && !str_at(r, "body").trim().is_empty());
    if !changes_requested
        && !conflicts
        && failing.is_empty()
        && !(any_comments && (!comments.is_empty() || !notes.is_empty() || commented))
    {
        return Ok(ReviewStatus {
            state,
            head,
            feedback: None,
            reviewed_at: None,
            ci_running: false,
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
    if !notes.is_empty() {
        text.push_str("### Conversation comments\n\n");
        text.push_str(&notes);
        text.push('\n');
    }
    if conflicts {
        let base = str_at(&view, "baseRefName");
        text.push_str(&format!(
            "### Merge conflicts\n\nThe branch conflicts with `{base}` and cannot be \
             merged. Fetch `{base}` from the repository's remote, merge it into the \
             branch, resolve every conflict keeping the intent of both sides, and commit \
             the merge.\n\n"
        ));
    }
    if !failing.is_empty() {
        text.push_str("### Failing CI checks\n\n");
        for (name, link) in &failing {
            text.push_str(&format!("- {name}: {link}\n"));
        }
        if ci_logs {
            text.push_str(&failure_logs(gh, url, &failing).await);
        }
    }
    Ok(ReviewStatus {
        state,
        head,
        feedback: Some(text.trim().to_string()),
        reviewed_at,
        ci_running: false,
    })
}

/// What an open pull request is made of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PrInfo {
    pub state: PrState,
    pub title: String,
    /// The branch the pull request proposes.
    pub head_branch: String,
    /// The owner of the repository that branch is in.
    pub head_owner: String,
    /// The branch's latest commit.
    pub head: String,
}

/// Reads the pull request at `url`.
pub(crate) async fn pr_info(gh: &Path, url: &str) -> Result<PrInfo, PoolError> {
    let fields = "state,title,headRefName,headRefOid,headRepositoryOwner";
    let view = json(&run(gh, &["pr", "view", url, "--json", fields]).await?, url)?;
    let state = match str_at(&view, "state") {
        "MERGED" => PrState::Merged,
        "CLOSED" => PrState::Closed,
        _ => PrState::Open,
    };
    Ok(PrInfo {
        state,
        title: str_at(&view, "title").to_string(),
        head_branch: str_at(&view, "headRefName").to_string(),
        head_owner: view
            .pointer("/headRepositoryOwner/login")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        head: str_at(&view, "headRefOid").to_string(),
    })
}

/// The checks on the pull request's latest commit: the required ones, or all
/// of them if none are required.
async fn checks(gh: &Path, url: &str) -> Result<Vec<Value>, PoolError> {
    let fields = "name,bucket,link";
    let mut out = run_output(gh, &["pr", "checks", url, "--required", "--json", fields]).await?;
    if serde_json::from_slice::<Value>(&out.stdout).is_err() {
        // `gh` reports a repository without required checks as an error.
        out = run_output(gh, &["pr", "checks", url, "--json", fields]).await?;
    }
    // `gh pr checks` exits non-zero while checks fail or are pending; its
    // JSON is what matters. No JSON at all means there are no checks.
    Ok(serde_json::from_slice::<Vec<Value>>(&out.stdout).unwrap_or_default())
}

/// Names and links of the failing checks among `checks`.
fn failing_checks(checks: &[Value]) -> Vec<(String, String)> {
    checks
        .iter()
        .filter(|c| str_at(c, "bucket") == "fail")
        .map(|c| (str_at(c, "name").to_string(), str_at(c, "link").to_string()))
        .collect()
}

/// How much of each failed run's log reaches the agent: its last lines, where
/// the error usually is, within a character budget.
const LOG_LINES: usize = 80;
const LOG_CHARS: usize = 6000;

/// The tails of the failed logs of the runs behind `failing`, one section per
/// run. Best effort: a run whose log cannot be read is left out, since the
/// check links are feedback enough.
async fn failure_logs(gh: &Path, url: &str, failing: &[(String, String)]) -> String {
    let Some((owner, repo, _)) = parse_pr_url(url) else {
        return String::new();
    };
    let mut seen: Vec<&str> = Vec::new();
    let mut text = String::new();
    for (name, link) in failing {
        let Some(run) = run_id(link) else { continue };
        if seen.contains(&run) {
            continue;
        }
        seen.push(run);
        let repo_flag = format!("{owner}/{repo}");
        let args = ["run", "view", run, "-R", &repo_flag, "--log-failed"];
        let Ok(out) = run_output(gh, &args).await else {
            continue;
        };
        if !out.status.success() {
            continue;
        }
        let log = String::from_utf8_lossy(&out.stdout);
        let lines: Vec<&str> = log.lines().collect();
        let tail = lines[lines.len().saturating_sub(LOG_LINES)..].join("\n");
        let start = tail.len().saturating_sub(LOG_CHARS);
        let start = (start..=tail.len())
            .find(|i| tail.is_char_boundary(*i))
            .unwrap_or(tail.len());
        let tail = tail[start..].trim();
        if !tail.is_empty() {
            text.push_str(&format!(
                "\n#### Log of the failed run behind {name}\n\n```\n{tail}\n```\n"
            ));
        }
    }
    text
}

/// The workflow run id in a check's link, `.../actions/runs/ID/job/JOB`.
fn run_id(link: &str) -> Option<&str> {
    let id = link.split("/actions/runs/").nth(1)?.split('/').next()?;
    (!id.is_empty() && id.bytes().all(|b| b.is_ascii_digit())).then_some(id)
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

/// The pull request conversation comments that name `head`, one bullet each:
/// a maintainer's note on where a commit stands, which no review carries.
/// Bots are left out, since their reports name the head too.
async fn conversation_comments(gh: &Path, url: &str, head: &str) -> Result<String, PoolError> {
    let Some((owner, repo, number)) = parse_pr_url(url) else {
        return Ok(String::new());
    };
    if head.is_empty() {
        return Ok(String::new());
    }
    let endpoint = format!("repos/{owner}/{repo}/issues/{number}/comments");
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
        let body = str_at(c, "body").trim();
        let bot = c.pointer("/user/type").and_then(Value::as_str) == Some("Bot");
        if bot || !body.contains(head) {
            continue;
        }
        text.push_str(&format!(
            "- ({}): {}\n",
            c.pointer("/user/login")
                .and_then(Value::as_str)
                .unwrap_or("someone"),
            body.replace('\n', "\n  "),
        ));
    }
    Ok(text)
}

/// `(owner, repo, number)` from `https://github.com/OWNER/REPO/pull/N`.
pub(crate) fn parse_pr_url(url: &str) -> Option<(&str, &str, &str)> {
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

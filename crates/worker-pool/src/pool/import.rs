//! Taking over a pull request opened elsewhere: it joins the queue in review,
//! where feedback on it is reworked like on any other item's.

use work_queue::{META_SUFFIX, State as ItemState, WorkItem};

use super::Supervisor;
use crate::github::{self, PrState};
use crate::history::EventKind;
use crate::issues;
use crate::{IssueSource, ItemMeta, PoolError, Project, git};

impl Supervisor {
    /// Adds the open pull request at `link` to the queue, in review, as the
    /// item `pr-<number>.txt` of the project that opens pull requests
    /// against its repository. The pull request's branch must be on that
    /// project's push remote: it is fetched as the item's `ai/` branch, and
    /// reworks are pushed back to it without replacing anything.
    pub async fn import_pull_request(&self, link: &str) -> Result<WorkItem, PoolError> {
        let (url, owner, repo, number) = canonical_link(link)?;
        let config = self.config();
        let full_name = format!("{owner}/{repo}");
        let (project, pr) = config
            .projects
            .iter()
            .find_map(|p| {
                let pr = p.pr.as_ref()?;
                pr.repo.eq_ignore_ascii_case(&full_name).then_some((p, pr))
            })
            .ok_or(PoolError::NoProjectForRepo(full_name))?;

        let queue = self.queue();
        for state in ItemState::ALL {
            for item in queue.list(state)? {
                if self.pull_request(state, &item).as_deref() == Some(url.as_str()) {
                    return Err(PoolError::AlreadyImported { url, item });
                }
            }
        }

        let gh = github::program(config.cli_dir.as_deref());
        let info = github::pr_info(&gh, &url).await?;
        if info.state != PrState::Open {
            return Err(PoolError::PullRequestNotOpen(url));
        }
        let remote_url = git::remote_url(&project.repo, &pr.push_remote).await?;
        let remote_owner =
            github::owner_from_url(&remote_url).ok_or_else(|| PoolError::NotGitHub {
                remote: pr.push_remote.clone(),
                url: remote_url.clone(),
            })?;
        if !info.head_owner.eq_ignore_ascii_case(&remote_owner) {
            return Err(PoolError::ForeignBranch {
                url,
                head: format!("{}:{}", info.head_owner, info.head_branch),
                remote: pr.push_remote.clone(),
            });
        }

        let item = WorkItem::new(&project.name, format!("pr-{number}.txt"));
        let body = format!(
            "Imported pull request: {title}\n\n{url}\n",
            title = info.title
        );
        // Creating the item file claims its name before anything else is touched.
        queue.add(ItemState::InReview, &item, &body)?;
        let meta = ItemMeta {
            pr_url: Some(url),
            pr_title: Some(info.title.clone()),
            pushed_head: Some(info.head),
            remote_branch: Some(info.head_branch.clone()),
            ..ItemMeta::default()
        };
        let imported = async {
            let source = git::push_url(&project.repo, &pr.push_remote).await?;
            git::fetch_into(
                &project.repo,
                &source,
                &info.head_branch,
                &Project::branch(&item),
            )
            .await?;
            meta.save(queue, ItemState::InReview, &item)
        }
        .await;
        if let Err(e) = imported {
            // Leave nothing behind that would block a retry.
            let _ = std::fs::remove_file(queue.path(ItemState::InReview, &item));
            return Err(e);
        }
        self.shared.record(&item, EventKind::Imported, None);
        self.shared.changed();
        Ok(item)
    }

    /// Brings each project's issues in from its issue source: tops its `open`
    /// items up to the source's `limit` imported issues, chosen in its order
    /// and added as `issue-<number>.txt`, and drops waiting ones whose issues
    /// are no longer open. An issue already in the queue in any state is not
    /// brought in again.
    pub async fn import_issues(&self) -> IssueImport {
        let mut result = IssueImport::default();
        for project in self.config().projects {
            let Some(source) = &project.issues else {
                continue;
            };
            let outcome = self.import_project_issues(&project.name, source).await;
            let mut state = self.shared.lock();
            match outcome {
                Ok((added, removed)) => {
                    state.issue_errors.remove(&project.name);
                    result.added.extend(added);
                    result.removed.extend(removed);
                }
                Err(e) => {
                    state
                        .issue_errors
                        .insert(project.name.clone(), e.to_string());
                    result.errors.push(e);
                }
            }
        }
        // Also redraws a cleared or new error.
        self.shared.changed();
        result
    }

    async fn import_project_issues(
        &self,
        project: &str,
        source: &IssueSource,
    ) -> Result<(Vec<WorkItem>, Vec<WorkItem>), PoolError> {
        let queue = self.queue();
        let (mut open, complete) = issues::fetch_open(source).await?;

        // Waiting items whose issues were closed meanwhile. One a worker has
        // already tried (it has a metadata file) is left to finish.
        let mut removed = Vec::new();
        let mut waiting = 0;
        for item in queue.list(ItemState::Open)? {
            let Some(number) = (item.project == project)
                .then(|| issue_number(&item.file))
                .flatten()
            else {
                continue;
            };
            let closed = complete && !open.iter().any(|i| i.number == number);
            let untried = !queue
                .sidecar_path(ItemState::Open, &item, META_SUFFIX)
                .exists();
            if closed && untried {
                let path = queue.path(ItemState::Open, &item);
                std::fs::remove_file(&path).map_err(PoolError::io("remove", path))?;
                removed.push(item);
            } else {
                waiting += 1;
            }
        }

        let room = (source.limit as usize).saturating_sub(waiting);
        let mut known = std::collections::HashSet::new();
        for state in ItemState::ALL {
            known.extend(
                queue
                    .list(state)?
                    .into_iter()
                    .filter(|i| i.project == project)
                    .map(|i| i.file),
            );
        }
        open.retain(|issue| !known.contains(&issue_file(issue.number)));
        let mut added = Vec::new();
        for issue in issues::ordered(open, source.order).into_iter().take(room) {
            let item = WorkItem::new(project, issue_file(issue.number));
            let body = format!(
                "Resolve issue #{number}: {title}\n\n{url}\n\n{description}\n",
                number = issue.number,
                title = issue.title,
                url = issue.url,
                description = issue.body.trim(),
            );
            queue.add(ItemState::Open, &item, &body)?;
            self.shared
                .record(&item, EventKind::Imported, Some(issue.url));
            added.push(item);
        }
        Ok((added, removed))
    }
}

/// What one check of the issue sources did.
#[derive(Debug, Default)]
pub struct IssueImport {
    /// Issues that joined the queue.
    pub added: Vec<WorkItem>,
    /// Waiting items dropped because their issues are no longer open.
    pub removed: Vec<WorkItem>,
    /// Why some project's issues could not be read.
    pub errors: Vec<PoolError>,
}

/// What names the items made from issues.
const ISSUE_PREFIX: &str = "issue-";

/// The issue number in an item file name made by [`issue_file`].
fn issue_number(file: &str) -> Option<u64> {
    file.strip_prefix(ISSUE_PREFIX)?
        .strip_suffix(".txt")?
        .parse()
        .ok()
}

fn issue_file(number: u64) -> String {
    format!("{ISSUE_PREFIX}{number}.txt")
}

/// `(url, owner, repo, number)` of a pull request link, with any trailing
/// page (`/files`), query or fragment dropped.
fn canonical_link(link: &str) -> Result<(String, String, String, String), PoolError> {
    let bad = || PoolError::NotPullRequestLink(link.trim().to_string());
    let trimmed = link.trim();
    let path = trimmed
        .strip_prefix("http://")
        .map(|rest| format!("https://{rest}"))
        .unwrap_or_else(|| trimmed.to_string());
    let path = path.split(['?', '#']).next().unwrap_or_default();
    let (owner, repo, number) = github::parse_pr_url(path).ok_or_else(bad)?;
    if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad());
    }
    Ok((
        format!("https://github.com/{owner}/{repo}/pull/{number}"),
        owner.to_string(),
        repo.to_string(),
        number.to_string(),
    ))
}

#[cfg(test)]
mod tests {
    // Test code asserts rather than propagating; see Rust code review.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]

    use super::*;

    #[test]
    fn issue_file_names_round_trip() {
        assert_eq!(issue_number(&issue_file(42)), Some(42));
        assert_eq!(issue_number("pr-42.txt"), None);
        assert_eq!(issue_number("issue-x.txt"), None);
    }

    #[test]
    fn links_are_reduced_to_the_pull_request() {
        for link in [
            "https://github.com/up/repo/pull/7",
            "  https://github.com/up/repo/pull/7/files\n",
            "https://github.com/up/repo/pull/7#issuecomment-1",
            "https://github.com/up/repo/pull/7?diff=split",
            "http://github.com/up/repo/pull/7",
        ] {
            let (url, owner, repo, number) = canonical_link(link).unwrap();
            assert_eq!(url, "https://github.com/up/repo/pull/7", "{link}");
            assert_eq!(
                (owner.as_str(), repo.as_str(), number.as_str()),
                ("up", "repo", "7")
            );
        }
    }

    #[test]
    fn other_links_are_refused() {
        for link in [
            "",
            "up/repo#7",
            "https://github.com/up/repo/issues/7",
            "https://github.com/up/repo/pull/seven",
            "https://gitlab.com/up/repo/pull/7",
        ] {
            assert!(
                matches!(canonical_link(link), Err(PoolError::NotPullRequestLink(_))),
                "{link}"
            );
        }
    }
}

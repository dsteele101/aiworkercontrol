//! Taking over a pull request opened elsewhere: it joins the queue in review,
//! where feedback on it is reworked like on any other item's.

use work_queue::{State as ItemState, WorkItem};

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
            title: Some(info.title.clone()),
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

    /// Tops up each project's open items with issues from its issue source:
    /// as many as it takes to have the source's `limit` imported issues
    /// waiting, chosen in its order and added as `issue-<number>.txt`. An
    /// issue already in the queue in any state is not brought in again.
    /// Returns the new items, and why any project's issues could not be read.
    pub async fn import_issues(&self) -> (Vec<WorkItem>, Vec<PoolError>) {
        let mut added = Vec::new();
        let mut errors = Vec::new();
        for project in self.config().projects {
            let Some(source) = &project.issues else {
                continue;
            };
            match self.import_project_issues(&project.name, source).await {
                Ok(items) => added.extend(items),
                Err(e) => errors.push(e),
            }
        }
        if !added.is_empty() {
            self.shared.changed();
        }
        (added, errors)
    }

    async fn import_project_issues(
        &self,
        project: &str,
        source: &IssueSource,
    ) -> Result<Vec<WorkItem>, PoolError> {
        let queue = self.queue();
        let waiting = queue
            .list(ItemState::Open)?
            .iter()
            .filter(|i| i.project == project && i.file.starts_with(ISSUE_PREFIX))
            .count();
        let room = (source.limit as usize).saturating_sub(waiting);
        if room == 0 {
            return Ok(Vec::new());
        }
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
        let mut open = issues::fetch_open(source).await?;
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
            ItemMeta {
                title: Some(issue.title.clone()),
                issue_url: Some(issue.url.clone()),
                ..ItemMeta::default()
            }
            .save(queue, ItemState::Open, &item)?;
            self.shared
                .record(&item, EventKind::Imported, Some(issue.url));
            added.push(item);
        }
        Ok(added)
    }
}

/// What names the items made from issues.
const ISSUE_PREFIX: &str = "issue-";

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

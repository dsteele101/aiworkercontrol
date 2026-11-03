//! Keeping the issue an item came from informed, for projects whose issue
//! source asks for it: a comment when an agent picks the item up, then a
//! summary with the pull request's link and the issue closed once its work
//! is merged. A failure to reach the issue is shown but stops nothing.

use work_queue::{RESULT_SUFFIX, State as ItemState, WorkItem};

use super::Shared;
use crate::github;
use crate::{ItemMeta, PoolError, Project};

/// The most of the agent's summary quoted in the closing comment.
const MAX_SUMMARY: usize = 3000;

impl Shared {
    /// Tells the issue of the in-progress `item` that an agent has picked it
    /// up, once.
    pub(super) async fn note_pickup(&self, item: &WorkItem, project: &Project) {
        let meta = self.item_meta(ItemState::InProgress, item);
        let Some(url) = reporting_issue(project, &meta).filter(|_| !meta.issue_noted) else {
            return;
        };
        let body = "An AI worker has picked up this issue and is working on it. \
                    A summary and the pull request link will follow here.";
        let gh = github::program(self.config.borrow().cli_dir.as_deref());
        let result = github::comment_on_issue(&gh, &url, body).await;
        if self.shown(item, result) {
            let mut meta = meta;
            meta.issue_noted = true;
            let saved = meta.save(&self.queue, ItemState::InProgress, item);
            self.shown(item, saved);
        }
    }

    /// Comments on the issue of the finished `item`, now in `state`, with
    /// what was done and where, then closes it.
    pub(super) async fn report_done(&self, item: &WorkItem, state: ItemState, project: &Project) {
        let meta = self.item_meta(state, item);
        let Some(url) = reporting_issue(project, &meta) else {
            return;
        };
        let summary = std::fs::read_to_string(self.queue.sidecar_path(state, item, RESULT_SUFFIX))
            .ok()
            .and_then(|report| agent_summary(&report));
        let body = completion_comment(meta.pr_url.as_deref(), summary.as_deref());
        let gh = github::program(self.config.borrow().cli_dir.as_deref());
        let commented = github::comment_on_issue(&gh, &url, &body).await;
        self.shown(item, commented);
        let closed = github::close_issue(&gh, &url).await;
        self.shown(item, closed);
    }

    /// `item`'s metadata, or the default when it cannot be read.
    fn item_meta(&self, state: ItemState, item: &WorkItem) -> ItemMeta {
        ItemMeta::load(&self.queue, state, item).unwrap_or_default()
    }

    /// Whether `result` succeeded; a failure is shown as the pool's last error.
    fn shown(&self, item: &WorkItem, result: Result<(), PoolError>) -> bool {
        match result {
            Ok(()) => true,
            Err(e) => {
                self.lock().last_error = Some(format!("{item}: issue update: {e}"));
                false
            }
        }
    }
}

/// The link of the issue behind `meta`'s item, when its project wants it
/// kept informed and the link is a GitHub issue's.
fn reporting_issue(project: &Project, meta: &ItemMeta) -> Option<String> {
    let reports = project.issues.as_ref().is_some_and(|s| s.report_back);
    let url = meta
        .issue_url
        .as_deref()
        .filter(|u| github::is_issue_url(u))?;
    reports.then(|| url.to_string())
}

/// The closing comment: the pull request that resolves the issue, and what
/// the agent said it did.
fn completion_comment(pr_url: Option<&str>, summary: Option<&str>) -> String {
    let mut body = match pr_url {
        Some(url) => format!("This issue was resolved by {url}, which has been merged.\n"),
        None => "An AI worker has finished this issue.\n".to_string(),
    };
    if let Some(summary) = summary {
        body.push_str("\n## Summary\n\n");
        body.push_str(summary);
        body.push('\n');
    }
    body
}

/// The agent's summary in a result report, shortened.
fn agent_summary(report: &str) -> Option<String> {
    let start = report.find("\n## Agent summary")?;
    let section = &report[start + 1..];
    let text = section.split_once('\n')?.1;
    let text = text.split("\n## ").next().unwrap_or(text).trim();
    if text.is_empty() {
        return None;
    }
    let mut cut = text.len().min(MAX_SUMMARY);
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    Some(if cut < text.len() {
        format!("{}…", &text[..cut])
    } else {
        text.to_string()
    })
}

#[cfg(test)]
mod tests {
    // Test code asserts rather than propagating; see Rust code review.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]

    use super::*;
    use crate::{IssueOrder, IssueSource};

    fn project(report_back: bool) -> Project {
        let mut project = Project::new("p", "spec", "repo");
        project.issues = Some(IssueSource {
            url: "https://github.com/o/r/issues".into(),
            limit: 1,
            order: IssueOrder::Fifo,
            token: None,
            report_back,
        });
        project
    }

    fn meta(url: &str) -> ItemMeta {
        ItemMeta {
            issue_url: Some(url.into()),
            ..ItemMeta::default()
        }
    }

    #[test]
    fn only_asked_for_github_issues_are_reported_to() {
        let url = "https://github.com/o/r/issues/4";
        assert_eq!(
            reporting_issue(&project(true), &meta(url)).as_deref(),
            Some(url)
        );
        assert_eq!(reporting_issue(&project(false), &meta(url)), None);
        assert_eq!(
            reporting_issue(&project(true), &meta("https://gitlab.com/g/p/-/issues/4")),
            None
        );
        assert_eq!(reporting_issue(&project(true), &ItemMeta::default()), None);
    }

    #[test]
    fn the_summary_is_the_agent_section_of_the_report() {
        let report = "# issue-4.txt\n\n- Project: p\n\n## Commits\n\n- abc\n\n\
                      ## Agent summary (rework)\n\nFixed it.\n\nMore.\n\n## Other\n\nx\n";
        assert_eq!(agent_summary(report).as_deref(), Some("Fixed it.\n\nMore."));
        assert_eq!(agent_summary("# r\n\n## Commits\n"), None);
    }

    #[test]
    fn the_closing_comment_links_the_pull_request() {
        let body = completion_comment(Some("https://github.com/o/r/pull/9"), Some("Done."));
        assert!(body.contains("https://github.com/o/r/pull/9"));
        assert!(body.ends_with("Done.\n"));
    }
}

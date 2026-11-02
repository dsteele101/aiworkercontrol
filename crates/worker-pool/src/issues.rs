//! Reading the open issues of a git hosting service, for taking them in as
//! work items. One adapter per service family, picked from the issues link:
//! GitHub, GitLab, Bitbucket Cloud, and Gitea/Forgejo (the default for any
//! other host).

use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::process::Command;

use crate::PoolError;

/// The most pages of issues read in one go, so a huge tracker cannot keep a
/// poll busy for long.
const MAX_PAGES: u32 = 10;

/// The script behind a page read. The token header travels in the
/// environment, not the arguments, so `ps` does not show it.
const FETCH_SCRIPT: &str = r#"curl -sf --max-time 30 -H "Accept: application/json" ${AIWC_AUTH:+-H "$AIWC_AUTH"} "$AIWC_URL""#;

/// Where a project's issues come from, and how they join the queue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueSource {
    /// The issues page of the repository, e.g.
    /// `https://github.com/phase-rs/phase/issues`.
    pub url: String,
    /// How many imported issues are kept waiting in `open` at once; more are
    /// brought in as workers take them.
    pub limit: u32,
    /// Which of the open issues come first.
    #[serde(default)]
    pub order: IssueOrder,
    /// An access token, for private repositories and higher rate limits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}

/// Which open issue is brought in next.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IssueOrder {
    /// The oldest issue first.
    #[default]
    Fifo,
    /// The newest issue first.
    Lifo,
    /// Any issue, chosen at random.
    Random,
}

impl IssueOrder {
    /// Every order, as offered to the user.
    pub const ALL: [IssueOrder; 3] = [IssueOrder::Fifo, IssueOrder::Lifo, IssueOrder::Random];

    /// The name shown to the user.
    pub fn label(self) -> &'static str {
        match self {
            IssueOrder::Fifo => "Oldest first (FIFO)",
            IssueOrder::Lifo => "Newest first (LIFO)",
            IssueOrder::Random => "Random",
        }
    }

    /// The name used in forms.
    pub fn value(self) -> &'static str {
        match self {
            IssueOrder::Fifo => "fifo",
            IssueOrder::Lifo => "lifo",
            IssueOrder::Random => "random",
        }
    }
}

/// One open issue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Issue {
    /// The number shown to users (`#12`); grows with each new issue.
    pub number: u64,
    pub title: String,
    pub body: String,
    pub url: String,
}

/// `items` in the order issues are to be taken.
pub(crate) fn ordered(mut issues: Vec<Issue>, order: IssueOrder) -> Vec<Issue> {
    match order {
        IssueOrder::Fifo => issues.sort_by_key(|i| i.number),
        IssueOrder::Lifo => issues.sort_by_key(|i| std::cmp::Reverse(i.number)),
        IssueOrder::Random => {
            let keys = RandomState::new();
            issues.sort_by_key(|i| keys.hash_one(i.number));
        }
    }
    issues
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Forge {
    GitHub,
    GitLab,
    Bitbucket,
    Gitea,
}

/// A repository on a hosting service.
#[derive(Debug, PartialEq, Eq)]
struct Tracker {
    forge: Forge,
    host: String,
    /// `owner/name`, or `group/subgroup/name` on GitLab.
    path: String,
}

impl Tracker {
    /// Reads an issues link: `https://HOST/OWNER/REPO` with an optional
    /// `/issues` (or GitLab's `/-/issues`) page and query after it.
    fn parse(link: &str) -> Option<Self> {
        let link = link.trim();
        let rest = link
            .strip_prefix("https://")
            .or_else(|| link.strip_prefix("http://"))?;
        let rest = rest.split(['?', '#']).next()?;
        let (host, path) = rest.split_once('/')?;
        let mut parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
        if let Some(at) = parts.iter().position(|p| *p == "issues") {
            parts.truncate(at);
            if parts.last() == Some(&"-") {
                parts.pop();
            }
        }
        if let Some(last) = parts.last_mut() {
            *last = last.strip_suffix(".git").unwrap_or(last);
        }
        if parts.len() < 2 || host.is_empty() {
            return None;
        }
        let lower = host.to_ascii_lowercase();
        let forge = if lower.contains("github") {
            Forge::GitHub
        } else if lower.contains("gitlab") {
            Forge::GitLab
        } else if lower.contains("bitbucket") {
            Forge::Bitbucket
        } else {
            Forge::Gitea
        };
        // Only GitLab nests repositories in groups.
        if forge != Forge::GitLab && parts.len() != 2 {
            return None;
        }
        Some(Self {
            forge,
            host: host.to_string(),
            path: parts.join("/"),
        })
    }

    /// The API address of page `page` (from 1) of the open issues.
    fn page_url(&self, page: u32) -> String {
        let (host, path) = (&self.host, &self.path);
        match self.forge {
            Forge::GitHub => {
                let api = if host.eq_ignore_ascii_case("github.com") {
                    "https://api.github.com".to_string()
                } else {
                    format!("https://{host}/api/v3")
                };
                format!("{api}/repos/{path}/issues?state=open&per_page=100&page={page}")
            }
            Forge::GitLab => format!(
                "https://{host}/api/v4/projects/{}/issues?state=opened&per_page=100&page={page}",
                path.replace('/', "%2F")
            ),
            Forge::Bitbucket => format!(
                "https://api.bitbucket.org/2.0/repositories/{path}/issues\
                 ?q=state%3D%22new%22+OR+state%3D%22open%22&pagelen=100&page={page}"
            ),
            Forge::Gitea => {
                format!(
                    "https://{host}/api/v1/repos/{path}/issues?state=open&type=issues&limit=50&page={page}"
                )
            }
        }
    }

    /// The header that carries `token`.
    fn auth(&self, token: &str) -> String {
        match self.forge {
            Forge::GitLab => format!("PRIVATE-TOKEN: {token}"),
            _ => format!("Authorization: Bearer {token}"),
        }
    }

    /// The issues in one page of the API's answer. Pull requests, which
    /// GitHub lists among the issues, are left out.
    fn parse_page(&self, page: &Value) -> Vec<Issue> {
        let list = match self.forge {
            Forge::Bitbucket => page.get("values"),
            _ => Some(page),
        };
        list.and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|v| v.get("pull_request").is_none())
            .filter_map(|v| match self.forge {
                Forge::GitLab => Some(Issue {
                    number: v.get("iid")?.as_u64()?,
                    title: text(v, "title").to_string(),
                    body: text(v, "description").to_string(),
                    url: text(v, "web_url").to_string(),
                }),
                Forge::Bitbucket => Some(Issue {
                    number: v.get("id")?.as_u64()?,
                    title: text(v, "title").to_string(),
                    body: v
                        .pointer("/content/raw")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    url: v
                        .pointer("/links/html/href")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                }),
                Forge::GitHub | Forge::Gitea => Some(Issue {
                    number: v.get("number")?.as_u64()?,
                    title: text(v, "title").to_string(),
                    body: text(v, "body").to_string(),
                    url: text(v, "html_url").to_string(),
                }),
            })
            .collect()
    }
}

/// Reads the open issues behind `source`'s link, up to [`MAX_PAGES`] pages.
pub(crate) async fn fetch_open(source: &IssueSource) -> Result<Vec<Issue>, PoolError> {
    let fail = |reason: String| PoolError::Issues {
        url: source.url.clone(),
        reason,
    };
    let tracker = Tracker::parse(&source.url).ok_or_else(|| {
        fail("not a repository link (https://HOST/OWNER/REPO/issues)".to_string())
    })?;
    let auth = source
        .token
        .as_deref()
        .filter(|t| !t.is_empty())
        .map(|t| tracker.auth(t));
    let mut issues = Vec::new();
    for page in 1..=MAX_PAGES {
        let mut command = Command::new("sh");
        command
            .args(["-c", FETCH_SCRIPT])
            .env("AIWC_URL", tracker.page_url(page))
            .kill_on_drop(true);
        if let Some(auth) = &auth {
            command.env("AIWC_AUTH", auth);
        }
        let output = command
            .output()
            .await
            .map_err(|e| fail(format!("could not run curl: {e}")))?;
        if !output.status.success() {
            return Err(fail(format!(
                "the request for page {page} failed ({}); is the link right, and is a token needed?",
                output.status
            )));
        }
        let body: Value = serde_json::from_slice(&output.stdout)
            .map_err(|e| fail(format!("unexpected answer: {e}")))?;
        let found = tracker.parse_page(&body);
        let more = !found.is_empty() || has_entries(&body);
        issues.extend(found);
        if !more {
            break;
        }
    }
    Ok(issues)
}

fn text<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or_default()
}

/// Whether a page had entries at all (a page of only pull requests is not
/// the last one).
fn has_entries(page: &Value) -> bool {
    let list = page.get("values").unwrap_or(page);
    list.as_array().is_some_and(|l| !l.is_empty())
}

#[cfg(test)]
mod tests {
    // Test code asserts rather than propagating; see Rust code review.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]

    use super::*;

    fn issue(number: u64) -> Issue {
        Issue {
            number,
            title: String::new(),
            body: String::new(),
            url: String::new(),
        }
    }

    fn numbers(issues: &[Issue]) -> Vec<u64> {
        issues.iter().map(|i| i.number).collect()
    }

    #[test]
    fn links_name_their_service_and_repository() {
        let github =
            Tracker::parse("https://github.com/phase-rs/phase/issues?q=is%3Aopen").unwrap();
        assert_eq!(github.forge, Forge::GitHub);
        assert_eq!(github.path, "phase-rs/phase");
        let gitlab = Tracker::parse("https://gitlab.com/group/sub/proj/-/issues").unwrap();
        assert_eq!(gitlab.forge, Forge::GitLab);
        assert_eq!(gitlab.path, "group/sub/proj");
        let bitbucket = Tracker::parse("https://bitbucket.org/team/repo/issues/").unwrap();
        assert_eq!(bitbucket.forge, Forge::Bitbucket);
        let gitea = Tracker::parse("https://codeberg.org/me/repo.git").unwrap();
        assert_eq!(gitea.forge, Forge::Gitea);
        assert_eq!(gitea.path, "me/repo");
        assert_eq!(Tracker::parse("https://github.com/only-owner"), None);
        assert_eq!(Tracker::parse("not a link"), None);
    }

    #[test]
    fn api_addresses_follow_each_service() {
        let page = |link: &str| Tracker::parse(link).unwrap().page_url(2);
        assert_eq!(
            page("https://github.com/o/r/issues"),
            "https://api.github.com/repos/o/r/issues?state=open&per_page=100&page=2"
        );
        assert!(page("https://git.corp.example/o/r").contains("/api/v1/repos/o/r/issues"));
        assert!(
            page("https://gitlab.com/g/s/p/-/issues").contains("/api/v4/projects/g%2Fs%2Fp/issues")
        );
        assert!(page("https://bitbucket.org/t/r").starts_with("https://api.bitbucket.org/2.0/"));
    }

    #[test]
    fn github_pages_leave_out_pull_requests() {
        let tracker = Tracker::parse("https://github.com/o/r").unwrap();
        let page: Value = serde_json::from_str(
            r#"[{"number":3,"title":"Bug","body":"It breaks","html_url":"u3"},
                {"number":4,"title":"PR","body":null,"html_url":"u4","pull_request":{}}]"#,
        )
        .unwrap();
        let issues = tracker.parse_page(&page);
        assert_eq!(numbers(&issues), [3]);
        assert_eq!(issues[0].body, "It breaks");
    }

    #[test]
    fn gitlab_and_bitbucket_pages_are_read() {
        let gitlab = Tracker::parse("https://gitlab.com/g/p").unwrap();
        let page: Value =
            serde_json::from_str(r#"[{"iid":7,"title":"T","description":"D","web_url":"w"}]"#)
                .unwrap();
        assert_eq!(numbers(&gitlab.parse_page(&page)), [7]);
        let bitbucket = Tracker::parse("https://bitbucket.org/t/r").unwrap();
        let page: Value = serde_json::from_str(
            r#"{"values":[{"id":9,"title":"T","content":{"raw":"D"},"links":{"html":{"href":"h"}}}]}"#,
        )
        .unwrap();
        let issues = bitbucket.parse_page(&page);
        assert_eq!((issues[0].number, issues[0].body.as_str()), (9, "D"));
    }

    #[test]
    fn issues_come_in_the_chosen_order() {
        let all = || vec![issue(5), issue(2), issue(9)];
        assert_eq!(numbers(&ordered(all(), IssueOrder::Fifo)), [2, 5, 9]);
        assert_eq!(numbers(&ordered(all(), IssueOrder::Lifo)), [9, 5, 2]);
        let mut random = numbers(&ordered(all(), IssueOrder::Random));
        random.sort_unstable();
        assert_eq!(random, [2, 5, 9]);
    }
}

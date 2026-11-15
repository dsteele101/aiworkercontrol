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

/// The script behind a web page read: any redirect is followed, but only
/// http(s) is ever fetched.
const PAGE_SCRIPT: &str = r#"curl -sfL --proto '=http,https' --proto-redir '=http,https' --max-time 30 --max-filesize 5000000 "$AIWC_URL""#;

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
    /// Keep the issue informed: comment when an agent picks it up, and when
    /// its pull request is merged comment with a summary and the link, then
    /// close it. Done through `gh`, so for GitHub issues only.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub report_back: bool,
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
            .filter_map(|v| self.parse_issue(v))
            .collect()
    }

    /// One issue as the service's API describes it.
    fn parse_issue(&self, v: &Value) -> Option<Issue> {
        match self.forge {
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
        }
    }

    /// The API address of issue `number`.
    fn issue_url(&self, number: u64) -> String {
        let (host, path) = (&self.host, &self.path);
        match self.forge {
            Forge::GitHub => {
                let api = if host.eq_ignore_ascii_case("github.com") {
                    "https://api.github.com".to_string()
                } else {
                    format!("https://{host}/api/v3")
                };
                format!("{api}/repos/{path}/issues/{number}")
            }
            Forge::GitLab => format!(
                "https://{host}/api/v4/projects/{}/issues/{number}",
                path.replace('/', "%2F")
            ),
            Forge::Bitbucket => {
                format!("https://api.bitbucket.org/2.0/repositories/{path}/issues/{number}")
            }
            Forge::Gitea => format!("https://{host}/api/v1/repos/{path}/issues/{number}"),
        }
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
        let body = fetch_json(&tracker.page_url(page), auth.as_deref())
            .await
            .map_err(|reason| fail(format!("page {page}: {reason}")))?;
        let found = tracker.parse_page(&body);
        let more = !found.is_empty() || has_entries(&body);
        issues.extend(found);
        if !more {
            break;
        }
    }
    Ok(issues)
}

/// An issue taken in by its link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LinkedIssue {
    /// The issue's number when the link is to one on a known service; items
    /// from it are named like those the issue source brings in.
    pub number: Option<u64>,
    pub title: String,
    pub body: String,
    /// The canonical link when the service gave one, else the link as given.
    pub url: String,
    /// `owner/name` of the repository, when the link names one on a service.
    pub repo: Option<String>,
}

/// The repository and issue number in a link to one issue on a git hosting
/// service: `https://HOST/OWNER/REPO/issues/N` (GitLab: `/-/issues/N`).
fn issue_link(link: &str) -> Option<(Tracker, u64)> {
    let tracker = Tracker::parse(link)?;
    let rest = link.trim().split_once("://")?.1;
    let rest = rest.split(['?', '#']).next()?;
    let mut parts = rest.split('/').skip(1);
    let number = parts
        .by_ref()
        .skip_while(|p| *p != "issues")
        .nth(1)?
        .parse()
        .ok()?;
    Some((tracker, number))
}

/// `owner/name` of the repository a link to an issue is in, if it is one on
/// a known service's layout.
pub(crate) fn linked_repo(link: &str) -> Option<String> {
    issue_link(link).map(|(t, _)| t.path)
}

/// Whether `link`'s repository is the one `issues_link` (a project's issue
/// source) is for.
pub(crate) fn same_repo(link: &str, issues_link: &str) -> bool {
    match (Tracker::parse(link), Tracker::parse(issues_link)) {
        (Some(a), Some(b)) => {
            a.host.eq_ignore_ascii_case(&b.host) && a.path.eq_ignore_ascii_case(&b.path)
        }
        _ => false,
    }
}

/// Reads the issue or page at `link`. A link to one issue on GitHub, GitLab,
/// Bitbucket Cloud or Gitea/Forgejo is read through the service's API, with
/// `token` when given; any other page is read as a web page, its text being
/// the description.
pub(crate) async fn fetch_link(link: &str, token: Option<&str>) -> Result<LinkedIssue, PoolError> {
    let link = link.trim();
    let fail = |reason: String| PoolError::Issues {
        url: link.to_string(),
        reason,
    };
    if !(link.starts_with("https://") || link.starts_with("http://"))
        || link.contains(char::is_whitespace)
    {
        return Err(fail("not a web link (https://HOST/...)".to_string()));
    }
    if let Some((tracker, number)) = issue_link(link) {
        let auth = token.filter(|t| !t.is_empty()).map(|t| tracker.auth(t));
        let api = fetch_json(&tracker.issue_url(number), auth.as_deref()).await;
        // A host not running a known service's API is read as a plain page.
        if let Some(issue) = api.ok().and_then(|v| tracker.parse_issue(&v)) {
            return Ok(LinkedIssue {
                number: Some(issue.number),
                title: issue.title,
                body: issue.body,
                url: if issue.url.is_empty() {
                    link.to_string()
                } else {
                    issue.url
                },
                repo: Some(tracker.path),
            });
        }
    }
    let page = fetch_text(link).await.map_err(fail)?;
    let (title, body) = page_text(&page);
    Ok(LinkedIssue {
        number: None,
        title: title.unwrap_or_else(|| link.to_string()),
        body,
        url: link.to_string(),
        repo: None,
    })
}

/// The answer to a request for `url` as JSON.
async fn fetch_json(url: &str, auth: Option<&str>) -> Result<Value, String> {
    let mut command = Command::new("sh");
    command
        .args(["-c", FETCH_SCRIPT])
        .env("AIWC_URL", url)
        .kill_on_drop(true);
    if let Some(auth) = auth {
        command.env("AIWC_AUTH", auth);
    }
    let output = command
        .output()
        .await
        .map_err(|e| format!("could not run curl: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "the request failed ({}); is the link right, and is a token needed?",
            output.status
        ));
    }
    serde_json::from_slice(&output.stdout).map_err(|e| format!("unexpected answer: {e}"))
}

/// The body of the page at `url`, following redirects, over http(s) only.
async fn fetch_text(url: &str) -> Result<String, String> {
    let output = Command::new("sh")
        .args(["-c", PAGE_SCRIPT])
        .env("AIWC_URL", url)
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|e| format!("could not run curl: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "the request failed ({}); is the link right, and does it need a login?",
            output.status
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The most text kept from a page.
const MAX_PAGE_TEXT: usize = 20_000;

/// The title and readable text of a fetched page. Anything that is not HTML
/// is taken as it is.
fn page_text(page: &str) -> (Option<String>, String) {
    let lower = page.to_ascii_lowercase();
    if !lower.contains("<html") && !lower.contains("<body") && !lower.contains("<!doctype") {
        return (None, truncate(page.trim()));
    }
    let title = lower.find("<title").and_then(|at| {
        let start = lower[at..].find('>')? + at + 1;
        let end = lower[start..].find("</title")? + start;
        let title = collapse(&decode(&page[start..end]));
        (!title.is_empty()).then_some(title)
    });
    let mut out = String::new();
    let mut at = 0;
    while at < page.len() {
        let Some(open) = lower[at..].find('<').map(|i| i + at) else {
            out.push_str(&page[at..]);
            break;
        };
        out.push_str(&page[at..open]);
        // Scripts, styles and the head hold nothing to read.
        let skipped = ["script", "style", "head", "noscript", "svg"]
            .iter()
            .find(|t| lower[open + 1..].starts_with(*t))
            .and_then(|t| lower[open..].find(&format!("</{t}")).map(|i| (t, i + open)));
        let close = skipped.map_or(open, |(_, i)| i);
        at = lower[close..]
            .find('>')
            .map_or(page.len(), |i| i + close + 1);
        // Block-level tags break lines; inline ones are dropped.
        let name: String = lower[open + 1..]
            .trim_start_matches('/')
            .chars()
            .take_while(char::is_ascii_alphanumeric)
            .collect();
        if ![
            "a", "b", "i", "u", "em", "strong", "span", "code", "small", "sup", "sub",
        ]
        .contains(&name.as_str())
        {
            out.push('\n');
        }
    }
    let lines: Vec<String> = decode(&out)
        .lines()
        .map(collapse)
        .filter(|l| !l.is_empty())
        .collect();
    (title, truncate(&lines.join("\n")))
}

fn collapse(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn decode(s: &str) -> String {
    s.replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

fn truncate(s: &str) -> String {
    match s.char_indices().nth(MAX_PAGE_TEXT) {
        Some((at, _)) => format!("{}\n[truncated]", &s[..at]),
        None => s.to_string(),
    }
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
    fn issue_links_name_the_repository_and_number() {
        let (t, n) = issue_link("https://github.com/o/r/issues/12#issuecomment-1").unwrap();
        assert_eq!((t.path.as_str(), n), ("o/r", 12));
        let (t, n) = issue_link(" https://gitlab.com/g/s/p/-/issues/7?x=1 ").unwrap();
        assert_eq!((t.path.as_str(), n), ("g/s/p", 7));
        assert_eq!(
            t.issue_url(7),
            "https://gitlab.com/api/v4/projects/g%2Fs%2Fp/issues/7"
        );
        assert_eq!(
            Tracker::parse("https://github.com/o/r")
                .unwrap()
                .issue_url(3),
            "https://api.github.com/repos/o/r/issues/3"
        );
        // A listing, or a page of something else, has no issue number.
        assert!(issue_link("https://github.com/o/r/issues").is_none());
        assert!(issue_link("https://example.com/").is_none());
        assert!(same_repo(
            "https://github.com/O/r/issues/3",
            "https://github.com/o/r/issues"
        ));
        assert!(!same_repo(
            "https://github.com/o/x/issues/3",
            "https://github.com/o/r/issues"
        ));
    }

    #[test]
    fn web_pages_are_read_as_text() {
        let (title, body) = page_text(
            "<html><head><title>Fix &amp; ship</title><style>p{}</style></head>\
             <body><h1>Crash</h1><script>var x=1;</script><p>It  breaks on <b>start</b>.</p></body></html>",
        );
        assert_eq!(title.as_deref(), Some("Fix & ship"));
        assert_eq!(body, "Crash\nIt breaks on start.");
        let (title, body) = page_text("plain text\n");
        assert_eq!((title, body.as_str()), (None, "plain text"));
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

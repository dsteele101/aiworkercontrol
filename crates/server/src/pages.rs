//! HTML pages.

use std::collections::BTreeMap;
use std::time::SystemTime;

use agent_backend::{AgentEvent, AuthStatus, Cli, Provider};
use maud::{DOCTYPE, Markup, html};
use work_queue::{State as ItemState, WorkItem};
use worker_pool::Activity;

use crate::routes::{MAX_TURNS, MAX_WORKERS};
use crate::{AppState, Settings};

/// Agent events shown per worker card.
const CARD_EVENTS: usize = 8;
/// Longest event text shown before it is cut.
const EVENT_CHARS: usize = 240;

const STYLE: &str = r#"
:root {
  --bg: #f6f7f9; --panel: #ffffff; --text: #1d2330; --muted: #677085;
  --border: #dde1e8; --accent: #2f5bd3; --ok: #1f7a4d; --bad: #b3261e;
  --warn: #8a5a00; --code: #eef1f5;
}
@media (prefers-color-scheme: dark) {
  :root {
    --bg: #12151b; --panel: #1b1f27; --text: #e4e7ee; --muted: #9aa3b5;
    --border: #2c323e; --accent: #7c9cf5; --ok: #5cc48f; --bad: #f2867d;
    --warn: #e0b45c; --code: #232833;
  }
}
* { box-sizing: border-box; }
body { margin: 0; background: var(--bg); color: var(--text);
  font: 15px/1.45 system-ui, -apple-system, "Segoe UI", sans-serif; }
header { display: flex; gap: 20px; align-items: baseline; padding: 14px 16px;
  border-bottom: 1px solid var(--border); background: var(--panel); }
header h1 { font-size: 17px; margin: 0; }
header a { color: var(--muted); text-decoration: none; }
header a.here { color: var(--text); font-weight: 600; }
main { max-width: 1200px; margin: 0 auto; padding: 16px; }
h2 { font-size: 15px; margin: 24px 0 10px; text-transform: uppercase;
  letter-spacing: .04em; color: var(--muted); }
.panel { background: var(--panel); border: 1px solid var(--border);
  border-radius: 8px; padding: 14px; }
.bar { display: flex; flex-wrap: wrap; gap: 12px 24px; align-items: center; }
.notice { background: var(--code); border-left: 3px solid var(--accent);
  padding: 10px 12px; border-radius: 6px; margin-bottom: 14px; }
.grid { display: grid; gap: 12px;
  grid-template-columns: repeat(auto-fill, minmax(340px, 1fr)); }
.card h3 { margin: 0 0 6px; font-size: 15px; display: flex; justify-content: space-between; }
.muted { color: var(--muted); }
.ok { color: var(--ok); } .bad { color: var(--bad); } .warn { color: var(--warn); }
ul.events { list-style: none; margin: 8px 0 0; padding: 0; font-size: 13px; }
ul.events li { padding: 3px 0; border-top: 1px solid var(--border);
  overflow-wrap: anywhere; }
code, pre { font-family: ui-monospace, "SF Mono", Menlo, monospace; font-size: 13px; }
code { background: var(--code); padding: 1px 4px; border-radius: 4px; }
pre { background: var(--code); padding: 12px; border-radius: 6px;
  white-space: pre-wrap; overflow-wrap: anywhere; }
table { width: 100%; border-collapse: collapse; }
td, th { text-align: left; padding: 6px 8px; border-top: 1px solid var(--border);
  vertical-align: top; overflow-wrap: anywhere; }
th { color: var(--muted); font-weight: 500; border-top: 0; }
form.inline { display: inline; }
button, input, select { font: inherit; color: inherit; }
button { background: var(--panel); border: 1px solid var(--border);
  border-radius: 6px; padding: 4px 12px; cursor: pointer; }
button.primary { background: var(--accent); border-color: var(--accent); color: #fff; }
input[type=text], input[type=password], input[type=number], select {
  background: var(--bg); border: 1px solid var(--border); border-radius: 6px;
  padding: 5px 8px; width: 100%; }
label { display: block; margin: 8px 0 2px; font-size: 13px; color: var(--muted); }
label.check { display: flex; gap: 6px; align-items: center; color: var(--text); }
label.check input { width: auto; }
.fields { display: grid; gap: 4px 12px;
  grid-template-columns: repeat(auto-fill, minmax(220px, 1fr)); }
.stack > * + * { margin-top: 12px; }
@media (max-width: 600px) { header { flex-wrap: wrap; gap: 10px 16px; } }
"#;

fn layout(title: &str, here: &str, refresh: bool, notice: Option<String>, body: Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                @if refresh { meta http-equiv="refresh" content="2"; }
                title { (title) " · AI Worker Control" }
                style { (maud::PreEscaped(STYLE)) }
            }
            body {
                header {
                    h1 { "AI Worker Control" }
                    a href="/" class=[(here == "dashboard").then_some("here")] { "Dashboard" }
                    a href="/settings" class=[(here == "settings").then_some("here")] { "Settings" }
                }
                main {
                    @if let Some(notice) = notice { div.notice { (notice) } }
                    (body)
                }
            }
        }
    }
}

pub(crate) fn dashboard(app: &AppState, notice: Option<String>) -> Markup {
    let snapshot = app.pool().snapshot();
    let settings = app.settings();
    let queue = app.pool().queue();
    let lists: BTreeMap<_, _> = ItemState::ALL
        .iter()
        .map(|s| (s.dir_name(), queue.list(*s).unwrap_or_default()))
        .collect();
    let max_turns = settings.max_turns.max(1);
    let meta = |state: ItemState, item: &WorkItem| app.pool().meta(state, item).unwrap_or_default();
    let provider = settings.provider(settings.active);
    let auth = match provider {
        Some(p) if p.use_api_key || !settings.active.supports_plan() => "API key",
        _ => "plan login",
    };
    let known = |item: &WorkItem| settings.projects.iter().any(|p| p.name == item.project);
    let pool_projects = app.pool().config().projects;
    let has_worktree = |item: &WorkItem| {
        pool_projects
            .iter()
            .find(|p| p.name == item.project)
            .is_some_and(|p| p.worktree_path(item).exists())
    };

    let body = html! {
        div.panel.bar {
            div {
                @if snapshot.running { strong.ok { "● Running" } } @else { strong.muted { "○ Stopped" } }
            }
            div { (settings.active.label()) " " span.muted { "(" (auth) ")" } }
            div.bar {
                span { "Workers" }
                form.inline method="post" action="/workers" {
                    input type="hidden" name="count" value=(settings.workers.saturating_sub(1));
                    button disabled[settings.workers == 0] aria-label="Fewer workers" { "−" }
                }
                strong { (settings.workers) }
                form.inline method="post" action="/workers" {
                    input type="hidden" name="count" value=(settings.workers + 1);
                    button disabled[settings.workers >= MAX_WORKERS] aria-label="More workers" { "+" }
                }
            }
            div {
                @if snapshot.running {
                    form.inline method="post" action="/stop" { button { "Stop" } }
                } @else {
                    form.inline method="post" action="/start" { button.primary { "Start" } }
                }
            }
        }
        @if let Some(error) = &snapshot.last_error {
            p.bad { "Could not read the work queue: " (error) }
        }

        h2 { "Workers" }
        @if snapshot.workers.is_empty() {
            p.muted { "No workers running. Press Start to begin." }
        }
        div.grid {
            @for worker in &snapshot.workers {
                div.panel.card {
                    @match &worker.activity {
                        Activity::Idle => {
                            h3 { span { "Worker " (worker.id + 1) } span.muted { "idle" } }
                            @if worker.id >= snapshot.target_workers || !snapshot.running {
                                p.muted { "Stopping." }
                            } @else {
                                p.muted { "Waiting for open work." }
                            }
                        }
                        Activity::Working(job) => {
                            h3 { span { "Worker " (worker.id + 1) } span.muted { (elapsed(job.started)) } }
                            div { strong { (job.item) } " " span.muted { "· " (job.provider.label()) } }
                            ul.events {
                                @let skip = job.events.len().saturating_sub(CARD_EVENTS);
                                @for event in job.events.iter().skip(skip) { li { (event_line(event)) } }
                                @if job.events.is_empty() { li.muted { "Starting agent…" } }
                            }
                        }
                    }
                }
            }
        }

        h2 { "Queue" }
        div.panel {
            p.bar {
                @for (state, items) in &lists { span { (state) ": " strong { (items.len()) } } }
            }
            table {
                tr { th { "Item" } th { "State" } th { "" } }
                @for item in lists.get("inprogress").into_iter().flatten() {
                    tr { td { (item) } td.warn { "in progress" } td {} }
                }
                @for item in lists.get("open").into_iter().flatten() {
                    tr {
                        td { (item) }
                        @let m = meta(ItemState::Open, item);
                        @if let Some(error) = &m.last_error {
                            td.warn { "retrying (" (m.turns) " of " (max_turns) " turns used); last run: " (error) }
                            td {}
                        } @else if !known(item) {
                            td.muted { "no project “" (item.project) "” configured" }
                            td {}
                        } @else {
                            td { "open" } td {}
                        }
                    }
                }
                @for item in lists.get("hold").into_iter().flatten() {
                    tr {
                        td { (item) }
                        td.bad {
                            "on hold: "
                            (meta(ItemState::Hold, item).hold_reason.unwrap_or_else(|| "set aside for manual review".into()))
                        }
                        td {
                            form.inline method="post" action="/resume" {
                                input type="hidden" name="item" value=(item);
                                button title="Reset its turn count and give it back to the agents" { "Return to agent" }
                            }
                        }
                    }
                }
                @for item in lists.get("inreview").into_iter().flatten() {
                    @let m = meta(ItemState::InReview, item);
                    tr {
                        td { (item) }
                        @if m.needs_rework.is_some() {
                            td.warn {
                                "changes requested; waiting for a worker (" (m.turns) " of "
                                (max_turns) " turns used)"
                                @if let Some(error) = &m.last_error { "; last run: " (error) }
                            }
                        } @else {
                            td.warn { "in review (" (m.turns) " of " (max_turns) " turns used)" }
                        }
                        td {
                            @if let Some(url) = &m.pr_url {
                                a href=(url) target="_blank" rel="noopener" { "Pull request" }
                            }
                            " "
                            a href={ "/result?item=" (url_encode(&item.to_string())) } { "Result" }
                        }
                    }
                }
                @for item in lists.get("closed").into_iter().flatten() {
                    tr {
                        td { (item) }
                        td.ok { "closed" }
                        td {
                            a href={ "/result?item=" (url_encode(&item.to_string())) } { "Result" }
                            @if let Some(url) = app.pool().pull_request(ItemState::Closed, item) {
                                " " a href=(url) target="_blank" rel="noopener" { "Pull request" }
                            }
                            @if has_worktree(item) {
                                " "
                                form.inline method="post" action="/cleanup" {
                                    input type="hidden" name="item" value=(item);
                                    button title="Remove the worktree; delete the branch too if it is merged" { "Clean up" }
                                }
                            }
                        }
                    }
                }
            }
        }
    };
    layout("Dashboard", "dashboard", true, notice, body)
}

pub(crate) fn settings(
    app: &AppState,
    settings: &Settings,
    status: impl Fn(Cli) -> AuthStatus,
    notice: Option<String>,
) -> Markup {
    let body = html! {
        h2 { "Workers" }
        form.panel.bar method="post" action="/workers" {
            input type="hidden" name="back" value="settings";
            label for="count" { "Number of workers" }
            input #count type="number" name="count" min="0" max=(MAX_WORKERS) value=(settings.workers) style="width:6em";
            button { "Save" }
        }
        form.panel.bar method="post" action="/settings/max-turns" style="margin-top:12px" {
            label for="max_turns" { "Turns per item before hold" }
            input #max_turns type="number" name="max_turns" min="1" max=(MAX_TURNS) value=(settings.max_turns) style="width:6em";
            button { "Save" }
            span.muted {
                "Every agent run on an item is a turn, failed ones included; failures retry until "
                "the limit, then the item goes on hold for you to review and return."
            }
        }

        h2 { "AI system" }
        form.panel.bar method="post" action="/settings/active" {
            label for="active" { "Workers use" }
            select #active name="provider" style="width:auto" {
                @for p in Provider::ALL {
                    option value=(provider_value(p)) selected[p == settings.active] { (p.label()) }
                }
            }
            button { "Use" }
        }

        div.grid style="margin-top:12px" {
            @for p in &settings.providers {
                div.panel.stack {
                    h3 style="margin:0" {
                        (p.provider.label())
                        @if p.provider == settings.active { " " span.ok { "· active" } }
                    }
                    @if p.provider.supports_plan() {
                        (login_block(app, p.provider, status(p.provider.cli())))
                    } @else {
                        p.muted { "API key only; this provider has no plan login." }
                    }
                    form method="post" action="/settings/provider" {
                        input type="hidden" name="provider" value=(provider_value(p.provider));
                        @if p.provider.supports_plan() {
                            label.check {
                                input type="checkbox" name="use_api_key" checked[p.use_api_key];
                                "Use an API key instead of the plan login"
                            }
                        }
                        div.fields {
                            div {
                                label { "API key" }
                                input type="password" name="api_key" autocomplete="off"
                                    placeholder=(if p.api_key.is_empty() { "not set" } else { "saved — leave blank to keep" });
                            }
                            div {
                                label { "Model" }
                                input type="text" name="model" value=(p.model) placeholder="CLI default";
                            }
                            @let levels = p.provider.cli().effort_levels();
                            @if !levels.is_empty() {
                                div {
                                    label { "Effort" }
                                    select name="effort" {
                                        option value="" selected[p.effort.is_empty()] { "CLI default" }
                                        @for level in levels {
                                            option value=(level) selected[p.effort == *level] { (level) }
                                        }
                                    }
                                }
                            }
                            @if matches!(p.provider, Provider::Deepseek | Provider::OpenAiCompatible) {
                                div {
                                    label { "Base URL" }
                                    input type="text" name="base_url" value=(p.base_url)
                                        placeholder=(if p.provider == Provider::Deepseek { "https://api.deepseek.com/anthropic" } else { "http://localhost:8000/v1 (Responses API)" });
                                }
                            }
                        }
                        p.bar {
                            button { "Save" }
                            @if !p.api_key.is_empty() {
                                label.check { input type="checkbox" name="clear_key"; "Remove saved key" }
                            }
                        }
                    }
                }
            }
        }

        h2 { "Projects" }
        div.panel.stack {
            p.muted {
                "Work items for a project go in " code { "work/open/<name>/" } ". "
                "Relative paths are relative to " code { (app.root().display()) } "."
            }
            @if !settings.projects.is_empty() {
                table {
                    tr { th { "Name" } th { "Spec directory" } th { "Repository" } th { "Branch from" } th { "Worktree setup" } th { "Pull requests" } th {} }
                    @for project in &settings.projects {
                        tr {
                            td { (project.name) }
                            td { code { (project.spec_dir.display()) } }
                            td { code { (project.repo.display()) } }
                            td { code { (project.base_ref) } }
                            td {
                                @if let Some(setup) = &project.setup { code { (setup) } }
                                @else { span.muted { "none" } }
                            }
                            td {
                                @if let Some(pr) = &project.pr {
                                    "push to " code { (pr.push_remote) } ", open against "
                                    code { (pr.repo) " " (pr.base) }
                                    @if pr.draft { " (draft)" }
                                } @else { span.muted { "off" } }
                            }
                            td {
                                form.inline method="post" action="/settings/projects/delete" {
                                    input type="hidden" name="name" value=(project.name);
                                    button { "Remove" }
                                }
                            }
                        }
                    }
                }
            }
            form method="post" action="/settings/projects" {
                div.fields {
                    div { label { "Name" } input type="text" name="name" placeholder="phase" required; }
                    div { label { "Spec directory" } input type="text" name="spec_dir" placeholder="spec/phase" required; }
                    div { label { "Repository" } input type="text" name="repo" placeholder="/home/me/dev/phase or ../phase" required; }
                    div { label { "Branch from" } input type="text" name="base_ref" placeholder="main"; }
                }
                div.fields {
                    div { label { "Push to remote (optional)" } input type="text" name="pr_remote" placeholder="fork"; }
                    div { label { "Open pull request against" } input type="text" name="pr_repo" placeholder="phase-rs/phase"; }
                    div { label { "Pull request base branch" } input type="text" name="pr_base" placeholder="main"; }
                    div { label.check style="margin-top:26px" { input type="checkbox" name="pr_draft"; "Open as draft" } }
                }
                label { "Worktree setup command (optional)" }
                input type="text" name="setup" placeholder="e.g. cp \"$AIWC_REPO\"/data/*.json data/";
                p.muted {
                    "Each item gets a fresh git worktree on its own " code { "ai/<item>" } " branch, beside the repo in "
                    code { "<repo>-worktrees/" } ". This command runs there with " code { "sh -c" } " before the agent "
                    "starts, to supply gitignored or generated files the build needs; "
                    code { "$AIWC_REPO" } " is your checkout and " code { "$AIWC_WORKTREE" } " the new worktree. "
                    "Each worker keeps its own " code { "CARGO_TARGET_DIR" } " build cache there too. "
                    "With a push remote set, finished work is pushed there and a pull request opened with "
                    code { "gh" } " (your GitHub login), titled and described by the agent following the repo's PR template. "
                    "To change a project, add it again under the same name."
                }
                p { button { "Add project" } }
            }
        }
    };
    layout("Settings", "settings", false, notice, body)
}

fn login_block(app: &AppState, provider: Provider, status: AuthStatus) -> Markup {
    let cli = provider.cli();
    let session = app.logins().session(cli);
    html! {
        div {
            "Plan login (" code { (cli.program()) } "): "
            @match status {
                AuthStatus::LoggedIn => span.ok { "signed in" },
                AuthStatus::LoggedOut => span.bad { "not signed in" },
                AuthStatus::Unknown => span.muted { "unknown" },
            }
            " "
            @if agent_backend::login_invocation(cli).is_some() {
                form.inline method="post" action="/login" {
                    input type="hidden" name="provider" value=(provider_value(provider));
                    button { "Log in" }
                }
            } @else {
                div.muted { "Sign in by running " code { (cli.program()) } " once in a terminal." }
            }
        }
        @if let Some(session) = session {
            pre { (session.output) }
            @if session.running {
                form.bar method="post" action="/login/input" {
                    input type="hidden" name="provider" value=(provider_value(provider));
                    input type="text" name="line" placeholder="Paste a code if asked" style="width:auto;flex:1";
                    button { "Send" }
                    a href="/settings" { "Refresh" }
                }
            }
        }
    }
}

pub(crate) fn result(item: &WorkItem, body: &str) -> Markup {
    let body = html! {
        h2 { (item) }
        pre { (body) }
        p { a href="/" { "← Dashboard" } }
    };
    layout("Result", "dashboard", false, None, body)
}

fn event_line(event: &AgentEvent) -> Markup {
    html! {
        @match event {
            AgentEvent::Text(text) => (clip(text)),
            AgentEvent::ToolUse { name, detail } => { code { (name) } " " (clip(detail)) },
            AgentEvent::Notice(text) => span.muted { (clip(text)) },
            AgentEvent::Finished { ok: true, .. } => span.ok { "Agent finished" },
            AgentEvent::Finished { ok: false, summary } => {
                span.bad { "Agent failed" @if let Some(s) = summary { ": " (clip(s)) } }
            },
        }
    }
}

fn clip(text: &str) -> String {
    let text = text.trim();
    match text.char_indices().nth(EVENT_CHARS) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_string(),
    }
}

fn elapsed(started: SystemTime) -> String {
    let secs = started.elapsed().map(|d| d.as_secs()).unwrap_or(0);
    format!("{}:{:02}", secs / 60, secs % 60)
}

/// The form value of a provider, matching its serde name.
fn provider_value(provider: Provider) -> &'static str {
    match provider {
        Provider::Anthropic => "anthropic",
        Provider::Gemini => "gemini",
        Provider::OpenAi => "open_ai",
        Provider::Deepseek => "deepseek",
        Provider::OpenAiCompatible => "open_ai_compatible",
    }
}

fn url_encode(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    // Test code asserts rather than propagating; see Rust code review.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]

    use super::*;

    #[test]
    fn clip_cuts_long_text_on_a_char_boundary() {
        let long = "é".repeat(EVENT_CHARS + 5);
        let clipped = clip(&long);
        assert_eq!(clipped.chars().count(), EVENT_CHARS + 1);
        assert!(clipped.ends_with('…'));
        assert_eq!(clip(" short "), "short");
    }

    #[test]
    fn provider_values_match_serde_names() {
        for p in Provider::ALL {
            let json = format!("\"{}\"", provider_value(p));
            assert_eq!(serde_json_like(&json), p);
        }
    }

    fn serde_json_like(json: &str) -> Provider {
        #[derive(serde::Deserialize)]
        struct W {
            p: Provider,
        }
        let w: W = toml::from_str(&format!("p = {json}")).unwrap();
        w.p
    }

    #[test]
    fn url_encode_escapes_separators() {
        assert_eq!(url_encode("phase/a b.txt"), "phase%2Fa%20b.txt");
    }
}

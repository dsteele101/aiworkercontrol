//! HTML pages.

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use agent_backend::{AgentEvent, AuthStatus, Cli, Provider, UsageWindow};
use maud::{DOCTYPE, Markup, html};
use work_queue::{State as ItemState, WorkItem};
use worker_pool::{ActiveJob, Activity, DEFAULT_BUILD_JOBS, PoolSnapshot, Scheme, UsageLimits};

use crate::routes::{MAX_BUILD_JOBS, MAX_TURNS, MAX_WEIGHT, MAX_WORKERS};
use crate::{AppState, Settings};

/// Agent events shown per worker card.
const CARD_EVENTS: usize = 8;
/// Longest event text shown before it is cut.
const EVENT_CHARS: usize = 240;

const STYLE: &str = r#"
:root {
  color-scheme: light;
  --bg: #f6f7f9; --panel: #ffffff; --text: #1d2330; --muted: #5f6878;
  --border: #dde1e8; --accent: #2f5bd3; --on-accent: #ffffff; --link: #2448b8;
  --ok: #1f7a4d; --bad: #b3261e; --warn: #8a5a00; --code: #eef1f5;
}
@media (prefers-color-scheme: dark) {
  :root {
    color-scheme: dark;
    --bg: #12151b; --panel: #1b1f27; --text: #e4e7ee; --muted: #a3acbd;
    --border: #2c323e; --accent: #8fb0ff; --on-accent: #0d1220; --link: #9dbaff;
    --ok: #5cc48f; --bad: #f2867d; --warn: #e8bd68; --code: #262b36;
  }
}
* { box-sizing: border-box; }
body { margin: 0; background: var(--bg); color: var(--text);
  font: 15px/1.45 system-ui, -apple-system, "Segoe UI", sans-serif; }
header { display: flex; gap: 20px; align-items: baseline; padding: 14px 16px;
  border-bottom: 1px solid var(--border); background: var(--panel); }
header h1 { font-size: 17px; margin: 0; }
a, a:visited { color: var(--link); text-decoration: none; }
a:hover { text-decoration: underline; }
header a, header a:visited { color: var(--muted); }
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
.now { background: var(--code); border-radius: 6px; padding: 8px 10px; margin-top: 8px;
  overflow-wrap: anywhere; }
.now code { background: none; padding: 0; }
.timer { font-variant-numeric: tabular-nums; color: var(--muted); }
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
details.menu { display: inline-block; position: relative; }
details.menu summary { cursor: pointer; color: var(--link); list-style: none; }
details.menu[open] > div { position: absolute; right: 0; z-index: 1; display: flex;
  flex-direction: column; gap: 6px; margin-top: 4px; padding: 8px; white-space: nowrap;
  background: var(--panel); border: 1px solid var(--border); border-radius: 6px; }
button, input, select { font: inherit; color: inherit; }
button { background: var(--panel); border: 1px solid var(--border);
  border-radius: 6px; padding: 4px 12px; cursor: pointer; }
button.primary { background: var(--accent); border-color: var(--accent); color: var(--on-accent);
  font-weight: 600; }
button:hover { border-color: var(--accent); }
button:disabled { opacity: .5; cursor: default; }
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

/// Keeps the page's `data-live` sections current: every two seconds while
/// the tab is visible, fetches the page again and swaps in each section
/// whose content changed. A section is left alone while a field in it has
/// focus or an open menu, so nothing is replaced under the user's hands.
const LIVE_SCRIPT: &str = r#"
(() => {
  const url = location.pathname + "?live=1";
  async function refresh() {
    if (document.hidden) return;
    let doc;
    try {
      const res = await fetch(url, { cache: "no-store" });
      if (!res.ok) return;
      doc = new DOMParser().parseFromString(await res.text(), "text/html");
    } catch (_) {
      return;
    }
    for (const section of document.querySelectorAll("[data-live]")) {
      const next = doc.getElementById(section.id);
      const busy = section.querySelector("details[open]") ||
        (section.contains(document.activeElement) &&
          document.activeElement.matches("input, select, textarea"));
      if (next && !busy && next.innerHTML !== section.innerHTML) {
        section.innerHTML = next.innerHTML;
      }
    }
  }
  setInterval(refresh, 2000);
  document.addEventListener("visibilitychange", refresh);
})();
"#;

fn layout(title: &str, here: &str, refresh: bool, notice: Option<String>, body: Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                @if refresh {
                    // Without scripts, the whole page reloads instead.
                    noscript { meta http-equiv="refresh" content="2"; }
                }
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
                @if refresh { script { (maud::PreEscaped(LIVE_SCRIPT)) } }
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
        div #status data-live {
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
        }

        div #usage data-live {
            h2 { "Plan usage" }
            (usage_panel(&snapshot, &settings.limits))
            p.muted { "Balancing: " (settings.balancing.scheme.label()) }
        }

        div #workers data-live {
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
                                } @else if snapshot.limited.is_some() {
                                    p.bad { "Waiting: plan usage reached the stop level." }
                                } @else {
                                    p.muted { "Waiting for open work." }
                                }
                            }
                            Activity::Working(job) => {
                                h3 { span { "Worker " (worker.id + 1) } span.muted { (elapsed(job.started)) } }
                                div { strong { (job.item) } " " span.muted { "· " (job.provider.label()) } }
                                (now_line(job))
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

        }

        div #queue data-live {
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
                                @if m.pr_url.is_some() {
                                    " "
                                    details.menu {
                                        summary { "Next action" }
                                        div {
                                            form.inline method="post" action="/review/check" {
                                                input type="hidden" name="item" value=(item);
                                                button title="Read the latest reviews, comments and CI results now, and queue a rework if they ask for changes" { "Check latest" }
                                            }
                                            form.inline method="post" action="/review/bump" {
                                                input type="hidden" name="item" value=(item);
                                                button title="Push an empty commit to the pull request so the remote checks run again" { "Bump PR" }
                                            }
                                        }
                                    }
                                }
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

        h2 { "Plan usage limits" }
        form.panel method="post" action="/settings/limits" {
            div.fields {
                div {
                    label for="warn" { "Warn at (%, comma-separated)" }
                    input #warn type="text" name="warn" value=(settings.limits.warn.iter().map(u8::to_string).collect::<Vec<_>>().join(", "));
                }
                div {
                    label for="stop" { "Stop at (%)" }
                    input #stop type="number" name="stop" min="1" max="100" value=(settings.limits.stop);
                }
            }
            label.check {
                input type="checkbox" name="failover" checked[settings.limits.failover];
                "At the stop level, fail over to the next provider below (priority failover; the other balancing schemes always share work across all of them)"
            }
            div.fields {
                @for (i, name) in ["fallback1", "fallback2", "fallback3", "fallback4"].iter().enumerate() {
                    @let chosen = settings.fallbacks.get(i).copied();
                    div {
                        label for=(name) { "Fallback " (i + 1) }
                        select id=(name) name=(name) {
                            option value="" selected[chosen.is_none()] { "none" }
                            @for p in Provider::ALL {
                                option value=(provider_value(p)) selected[chosen == Some(p)] { (p.label()) }
                            }
                        }
                    }
                }
            }
            p.bar {
                button { "Save" }
                span.muted {
                    "Levels apply to each window a provider reports, the current one and the week. "
                    "At the stop level no new run starts on that provider; running agents finish their item. "
                    "Each fallback uses the model and effort on its card above, and is skipped unless it is set up. "
                    "Claude and Antigravity report their usage; Codex doesn't, so it is never stopped."
                }
            }
        }

        h2 { "Load balancing" }
        form.panel method="post" action="/settings/balancing" {
            p.muted style="margin-top:0" {
                "Spreads items across " (settings.active.label()) " and the fallbacks above, among those under their stop level."
            }
            div.fields {
                div {
                    label for="scheme" { "Scheme" }
                    select #scheme name="scheme" {
                        @for scheme in Scheme::ALL {
                            option value=(scheme_value(scheme)) selected[scheme == settings.balancing.scheme] { (scheme.label()) }
                        }
                    }
                }
                @for p in std::iter::once(settings.active).chain(settings.fallbacks.iter().copied().filter(|p| *p != settings.active)) {
                    div {
                        @let name = format!("weight_{}", provider_value(p));
                        label for=(name) { (p.label()) " weight" }
                        input id=(name) type="number" name=(name) min="1" max=(MAX_WEIGHT) value=(settings.balancing.weight(p));
                    }
                }
            }
            label.check {
                input type="checkbox" name="rework_any_provider" checked[settings.balancing.rework_any_provider];
                "Reworks may run on any provider (off: a rework waits for the provider that opened its pull request)"
            }
            p.bar {
                button { "Save" }
            }
            ul.muted {
                li { strong { "Priority failover" } ": the active provider, then each fallback in order once the ones before it reach the stop level." }
                li { strong { "Round robin" } ": each provider in turn." }
                li { strong { "Weighted" } ": in proportion to the weights, spread evenly; weights only matter here." }
                li { strong { "Pace-based" } ": the provider furthest below its pace, i.e. least used for how much of its plan window has passed. Codex reports no usage, so it counts as exactly on pace." }
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
                    tr { th { "Name" } th { "Spec directory" } th { "Repository" } th { "Branch from" } th { "Worktree setup" } th { "Build jobs" } th { "Pull requests" } th {} }
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
                            td { (project.build_jobs) }
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
                    div { label { "Build jobs" } input type="number" name="build_jobs" min="1" max=(MAX_BUILD_JOBS) placeholder=(DEFAULT_BUILD_JOBS); }
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
                    "Each worker keeps its own " code { "CARGO_TARGET_DIR" } " build cache there too, and builds with "
                    code { "CARGO_BUILD_JOBS" } " set to the build jobs. "
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
            AgentEvent::Usage(_) => span.muted { "Plan usage updated" },
            // Claude ends a turn while its background commands run, then
            // carries on, so this is not necessarily the end of the run.
            AgentEvent::Finished { ok: true, summary } => {
                span.ok { "Turn ended" } @if let Some(s) = summary { ": " (clip(s)) }
            },
            AgentEvent::Status(text) => span.muted { (clip(text)) },
            AgentEvent::Background(tasks) => span.muted { "Background: " (tasks.join("; ")) },
            AgentEvent::Finished { ok: false, summary } => {
                span.bad { "Agent failed" @if let Some(s) = summary { ": " (clip(s)) } }
            },
        }
    }
}

fn clip(text: &str) -> String {
    clip_to(text, EVENT_CHARS)
}

/// `text`, trimmed and cut to `chars` characters.
fn clip_to(text: &str, chars: usize) -> String {
    let text = text.trim();
    match text.char_indices().nth(chars) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_string(),
    }
}

fn usage_panel(snapshot: &PoolSnapshot, limits: &UsageLimits) -> Markup {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    html! {
        div.panel {
            @if let Some(limited) = &snapshot.limited {
                p.bad { "Workers are waiting: " (limited) ". Override a provider below to let runs start on it again." }
            }
            @for (provider, error) in &snapshot.suspended {
                p.warn {
                    (provider.label()) " is set aside as a fallback until the active provider can run again. "
                    "Its last run failed: " (clip(error))
                }
            }
            @if snapshot.usage.is_empty() {
                p.muted { "No usage reported yet. Claude reports its plan usage during each run, and Antigravity is asked for its usage every five minutes; Codex doesn't report its." }
            } @else {
                table {
                    tr { th { "Provider" } th { "Current window" } th { "Week" } th { "Status" } }
                    @for u in &snapshot.usage {
                        @let used = u.usage.highest_at(now);
                        tr {
                            td { (u.provider.label()) }
                            td { (usage_window(u.usage.session, now, limits)) }
                            td { (usage_window(u.usage.weekly, now, limits)) }
                            td {
                                @if limits.stopped(used) {
                                    @if snapshot.overridden.contains(&u.provider) {
                                        span.warn { "Past the " (limits.stop) "% stop level; overridden until the window resets" }
                                    } @else {
                                        span.bad { "Stopped at the " (limits.stop) "% level " }
                                        form.inline method="post" action="/usage/override" {
                                            input type="hidden" name="provider" value=(provider_value(u.provider));
                                            button { "Override" }
                                        }
                                    }
                                } @else if let Some(level) = limits.warning(used) {
                                    span.warn { "Past the " (level) "% warning" }
                                } @else {
                                    span.muted { "OK" }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn usage_window(window: Option<UsageWindow>, now: u64, limits: &UsageLimits) -> Markup {
    html! {
        @match window {
            None => span.muted { "not reported" },
            Some(w) => {
                @let percent = w.percent_at(now);
                @let class = if limits.stopped(percent) {
                    Some("bad")
                } else {
                    limits.warning(percent).map(|_| "warn")
                };
                strong class=[class] { (percent) "%" }
                @if let Some(reset) = w.resets_at.filter(|r| *r > now) {
                    " " span.muted { "· resets in " (duration(reset - now)) }
                } @else if w.resets_at.is_some() {
                    " " span.muted { "· reset since last report" }
                }
            }
        }
    }
}

/// A rough length of time: `3d 4h`, `2h 14m` or `9m`.
fn duration(secs: u64) -> String {
    let (days, hours, mins) = (secs / 86_400, secs / 3_600 % 24, secs / 60 % 60);
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {mins}m")
    } else {
        format!("{mins}m")
    }
}

/// What the agent is doing right now, and for how long: waiting on
/// background commands, running a step, or thinking; then its own latest
/// account of where it stands.
fn now_line(job: &ActiveJob) -> Markup {
    html! {
        div.now {
            @if !job.background.is_empty() {
                span.warn { "Waiting on " (job.background.join("; ")) }
                " " span.timer { (elapsed(job.background_since.unwrap_or(job.last_event))) }
            } @else {
                @match job.events.back() {
                    Some(AgentEvent::ToolUse { name, detail }) => {
                        strong { (activity(name, detail)) }
                        " " span.timer { (elapsed(job.last_event)) }
                        @if !detail.is_empty() { div { code { (clip_to(detail, 120)) } } }
                    }
                    Some(AgentEvent::Finished { .. }) => {
                        strong { "Wrapping up" } " " span.timer { (elapsed(job.last_event)) }
                    }
                    None => { strong { "Starting" } " " span.timer { (elapsed(job.started)) } }
                    Some(_) => { strong { "Thinking" } " " span.timer { (elapsed(job.last_event)) } }
                }
            }
            @if let Some(status) = &job.status {
                div.muted { "Agent's status: " (status) }
            }
        }
    }
}

/// A plain description of a tool call: for shell commands, what the
/// command is for when it is a common one.
fn activity(tool: &str, detail: &str) -> String {
    let lower = tool.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "bash" | "shell" | "run_command" | "run_shell_command"
    ) {
        let command = detail.trim_start();
        let label = if command.contains("clippy") {
            "Linting (clippy)"
        } else if command.contains("nextest") || command.contains("cargo test") {
            "Running tests"
        } else if command.contains("cargo build") || command.contains("cargo check") {
            "Building"
        } else if command.contains("cargo fmt") || command.contains("rustfmt") {
            "Formatting"
        } else if command.contains("check-parser-combinators") {
            "Running Gate A"
        } else if command.starts_with("git ") {
            "Running git"
        } else if command.starts_with("gh ") {
            "Querying GitHub"
        } else {
            "Running a command"
        };
        return label.to_string();
    }
    let label = match lower.as_str() {
        "read" | "view_file" | "read_file" => "Reading",
        "edit" | "write" | "multiedit" | "write_file" | "replace" => "Editing",
        "grep" | "glob" | "search" | "grep_search" | "find_by_name" => "Searching",
        "webfetch" | "websearch" | "web_search" | "read_url_content" => "Looking it up",
        "todowrite" => "Planning",
        "task" | "agent" => "Delegating to a subagent",
        _ => return format!("Using {tool}"),
    };
    label.to_string()
}

fn elapsed(started: SystemTime) -> String {
    clock(started.elapsed().map(|d| d.as_secs()).unwrap_or(0))
}

/// `m:ss`, or `h:mm:ss` from an hour on.
fn clock(secs: u64) -> String {
    let (h, m, s) = (secs / 3_600, secs / 60 % 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// The form value of a scheme, matching its serde name.
fn scheme_value(scheme: Scheme) -> &'static str {
    match scheme {
        Scheme::Priority => "priority",
        Scheme::RoundRobin => "round_robin",
        Scheme::Weighted => "weighted",
        Scheme::Pace => "pace",
    }
}

/// The form value of a provider, matching its serde name.
pub(crate) fn provider_value(provider: Provider) -> &'static str {
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
    fn clock_shows_hours_once_past_one() {
        assert_eq!(clock(5), "0:05");
        assert_eq!(clock(59 * 60 + 9), "59:09");
        assert_eq!(clock(3_600 + 62), "1:01:02");
    }

    #[test]
    fn common_commands_get_plain_names() {
        assert_eq!(
            activity(
                "Bash",
                "cargo clippy --workspace --all-targets -- -D warnings"
            ),
            "Linting (clippy)"
        );
        assert_eq!(
            activity("Bash", "cargo nextest run -p engine"),
            "Running tests"
        );
        assert_eq!(activity("shell", "git status"), "Running git");
        assert_eq!(activity("Bash", "ls -la"), "Running a command");
        assert_eq!(activity("view_file", "/a.rs"), "Reading");
        assert_eq!(activity("Monitor", ""), "Using Monitor");
    }

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

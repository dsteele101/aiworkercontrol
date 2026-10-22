//! Request handlers. Every form posts, applies its change, sets a notice
//! when there is something to say, and redirects back to a page.

use std::collections::HashMap;

use agent_backend::{Cli, Provider};
use axum::Router;
use axum::extract::{Form, Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use work_queue::{RESULT_SUFFIX, State as ItemState, WorkItem};
use worker_pool::{
    Balancing, BranchOutcome, PoolError, Project, PullRequests, ReviewChange, Scheme,
};

use crate::{AppState, login, pages};

/// The most workers the UI allows.
pub(crate) const MAX_WORKERS: usize = 32;
/// The highest turn limit the UI allows.
pub(crate) const MAX_TURNS: u32 = 50;
/// The highest provider weight the UI allows.
pub(crate) const MAX_WEIGHT: u32 = 100;
/// The most build jobs a project's agents may run at once.
pub(crate) const MAX_BUILD_JOBS: u32 = 64;

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(dashboard))
        .route("/history", get(history_page))
        .route("/history.json", get(history_json))
        .route("/static/{name}", get(static_file))
        .route("/settings", get(settings_page))
        .route("/result", get(result))
        .route("/start", post(start))
        .route("/stop", post(stop))
        .route("/workers", post(set_workers))
        .route("/resume", post(resume))
        .route("/import", post(import_pull_request))
        .route("/review/check", post(check_review))
        .route("/review/rework", post(rework_review))
        .route("/review/bump", post(bump_review))
        .route("/settings/max-turns", post(set_max_turns))
        .route("/settings/limits", post(set_limits))
        .route("/settings/balancing", post(set_balancing))
        .route("/usage/override", post(override_stop))
        .route("/cleanup", post(clean_up))
        .route("/settings/active", post(set_active))
        .route("/settings/provider", post(save_provider))
        .route("/settings/provider/enabled", post(set_provider_enabled))
        .route("/settings/projects", post(add_project))
        .route("/settings/projects/delete", post(delete_project))
        .route("/login", post(start_login))
        .route("/login/input", post(login_input))
}

#[derive(Deserialize)]
struct DashboardQuery {
    /// Set by the page's own refresh, which must not use up a notice meant
    /// for the next page the user opens.
    #[serde(default)]
    live: Option<String>,
}

async fn dashboard(State(app): State<AppState>, Query(q): Query<DashboardQuery>) -> Html<String> {
    let notice = if q.live.is_some() {
        None
    } else {
        app.take_notice()
    };
    Html(pages::dashboard(&app, notice).into_string())
}

async fn settings_page(State(app): State<AppState>) -> Html<String> {
    let notice = app.take_notice();
    let settings = app.settings();
    let cli_dir = settings.cli_dir.as_deref();
    let (claude, codex, antigravity, vibe) = tokio::join!(
        login::status(Cli::Claude, cli_dir),
        login::status(Cli::Codex, cli_dir),
        login::status(Cli::Antigravity, cli_dir),
        login::status(Cli::Vibe, cli_dir),
    );
    let status = |cli| match cli {
        Cli::Claude => claude,
        Cli::Codex => codex,
        Cli::Antigravity => antigravity,
        Cli::Vibe => vibe,
    };
    Html(pages::settings(&app, &settings, status, notice).into_string())
}

async fn history_page(State(app): State<AppState>) -> Html<String> {
    Html(pages::history(app.take_notice()).into_string())
}

/// Every item's history summary, for the history page to draw.
async fn history_json(State(app): State<AppState>) -> Response {
    #[derive(serde::Serialize)]
    struct History {
        generated_at: u64,
        items: Vec<worker_pool::ItemSummary>,
    }
    match app.pool().history() {
        Ok(items) => {
            let generated_at = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            axum::Json(History {
                generated_at,
                items,
            })
            .into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// The scripts of the history page, compiled in so it works offline.
async fn static_file(Path(name): Path<String>) -> Response {
    let body = match name.as_str() {
        "chart.umd.js" => include_str!("../assets/chart.umd.js"),
        "history.js" => include_str!("../assets/history.js"),
        _ => return (StatusCode::NOT_FOUND, "no such file").into_response(),
    };
    (
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            // Revalidated each time, so an upgrade shows at once.
            (header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
        .into_response()
}

#[derive(Deserialize)]
struct ItemQuery {
    item: String,
}

async fn result(State(app): State<AppState>, Query(q): Query<ItemQuery>) -> Response {
    let queue = app.pool().queue();
    // Only items actually listed in some state are served, so the query
    // cannot name an arbitrary path.
    let found = ItemState::ALL.into_iter().find_map(|state| {
        let items = queue.list(state).unwrap_or_default();
        items
            .into_iter()
            .find(|i| i.to_string() == q.item)
            .map(|item| (state, item))
    });
    let Some((state, item)) = found else {
        return (StatusCode::NOT_FOUND, "no such item").into_response();
    };
    let path = queue.sidecar_path(state, &item, RESULT_SUFFIX);
    let body = std::fs::read_to_string(&path).unwrap_or_else(|e| format!("(no result file: {e})"));
    Html(pages::result(&item, &body).into_string()).into_response()
}

async fn start(State(app): State<AppState>) -> Redirect {
    let settings = app.settings();
    if let Err(e) = settings.active_config() {
        app.notify(format!("Not started: {e}."));
    } else if settings.projects.is_empty() {
        app.notify("Not started: add a project in Settings first.");
    } else {
        app.pool().start();
    }
    Redirect::to("/")
}

async fn stop(State(app): State<AppState>) -> Redirect {
    app.pool().stop();
    app.notify("Stopping: workers finish their current item, then stop.");
    Redirect::to("/")
}

#[derive(Deserialize)]
struct WorkersForm {
    count: usize,
    #[serde(default)]
    back: Option<String>,
}

async fn set_workers(State(app): State<AppState>, Form(f): Form<WorkersForm>) -> Redirect {
    let count = f.count.min(MAX_WORKERS);
    report(
        &app,
        app.update_settings(|s| {
            s.workers = count;
            Ok(())
        }),
    );
    back(f.back.as_deref())
}

#[derive(Deserialize)]
struct ItemForm {
    item: String,
}

async fn resume(State(app): State<AppState>, Form(f): Form<ItemForm>) -> Redirect {
    if let Some((project, file)) = f.item.split_once('/') {
        let item = WorkItem::new(project, file);
        let message = match app.pool().resume(&item).await {
            Ok(()) => format!("Returned {item} to the agents with a fresh turn count."),
            Err(e) => format!("Could not return {item}: {e}"),
        };
        app.notify(message);
    }
    Redirect::to("/")
}

#[derive(Deserialize)]
struct ImportForm {
    url: String,
}

async fn import_pull_request(State(app): State<AppState>, Form(f): Form<ImportForm>) -> Redirect {
    let message = match app.pool().import_pull_request(&f.url).await {
        Ok(item) => format!("Imported the pull request as {item}; it is in review."),
        Err(e) => format!("Could not import the pull request: {e}"),
    };
    app.notify(message);
    Redirect::to("/")
}

async fn check_review(State(app): State<AppState>, Form(f): Form<ItemForm>) -> Redirect {
    if let Some((project, file)) = f.item.split_once('/') {
        let item = WorkItem::new(project, file);
        let result = app.pool().check_review(&item).await;
        app.notify(review_message(&item, result));
    }
    Redirect::to("/")
}

async fn rework_review(State(app): State<AppState>, Form(f): Form<ItemForm>) -> Redirect {
    if let Some((project, file)) = f.item.split_once('/') {
        let item = WorkItem::new(project, file);
        let result = app.pool().rework_again(&item).await;
        app.notify(review_message(&item, result));
    }
    Redirect::to("/")
}

fn review_message(item: &WorkItem, result: Result<Option<ReviewChange>, PoolError>) -> String {
    match result {
        Ok(Some(ReviewChange::Merged)) => format!("The pull request of {item} was merged."),
        Ok(Some(ReviewChange::ClosedUnmerged)) => {
            format!("The pull request of {item} was closed without merging; it is on hold.")
        }
        Ok(Some(ReviewChange::ChangesRequested)) => {
            format!("Feedback found on {item}; it waits for a worker to rework it.")
        }
        Ok(None) => format!(
            "Nothing new to act on for {item}: no changes requested, failing checks or \
             inline comments on the latest commit."
        ),
        Err(e) => format!("Could not check {item}: {e}"),
    }
}

async fn bump_review(State(app): State<AppState>, Form(f): Form<ItemForm>) -> Redirect {
    if let Some((project, file)) = f.item.split_once('/') {
        let item = WorkItem::new(project, file);
        let message = match app.pool().bump(&item).await {
            Ok(()) => format!("Pushed an empty commit to the pull request of {item}."),
            Err(e) => format!("Could not bump {item}: {e}"),
        };
        app.notify(message);
    }
    Redirect::to("/")
}

#[derive(Deserialize)]
struct MaxTurnsForm {
    max_turns: u32,
}

async fn set_max_turns(State(app): State<AppState>, Form(f): Form<MaxTurnsForm>) -> Redirect {
    let max_turns = f.max_turns.clamp(1, MAX_TURNS);
    report(
        &app,
        app.update_settings(|s| {
            s.max_turns = max_turns;
            Ok(())
        }),
    );
    Redirect::to("/settings")
}

#[derive(Deserialize)]
struct LimitsForm {
    warn: String,
    stop: u8,
    #[serde(default)]
    failover: Option<String>,
    // Blank when the select is left on "none".
    #[serde(default)]
    fallback1: String,
    #[serde(default)]
    fallback2: String,
    #[serde(default)]
    fallback3: String,
    #[serde(default)]
    fallback4: String,
}

async fn set_limits(State(app): State<AppState>, Form(f): Form<LimitsForm>) -> Redirect {
    let saved = app.update_settings(|s| {
        if !(1..=100).contains(&f.stop) {
            return Err("the stop level must be between 1 and 100%".into());
        }
        let mut warn = Vec::new();
        for level in f.warn.split(',').map(str::trim).filter(|l| !l.is_empty()) {
            let level: u8 = level
                .trim_end_matches('%')
                .parse()
                .map_err(|_| format!("{level:?} is not a warning level"))?;
            if level == 0 || level >= f.stop {
                return Err(format!(
                    "warning levels must be above 0 and below the {}% stop level",
                    f.stop
                ));
            }
            warn.push(level);
        }
        warn.sort_unstable();
        warn.dedup();
        s.limits.warn = warn;
        s.limits.stop = f.stop;
        s.limits.failover = f.failover.is_some();
        s.fallbacks = [&f.fallback1, &f.fallback2, &f.fallback3, &f.fallback4]
            .into_iter()
            .filter_map(|v| {
                Provider::ALL
                    .into_iter()
                    .find(|p| pages::provider_value(*p) == v)
            })
            .collect();
        Ok(())
    });
    if saved.is_ok() {
        app.notify("Saved the plan usage limits.");
    }
    report(&app, saved);
    Redirect::to("/settings")
}

/// Every field is text, so the per-provider `weight_<provider>` fields can
/// be gathered by `flatten`.
#[derive(Deserialize)]
struct BalancingForm {
    scheme: Scheme,
    #[serde(default)]
    rework_any_provider: Option<String>,
    #[serde(flatten)]
    weights: HashMap<String, String>,
}

async fn set_balancing(State(app): State<AppState>, Form(f): Form<BalancingForm>) -> Redirect {
    let saved = app.update_settings(|s| {
        // Only the listed providers have a field; others keep their weight.
        let mut weights = s.balancing.weights.clone();
        for p in Provider::ALL {
            let Some(value) = f
                .weights
                .get(&format!("weight_{}", pages::provider_value(p)))
            else {
                continue;
            };
            let weight: u32 = value
                .trim()
                .parse()
                .ok()
                .filter(|w| (1..=MAX_WEIGHT).contains(w))
                .ok_or_else(|| {
                    format!(
                        "{}'s weight must be a whole number from 1 to {MAX_WEIGHT}",
                        p.label()
                    )
                })?;
            if weight == 1 {
                weights.remove(&p);
            } else {
                weights.insert(p, weight);
            }
        }
        s.balancing = Balancing {
            scheme: f.scheme,
            weights,
            rework_any_provider: f.rework_any_provider.is_some(),
        };
        Ok(())
    });
    if saved.is_ok() {
        app.notify(format!("Workers now use {} balancing.", f.scheme.label()));
    }
    report(&app, saved);
    Redirect::to("/settings")
}

#[derive(Deserialize)]
struct OverrideForm {
    provider: Provider,
}

async fn override_stop(State(app): State<AppState>, Form(f): Form<OverrideForm>) -> Redirect {
    app.pool().override_stop(f.provider);
    app.notify(format!(
        "Runs may start on {} again until its usage window resets.",
        f.provider.label()
    ));
    Redirect::to("/")
}

async fn clean_up(State(app): State<AppState>, Form(f): Form<ItemForm>) -> Redirect {
    let Some((project, file)) = f.item.split_once('/') else {
        return Redirect::to("/");
    };
    let item = WorkItem::new(project, file);
    let branch = Project::branch(&item);
    let message = match app.pool().clean_up(&item).await {
        Ok(done) => {
            let worktree = if done.worktree_removed {
                "Removed the worktree"
            } else {
                "The worktree was already gone"
            };
            let branch = match done.branch {
                BranchOutcome::Deleted => format!("deleted merged branch {branch}"),
                BranchOutcome::KeptUnmerged => format!(
                    "kept {branch} because its changes are not all in the base branch \
                     (a squash merge looks like this); delete it yourself once merged"
                ),
                BranchOutcome::Missing => format!("{branch} was already deleted"),
            };
            format!("{worktree} of {item}; {branch}.")
        }
        Err(e) => format!("Could not clean up {item}: {e}"),
    };
    app.notify(message);
    Redirect::to("/")
}

#[derive(Deserialize)]
struct ActiveForm {
    provider: Provider,
}

async fn set_active(State(app): State<AppState>, Form(f): Form<ActiveForm>) -> Redirect {
    let saved = app.update_settings(|s| {
        if !s.is_enabled(f.provider) {
            return Err(format!("{} is disabled", f.provider.label()));
        }
        s.active = f.provider;
        Ok(())
    });
    if saved.is_ok() {
        app.notify(format!(
            "Workers now use {} for the next item they pick up.",
            f.provider.label()
        ));
    }
    report(&app, saved);
    Redirect::to("/settings")
}

#[derive(Deserialize)]
struct EnabledForm {
    provider: Provider,
    enabled: bool,
}

async fn set_provider_enabled(State(app): State<AppState>, Form(f): Form<EnabledForm>) -> Redirect {
    let saved = app.update_settings(|s| {
        s.provider_mut(f.provider).enabled = f.enabled;
        Ok(())
    });
    if saved.is_ok() {
        let state = if f.enabled { "enabled" } else { "disabled" };
        app.notify(format!("{} is {state}.", f.provider.label()));
    }
    report(&app, saved);
    Redirect::to("/settings")
}

#[derive(Deserialize)]
struct ProviderForm {
    provider: Provider,
    #[serde(default)]
    use_api_key: Option<String>,
    #[serde(default)]
    api_key: String,
    #[serde(default)]
    clear_key: Option<String>,
    #[serde(default)]
    model: String,
    #[serde(default)]
    effort: String,
    #[serde(default)]
    base_url: String,
}

async fn save_provider(State(app): State<AppState>, Form(f): Form<ProviderForm>) -> Redirect {
    let saved = app.update_settings(|s| {
        let p = s.provider_mut(f.provider);
        p.use_api_key = f.use_api_key.is_some() || !f.provider.supports_plan();
        // A blank key field keeps the saved key; it is never sent back to
        // the browser, so blank is what an untouched field submits.
        if f.clear_key.is_some() {
            p.api_key.clear();
        } else if !f.api_key.trim().is_empty() {
            p.api_key = f.api_key.trim().to_string();
        }
        p.model = f.model.trim().to_string();
        p.effort = f.effort.trim().to_string();
        p.base_url = f.base_url.trim().to_string();
        Ok(())
    });
    if saved.is_ok() {
        app.notify(format!("Saved {} settings.", f.provider.label()));
    }
    report(&app, saved);
    Redirect::to("/settings")
}

#[derive(Deserialize)]
struct ProjectForm {
    name: String,
    spec_dir: String,
    repo: String,
    #[serde(default)]
    base_ref: String,
    #[serde(default)]
    setup: String,
    #[serde(default)]
    pr_remote: String,
    #[serde(default)]
    pr_repo: String,
    #[serde(default)]
    pr_base: String,
    #[serde(default)]
    pr_draft: Option<String>,
    #[serde(default)]
    build_jobs: String,
}

async fn add_project(State(app): State<AppState>, Form(f): Form<ProjectForm>) -> Redirect {
    let root = app.root().to_path_buf();
    report(
        &app,
        app.update_settings(|s| {
            let name = f.name.trim();
            let valid_name = !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
                && !name.starts_with('.');
            if !valid_name {
                return Err("Project name must be a plain directory name.".into());
            }
            let mut project = Project::new(name, f.spec_dir.trim(), f.repo.trim());
            if !f.base_ref.trim().is_empty() {
                project.base_ref = f.base_ref.trim().to_string();
            }
            project.setup = Some(f.setup.trim().to_string()).filter(|s| !s.is_empty());
            if !f.build_jobs.trim().is_empty() {
                project.build_jobs = f
                    .build_jobs
                    .trim()
                    .parse()
                    .ok()
                    .filter(|j| (1..=MAX_BUILD_JOBS).contains(j))
                    .ok_or(format!(
                        "Build jobs must be a number from 1 to {MAX_BUILD_JOBS}."
                    ))?;
            }
            let (remote, repo) = (f.pr_remote.trim(), f.pr_repo.trim());
            project.pr = match (remote.is_empty(), repo.is_empty()) {
                (true, true) => None,
                (false, false) if repo.split('/').filter(|p| !p.is_empty()).count() == 2 => {
                    Some(PullRequests {
                        push_remote: remote.to_string(),
                        repo: repo.to_string(),
                        base: Some(f.pr_base.trim())
                            .filter(|b| !b.is_empty())
                            .unwrap_or("main")
                            .to_string(),
                        draft: f.pr_draft.is_some(),
                    })
                }
                _ => {
                    return Err(
                        "Pull requests need both a push remote and a repository (owner/name)."
                            .into(),
                    );
                }
            };
            if !root.join(&project.spec_dir).is_dir() {
                return Err(format!(
                    "Spec directory {} not found.",
                    project.spec_dir.display()
                ));
            }
            if !root.join(&project.repo).join(".git").exists() {
                return Err(format!(
                    "{} is not a git repository.",
                    project.repo.display()
                ));
            }
            s.projects.retain(|p| p.name != project.name);
            s.projects.push(project);
            s.projects.sort_by(|a, b| a.name.cmp(&b.name));
            Ok(())
        }),
    );
    Redirect::to("/settings")
}

#[derive(Deserialize)]
struct NameForm {
    name: String,
}

async fn delete_project(State(app): State<AppState>, Form(f): Form<NameForm>) -> Redirect {
    report(
        &app,
        app.update_settings(|s| {
            s.projects.retain(|p| p.name != f.name);
            Ok(())
        }),
    );
    Redirect::to("/settings")
}

#[derive(Deserialize)]
struct LoginForm {
    provider: Provider,
}

async fn start_login(State(app): State<AppState>, Form(f): Form<LoginForm>) -> Redirect {
    let cli = f.provider.cli();
    let settings = app.settings();
    if !app.logins().start(cli, settings.cli_dir.as_deref()) {
        app.notify(format!(
            "{} has no login command; run `{}` in a terminal to sign in.",
            cli.program(),
            cli.program()
        ));
    }
    Redirect::to("/settings")
}

#[derive(Deserialize)]
struct LoginInputForm {
    provider: Provider,
    line: String,
}

async fn login_input(State(app): State<AppState>, Form(f): Form<LoginInputForm>) -> Redirect {
    if !app.logins().send(f.provider.cli(), &f.line).await {
        app.notify("That login is no longer running.");
    }
    Redirect::to("/settings")
}

fn report(app: &AppState, result: Result<(), String>) {
    if let Err(e) = result {
        app.notify(format!("Not saved: {e}"));
    }
}

/// Redirects to `/settings` or the dashboard, the only two pages that post
/// a `back` value.
fn back(to: Option<&str>) -> Redirect {
    match to {
        Some("settings") => Redirect::to("/settings"),
        _ => Redirect::to("/"),
    }
}

//! Request handlers. Every form posts, applies its change, sets a notice
//! when there is something to say, and redirects back to a page.

use agent_backend::{Cli, Provider};
use axum::Router;
use axum::extract::{Form, Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use work_queue::{RESULT_SUFFIX, State as ItemState, WorkItem};
use worker_pool::{BranchOutcome, Project, PullRequests};

use crate::{AppState, login, pages};

/// The most workers the UI allows.
pub(crate) const MAX_WORKERS: usize = 32;

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(dashboard))
        .route("/settings", get(settings_page))
        .route("/result", get(result))
        .route("/start", post(start))
        .route("/stop", post(stop))
        .route("/workers", post(set_workers))
        .route("/retry", post(retry))
        .route("/cleanup", post(clean_up))
        .route("/settings/active", post(set_active))
        .route("/settings/provider", post(save_provider))
        .route("/settings/projects", post(add_project))
        .route("/settings/projects/delete", post(delete_project))
        .route("/login", post(start_login))
        .route("/login/input", post(login_input))
}

async fn dashboard(State(app): State<AppState>) -> Html<String> {
    let notice = app.take_notice();
    Html(pages::dashboard(&app, notice).into_string())
}

async fn settings_page(State(app): State<AppState>) -> Html<String> {
    let notice = app.take_notice();
    let settings = app.settings();
    let cli_dir = settings.cli_dir.as_deref();
    let (claude, codex, gemini) = tokio::join!(
        login::status(Cli::Claude, cli_dir),
        login::status(Cli::Codex, cli_dir),
        login::status(Cli::Gemini, cli_dir),
    );
    let status = |cli| match cli {
        Cli::Claude => claude,
        Cli::Codex => codex,
        Cli::Gemini => gemini,
    };
    Html(pages::settings(&app, &settings, status, notice).into_string())
}

#[derive(Deserialize)]
struct ItemQuery {
    item: String,
}

async fn result(State(app): State<AppState>, Query(q): Query<ItemQuery>) -> Response {
    let queue = app.pool().queue();
    // Only items actually listed as closed are served, so the query cannot
    // name an arbitrary path.
    let closed = queue.list(ItemState::Closed).unwrap_or_default();
    let Some(item) = closed.into_iter().find(|i| i.to_string() == q.item) else {
        return (StatusCode::NOT_FOUND, "no such closed item").into_response();
    };
    let path = queue
        .path(ItemState::Closed, &item)
        .with_file_name(format!("{}{RESULT_SUFFIX}", item.stem()));
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

async fn retry(State(app): State<AppState>, Form(f): Form<ItemForm>) -> Redirect {
    if let Some((project, file)) = f.item.split_once('/') {
        app.pool().retry(&WorkItem::new(project, file));
    }
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

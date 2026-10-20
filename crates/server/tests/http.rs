//! The app over HTTP, through the router.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]

use std::fs;

use agent_backend::Provider;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use server::{AppState, SETTINGS_FILE, Settings};
use tower::ServiceExt;
use worker_pool::Scheme;

const PORT: u16 = 8787;
const HOST: &str = "127.0.0.1:8787";

struct App {
    dir: tempfile::TempDir,
    state: AppState,
}

/// An app over an empty work queue. Agent CLIs are looked up in an empty
/// directory, so login status checks never reach a real CLI.
fn app() -> App {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("work/open")).unwrap();
    fs::create_dir_all(dir.path().join("no-clis")).unwrap();
    let settings = Settings {
        cli_dir: Some(dir.path().join("no-clis")),
        ..Settings::default()
    };
    settings.save(dir.path()).unwrap();
    let state = AppState::new(dir.path(), PORT).unwrap();
    App { dir, state }
}

impl App {
    async fn send(&self, req: Request<Body>) -> (StatusCode, String) {
        let res = server::router(self.state.clone())
            .oneshot(req)
            .await
            .unwrap();
        let status = res.status();
        let body = to_bytes(res.into_body(), usize::MAX).await.unwrap();
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    async fn get(&self, path: &str) -> (StatusCode, String) {
        self.send(
            Request::get(path)
                .header(header::HOST, HOST)
                .body(Body::empty())
                .unwrap(),
        )
        .await
    }

    async fn post(&self, path: &str, form: &str) -> StatusCode {
        let req = Request::post(path)
            .header(header::HOST, HOST)
            .header(header::ORIGIN, format!("http://{HOST}"))
            .header("sec-fetch-site", "same-origin")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(form.to_string()))
            .unwrap();
        self.send(req).await.0
    }

    fn saved(&self) -> Settings {
        Settings::load(self.dir.path()).unwrap()
    }
}

#[tokio::test]
async fn dashboard_renders() {
    let app = app();
    let (status, body) = app.get("/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Workers"));
    assert!(body.contains("Stopped"));
    assert!(
        body.contains(r#"<noscript><meta http-equiv="refresh" content="2"></noscript>"#),
        "{body}"
    );
    for id in ["status", "usage", "workers", "queue"] {
        assert!(
            body.contains(&format!(r#"<div id="{id}" data-live>"#)),
            "{id}"
        );
    }
    assert!(body.contains(r#"location.pathname + "?live=1""#));

    // The settings page has nothing live.
    let (_, settings) = app.get("/settings").await;
    assert!(!settings.contains("data-live") && !settings.contains("<script>"));
}

#[tokio::test]
async fn importing_a_pull_request_reports_why_it_cannot() {
    let app = app();
    let (_, body) = app.get("/").await;
    assert!(body.contains(r#"action="/import""#), "{body}");

    let status = app.post("/import", "url=not+a+link").await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (_, page) = app.get("/").await;
    assert!(page.contains("is not a GitHub pull request link"), "{page}");

    app.post(
        "/import",
        "url=https%3A%2F%2Fgithub.com%2Fup%2Frepo%2Fpull%2F9",
    )
    .await;
    let (_, page) = app.get("/").await;
    assert!(
        page.contains("no project opens pull requests against up/repo"),
        "{page}"
    );
}

#[tokio::test]
async fn live_refresh_leaves_notices_for_the_next_page() {
    let app = app();
    app.post("/settings/active", "provider=anthropic").await;
    let notice = "Workers now use Anthropic (Claude)";
    let (_, live) = app.get("/?live=1").await;
    assert!(!live.contains(notice));
    let (_, page) = app.get("/").await;
    assert!(page.contains(notice), "{page}");
}

#[tokio::test]
async fn dashboard_shows_recorded_plan_usage() {
    let app = app();
    let (_, body) = app.get("/").await;
    assert!(body.contains("No usage reported yet"));

    // A window that resets far in the future, and one that already has.
    fs::write(
        app.dir.path().join("work/usage.json"),
        r#"{"anthropic":{"session":{"percent":64,"resets_at":4000000000},"weekly":{"percent":9,"resets_at":1}}}"#,
    )
    .unwrap();
    let app = App {
        state: AppState::new(app.dir.path(), PORT).unwrap(),
        ..app
    };
    let (_, body) = app.get("/").await;
    assert!(
        body.contains(r#"<strong class="warn">64%</strong>"#),
        "{body}"
    );
    assert!(body.contains("Past the 50% warning"));
    assert!(body.contains("resets in "));
    assert!(body.contains("<strong>0%</strong>"));
    assert!(body.contains("reset since last report"));
}

#[tokio::test]
async fn stop_level_offers_an_override() {
    let app = app();
    fs::write(
        app.dir.path().join("work/usage.json"),
        r#"{"anthropic":{"session":{"percent":86,"resets_at":4000000000}}}"#,
    )
    .unwrap();
    let app = App {
        state: AppState::new(app.dir.path(), PORT).unwrap(),
        ..app
    };
    let (_, body) = app.get("/").await;
    assert!(
        body.contains(r#"<strong class="bad">86%</strong>"#),
        "{body}"
    );
    assert!(body.contains("Stopped at the 85% level"));
    assert!(body.contains(r#"action="/usage/override""#));

    let status = app.post("/usage/override", "provider=anthropic").await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(
        app.state.pool().snapshot().overridden,
        vec![Provider::Anthropic]
    );
    let (_, body) = app.get("/").await;
    assert!(body.contains("overridden until the window resets"));
}

#[tokio::test]
async fn usage_limits_are_saved_and_checked() {
    let app = app();
    let status = app
        .post(
            "/settings/limits",
            "warn=70%2C+60%2C+60&stop=90&fallback1=gemini&fallback2=&fallback3=open_ai&fallback4=",
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let saved = app.saved();
    assert_eq!(saved.limits.warn, vec![60, 70]);
    assert_eq!(saved.limits.stop, 90);
    assert!(!saved.limits.failover, "an unticked box turns failover off");
    assert_eq!(saved.fallbacks, vec![Provider::Gemini, Provider::OpenAi]);
    assert_eq!(app.state.pool().config().limits.stop, 90);

    let (_, page) = app.get("/settings").await;
    assert!(page.contains(r#"value="60, 70""#), "{page}");

    // A warning at or above the stop level is refused, and nothing changes.
    app.post("/settings/limits", "warn=95&stop=90&failover=on")
        .await;
    assert_eq!(app.saved().limits.warn, vec![60, 70]);
    let (_, page) = app.get("/settings").await;
    assert!(
        page.contains("Not saved: warning levels must be above 0 and below the 90% stop level")
    );
}

#[tokio::test]
async fn balancing_is_saved_with_weights() {
    let app = app();
    app.post(
        "/settings/limits",
        "warn=50&stop=85&failover=on&fallback1=gemini",
    )
    .await;
    let (_, page) = app.get("/settings").await;
    assert!(page.contains(r#"name="weight_gemini""#), "{page}");
    assert!(!page.contains(r#"name="weight_open_ai""#));

    let status = app
        .post(
            "/settings/balancing",
            "scheme=weighted&weight_anthropic=3&weight_gemini=1",
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let saved = app.saved().balancing;
    assert_eq!(saved.scheme, Scheme::Weighted);
    assert_eq!(saved.weight(Provider::Anthropic), 3);
    assert_eq!(saved.weight(Provider::Gemini), 1);
    assert!(
        !saved.rework_any_provider,
        "an unticked box keeps reworks on their provider"
    );
    assert_eq!(app.state.pool().config().balancing, saved);
    let (_, page) = app.get("/").await;
    assert!(page.contains("Balancing: Weighted"));

    app.post("/settings/balancing", "scheme=pace&weight_anthropic=0")
        .await;
    assert_eq!(app.saved().balancing.scheme, Scheme::Weighted);
    let (_, page) = app.get("/settings").await;
    assert!(
        page.contains("weight must be a whole number from 1 to 100"),
        "{page}"
    );
}

#[tokio::test]
async fn foreign_host_is_refused() {
    let app = app();
    let req = Request::get("/")
        .header(header::HOST, "attacker.test:8787")
        .body(Body::empty())
        .unwrap();
    assert_eq!(app.send(req).await.0, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn cross_site_post_is_refused() {
    let app = app();
    let req = Request::post("/workers")
        .header(header::HOST, HOST)
        .header(header::ORIGIN, "https://attacker.test")
        .header("sec-fetch-site", "cross-site")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from("count=9"))
        .unwrap();
    assert_eq!(app.send(req).await.0, StatusCode::FORBIDDEN);
    assert_eq!(app.saved().workers, 1);
}

#[tokio::test]
async fn worker_count_is_saved_and_applied() {
    let app = app();
    assert_eq!(app.post("/workers", "count=3").await, StatusCode::SEE_OTHER);
    assert_eq!(app.saved().workers, 3);
    assert_eq!(app.state.pool().config().workers, 3);

    // Capped at the UI's maximum.
    app.post("/workers", "count=500").await;
    assert_eq!(app.saved().workers, 32);
}

#[tokio::test]
async fn api_key_is_saved_but_never_shown() {
    let app = app();
    let status = app
        .post(
            "/settings/provider",
            "provider=open_ai&use_api_key=on&api_key=sk-very-secret&model=gpt-5&effort=high",
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let saved = app.saved();
    let openai = saved.provider(Provider::OpenAi).unwrap();
    assert_eq!(openai.api_key, "sk-very-secret");
    assert!(openai.use_api_key);
    assert_eq!(openai.model, "gpt-5");
    assert_eq!(openai.effort, "high");
    assert_eq!(openai.config().effort.as_deref(), Some("high"));

    let (_, page) = app.get("/settings").await;
    assert!(!page.contains("sk-very-secret"));
    assert!(page.contains("saved — leave blank to keep"));
    assert!(page.contains(r#"<option value="high" selected>high</option>"#));

    // A blank key field keeps the key.
    app.post(
        "/settings/provider",
        "provider=open_ai&use_api_key=on&api_key=",
    )
    .await;
    assert_eq!(
        app.saved().provider(Provider::OpenAi).unwrap().api_key,
        "sk-very-secret"
    );
}

#[tokio::test]
async fn disabling_a_provider_is_saved_and_the_active_one_is_protected() {
    let app = app();
    app.post(
        "/settings/provider/enabled",
        "provider=gemini&enabled=false",
    )
    .await;
    assert!(!app.saved().provider(Provider::Gemini).unwrap().enabled);

    // Disabled providers can't be made active.
    app.post("/settings/active", "provider=gemini").await;
    assert_ne!(app.saved().active, Provider::Gemini);

    // The active provider can't be disabled.
    app.post(
        "/settings/provider/enabled",
        "provider=anthropic&enabled=false",
    )
    .await;
    assert!(app.saved().provider(Provider::Anthropic).unwrap().enabled);

    app.post("/settings/provider/enabled", "provider=gemini&enabled=true")
        .await;
    assert!(app.saved().provider(Provider::Gemini).unwrap().enabled);
}

#[tokio::test]
async fn unrunnable_active_provider_is_rejected() {
    let app = app();
    app.post("/settings/active", "provider=deepseek").await;
    assert_eq!(app.saved().active, Provider::Anthropic);
    let (_, page) = app.get("/settings").await;
    assert!(
        page.contains("Not saved: Deepseek needs an API key"),
        "{page}"
    );

    app.post("/settings/provider", "provider=deepseek&api_key=sk-ds")
        .await;
    app.post("/settings/active", "provider=deepseek").await;
    assert_eq!(app.saved().active, Provider::Deepseek);
    assert_eq!(
        app.state.pool().config().provider.provider,
        Provider::Deepseek
    );
}

#[tokio::test]
async fn start_needs_a_project() {
    let app = app();
    app.post("/start", "").await;
    assert!(!app.state.pool().snapshot().running);
    let (_, page) = app.get("/").await;
    assert!(page.contains("add a project in Settings first"));
}

#[tokio::test]
async fn projects_are_validated() {
    let app = app();
    fs::create_dir_all(app.dir.path().join("spec/demo")).unwrap();
    app.post(
        "/settings/projects",
        "name=demo&spec_dir=spec/demo&repo=nowhere",
    )
    .await;
    assert!(app.saved().projects.is_empty());
    let (_, page) = app.get("/settings").await;
    assert!(page.contains("nowhere is not a git repository"), "{page}");

    fs::create_dir_all(app.dir.path().join("repo/.git")).unwrap();
    app.post(
        "/settings/projects",
        "name=demo&spec_dir=spec/demo&repo=repo&base_ref=develop&setup=make+data",
    )
    .await;
    let projects = app.saved().projects;
    assert_eq!(projects.len(), 1);
    assert_eq!(projects[0].base_ref, "develop");
    assert_eq!(projects[0].setup.as_deref(), Some("make data"));
    assert_eq!(projects[0].pr, None);
    assert_eq!(projects[0].build_jobs, 4);

    app.post(
        "/settings/projects",
        "name=demo&spec_dir=spec/demo&repo=repo&build_jobs=8",
    )
    .await;
    assert_eq!(app.saved().projects[0].build_jobs, 8);
    // Out of range is rejected, keeping the last settings.
    app.post(
        "/settings/projects",
        "name=demo&spec_dir=spec/demo&repo=repo&build_jobs=0",
    )
    .await;
    assert_eq!(app.saved().projects[0].build_jobs, 8);

    app.post(
        "/settings/projects",
        "name=demo&spec_dir=spec/demo&repo=repo&pr_remote=fork&pr_repo=up%2Frepo",
    )
    .await;
    let pr = app.saved().projects[0].pr.clone().unwrap();
    assert_eq!(
        (pr.push_remote.as_str(), pr.repo.as_str(), pr.base.as_str()),
        ("fork", "up/repo", "main")
    );
    assert!(!pr.draft);

    // A remote without a repository is rejected, keeping the last settings.
    app.post(
        "/settings/projects",
        "name=demo&spec_dir=spec/demo&repo=repo&pr_remote=fork",
    )
    .await;
    assert!(app.saved().projects[0].pr.is_some());

    app.post(
        "/settings/projects",
        "name=..%2Fx&spec_dir=spec/demo&repo=repo",
    )
    .await;
    assert_eq!(app.saved().projects.len(), 1);

    app.post("/settings/projects/delete", "name=demo").await;
    assert!(app.saved().projects.is_empty());
}

#[tokio::test]
async fn result_serves_only_closed_items() {
    let app = app();
    let closed = app.dir.path().join("work/closed/demo");
    fs::create_dir_all(&closed).unwrap();
    fs::write(closed.join("a.txt"), "item").unwrap();
    fs::write(closed.join("a.result.md"), "# all good").unwrap();

    let (status, body) = app.get("/result?item=demo%2Fa.txt").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("# all good"));

    let (status, _) = app.get("/result?item=..%2F..%2Fetc%2Fpasswd").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn settings_file_is_not_written_by_reads() {
    let app = app();
    let before = fs::read_to_string(app.dir.path().join(SETTINGS_FILE)).unwrap();
    app.get("/settings").await;
    app.get("/").await;
    let after = fs::read_to_string(app.dir.path().join(SETTINGS_FILE)).unwrap();
    assert_eq!(before, after);
}

#[tokio::test]
async fn clean_up_refuses_items_that_are_not_closed() {
    let app = app();
    let open = app.dir.path().join("work/open/demo");
    fs::create_dir_all(&open).unwrap();
    fs::write(open.join("a.txt"), "item").unwrap();

    assert_eq!(
        app.post("/cleanup", "item=demo%2Fa.txt").await,
        StatusCode::SEE_OTHER
    );
    let (_, page) = app.get("/").await;
    assert!(
        page.contains("Could not clean up demo/a.txt: demo/a.txt is not closed"),
        "{page}"
    );
    assert!(open.join("a.txt").is_file());
}

#[tokio::test]
async fn held_items_show_their_reason_and_can_be_returned() {
    let app = app();
    let hold = app.dir.path().join("work/hold/demo");
    fs::create_dir_all(&hold).unwrap();
    fs::write(hold.join("a.txt"), "item").unwrap();
    fs::write(
        hold.join("a.meta.json"),
        r#"{"turns": 3, "last_error": "boom", "hold_reason": "used 3 of 3 turns; the last one failed: boom"}"#,
    )
    .unwrap();

    let (_, page) = app.get("/").await;
    assert!(
        page.contains("on hold: used 3 of 3 turns; the last one failed: boom"),
        "{page}"
    );
    assert!(page.contains("Return to agent"));

    assert_eq!(
        app.post("/resume", "item=demo%2Fa.txt").await,
        StatusCode::SEE_OTHER
    );
    let open = app.dir.path().join("work/open/demo");
    assert!(open.join("a.txt").is_file());
    let meta = fs::read_to_string(open.join("a.meta.json")).unwrap();
    assert!(meta.contains(r#""turns": 0"#), "{meta}");
    assert!(!meta.contains("hold_reason"), "{meta}");
}

#[tokio::test]
async fn max_turns_is_saved_and_clamped() {
    let app = app();
    assert_eq!(app.saved().max_turns, 3);
    app.post("/settings/max-turns", "max_turns=5").await;
    assert_eq!(app.saved().max_turns, 5);
    assert_eq!(app.state.pool().config().max_turns, 5);
    app.post("/settings/max-turns", "max_turns=0").await;
    assert_eq!(app.saved().max_turns, 1);
}

#[tokio::test]
async fn in_review_items_show_their_pull_request_and_status() {
    let app = app();
    let review = app.dir.path().join("work/inreview/demo");
    fs::create_dir_all(&review).unwrap();
    fs::write(review.join("a.txt"), "item").unwrap();
    fs::write(review.join("a.result.md"), "# reworked once").unwrap();
    fs::write(
        review.join("a.meta.json"),
        r#"{"turns": 2, "pr_url": "https://github.com/up/repo/pull/7", "needs_rework": "Rename it."}"#,
    )
    .unwrap();
    fs::write(review.join("b.txt"), "item").unwrap();
    fs::write(
        review.join("b.meta.json"),
        r#"{"turns": 1, "pr_url": "https://github.com/up/repo/pull/8"}"#,
    )
    .unwrap();

    let (_, page) = app.get("/").await;
    assert!(
        page.contains("changes requested; waiting for a worker (2 of 3 turns used)"),
        "{page}"
    );
    assert!(page.contains("in review (1 of 3 turns used)"), "{page}");
    assert!(page.contains(r#"href="https://github.com/up/repo/pull/7""#));
    assert!(page.contains("Next action"), "{page}");
    assert!(page.contains(r#"action="/review/check""#), "{page}");
    assert!(page.contains(r#"action="/review/bump""#), "{page}");

    let (status, body) = app.get("/result?item=demo%2Fa.txt").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("# reworked once"));
}

#[tokio::test]
async fn history_page_loads_its_scripts_from_the_app() {
    let app = app();
    let (status, body) = app.get("/history").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(r#"src="/static/chart.umd.js""#));
    assert!(body.contains(r#"src="/static/history.js""#));
    assert!(body.contains(r#"href="/history""#));
    assert!(
        !body.contains("https://") && !body.contains("http://cdn"),
        "{body}"
    );

    for (name, marker) in [
        ("chart.umd.js", "Chart.js v4"),
        ("history.js", "history dashboard"),
    ] {
        let (status, script) = app.get(&format!("/static/{name}")).await;
        assert_eq!(status, StatusCode::OK, "{name}");
        assert!(script.contains(marker), "{name}");
    }
    assert_eq!(
        app.get("/static/settings.toml").await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        app.get("/static/..%2fCargo.toml").await.0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn history_json_summarizes_the_ledger() {
    let app = app();
    let (status, body) = app.get("/history.json").await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["items"], serde_json::json!([]));

    let turn = r#"{"kind":"turn","project":"demo","item":"a.txt","turn":1,"reason":"first","provider":"anthropic","fallback":false,"models":["claude-x"],"effort":"medium","started_at":1000,"ended_at":1100,"duration_ms":100000,"agent_ms":90000,"outcome":"closed","metrics":{"input_tokens":7,"output_tokens":3},"commits":1}"#;
    fs::write(
        app.dir.path().join("work/history.jsonl"),
        format!("{turn}\n"),
    )
    .unwrap();
    let (_, body) = app.get("/history.json").await;
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    let item = &json["items"][0];
    assert_eq!(item["item"], "a.txt");
    assert_eq!(item["finished"], "closed");
    assert_eq!(item["providers"], serde_json::json!(["anthropic"]));
    assert_eq!(item["models"], serde_json::json!(["claude-x"]));
    assert_eq!(item["input_tokens"], 7);
    assert_eq!(item["records"][0]["effort"], "medium");
}

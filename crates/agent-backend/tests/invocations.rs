//! The CLI invocation built for every provider and auth mode.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]

use std::path::{Path, PathBuf};

use agent_backend::{Auth, ConfigError, Invocation, Job, Provider, ProviderConfig};

fn key() -> Auth {
    Auth::ApiKey {
        key: "sk-test".into(),
    }
}

fn run(config: &ProviderConfig) -> Invocation {
    let extra = [PathBuf::from("/spec/phase")];
    let job = Job {
        prompt: "PROMPT",
        workdir: Path::new("/wt/phase/a"),
        extra_dirs: &extra,
    };
    config.invocation(&job).unwrap()
}

fn has_pair(run: &Invocation, flag: &str, value: &str) -> bool {
    run.args.windows(2).any(|w| w[0] == flag && w[1] == value)
}

#[test]
fn every_run_works_in_the_worktree() {
    let configs = [
        ProviderConfig::new(Provider::Anthropic, Auth::Plan),
        ProviderConfig::new(Provider::Gemini, Auth::Plan),
        ProviderConfig::new(Provider::OpenAi, Auth::Plan),
        ProviderConfig::new(Provider::Deepseek, key()),
    ];
    for config in &configs {
        let run = run(config);
        assert_eq!(run.cwd.as_deref(), Some(Path::new("/wt/phase/a")));
        assert!(run.args.iter().any(|a| a == "PROMPT"), "{config:?}");
        assert!(run.args.iter().any(|a| a == "/spec/phase"), "{config:?}");
    }
}

#[test]
fn anthropic_plan_strips_inherited_keys() {
    let run = run(&ProviderConfig::new(Provider::Anthropic, Auth::Plan));
    assert_eq!(run.program, "claude");
    assert!(has_pair(&run, "-p", "PROMPT"));
    assert!(has_pair(&run, "--output-format", "stream-json"));
    assert!(has_pair(&run, "--add-dir", "/spec/phase"));
    assert!(run.args.iter().any(|a| a == "--verbose"));
    assert!(run.env.is_empty());
    assert!(run.env_remove.contains(&"ANTHROPIC_API_KEY".to_string()));
}

#[test]
fn anthropic_key_and_model() {
    let mut config = ProviderConfig::new(Provider::Anthropic, key());
    config.model = Some("claude-opus-5-5".into());
    let run = run(&config);
    assert_eq!(run.env_value("ANTHROPIC_API_KEY"), Some("sk-test"));
    assert!(has_pair(&run, "--model", "claude-opus-5-5"));
}

#[test]
fn deepseek_uses_claude_against_anthropic_compatible_endpoint() {
    let run = run(&ProviderConfig::new(Provider::Deepseek, key()));
    assert_eq!(run.program, "claude");
    assert_eq!(
        run.env_value("ANTHROPIC_BASE_URL"),
        Some("https://api.deepseek.com/anthropic")
    );
    assert_eq!(run.env_value("ANTHROPIC_AUTH_TOKEN"), Some("sk-test"));
    assert_eq!(run.env_value("ANTHROPIC_MODEL"), Some("deepseek-chat"));
    assert_eq!(run.env_value("ANTHROPIC_API_KEY"), None);
}

#[test]
fn deepseek_plan_is_rejected() {
    let config = ProviderConfig::new(Provider::Deepseek, Auth::Plan);
    let job = Job {
        prompt: "",
        workdir: Path::new("/wt"),
        extra_dirs: &[],
    };
    assert_eq!(
        config.invocation(&job),
        Err(ConfigError::PlanUnsupported(Provider::Deepseek))
    );
}

#[test]
fn openai_plan_and_key() {
    let plan = run(&ProviderConfig::new(Provider::OpenAi, Auth::Plan));
    assert_eq!(plan.program, "codex");
    assert_eq!(plan.args[..2], ["exec", "--json"]);
    assert!(has_pair(&plan, "--cd", "/wt/phase/a"));
    assert!(has_pair(&plan, "--sandbox", "workspace-write"));
    assert!(has_pair(&plan, "--add-dir", "/spec/phase"));
    assert_eq!(plan.args.last().map(String::as_str), Some("PROMPT"));
    assert!(plan.env.is_empty());
    assert!(plan.env_remove.contains(&"OPENAI_API_KEY".to_string()));

    let keyed = run(&ProviderConfig::new(Provider::OpenAi, key()));
    assert_eq!(keyed.env_value("CODEX_API_KEY"), Some("sk-test"));
}

#[test]
fn openai_compatible_defines_a_responses_provider() {
    let mut config = ProviderConfig::new(Provider::OpenAiCompatible, key());
    config.base_url = Some("http://localhost:8000/v1".into());
    config.model = Some("qwen3-coder".into());
    let run = run(&config);
    assert_eq!(run.program, "codex");
    assert!(has_pair(&run, "-c", "model_provider=\"aiwc\""));
    assert!(has_pair(
        &run,
        "-c",
        "model_providers.aiwc.base_url=\"http://localhost:8000/v1\""
    ));
    assert!(has_pair(
        &run,
        "-c",
        "model_providers.aiwc.wire_api=\"responses\""
    ));
    assert!(has_pair(&run, "--model", "qwen3-coder"));
    assert_eq!(run.env_value("AIWC_PROVIDER_API_KEY"), Some("sk-test"));
}

#[test]
fn openai_compatible_without_key_sets_no_env_key() {
    let mut config = ProviderConfig::new(
        Provider::OpenAiCompatible,
        Auth::ApiKey { key: String::new() },
    );
    config.base_url = Some("http://localhost:11434/v1".into());
    config.model = Some("llama".into());
    let run = run(&config);
    assert!(!run.args.iter().any(|a| a.contains("env_key")));
    assert!(run.env.is_empty());
}

#[test]
fn gemini_plan_and_key() {
    let plan = run(&ProviderConfig::new(Provider::Gemini, Auth::Plan));
    assert_eq!(plan.program, "gemini");
    assert!(has_pair(&plan, "-p", "PROMPT"));
    assert!(has_pair(&plan, "--output-format", "stream-json"));
    assert!(has_pair(&plan, "--approval-mode", "yolo"));
    assert!(has_pair(&plan, "--include-directories", "/spec/phase"));
    assert!(plan.args.iter().any(|a| a == "--skip-trust"));
    assert!(plan.env_remove.contains(&"GEMINI_API_KEY".to_string()));

    let keyed = run(&ProviderConfig::new(Provider::Gemini, key()));
    assert_eq!(keyed.env_value("GEMINI_API_KEY"), Some("sk-test"));
}

#[test]
fn config_round_trips_through_json() {
    let mut config = ProviderConfig::new(Provider::OpenAiCompatible, key());
    config.base_url = Some("http://x/v1".into());
    let json = serde_json::to_string(&config).unwrap();
    assert!(json.contains(r#""mode":"api_key""#), "{json}");
    let back: ProviderConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(back, config);
}

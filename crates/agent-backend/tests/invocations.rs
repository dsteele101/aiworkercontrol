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
    let writable = [PathBuf::from("/cache/target-1")];
    let job = Job {
        prompt: "PROMPT",
        workdir: Path::new("/wt/phase/a"),
        extra_dirs: &extra,
        writable_dirs: &writable,
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
        let prompted =
            run.args.iter().any(|a| a == "PROMPT") || run.stdin.as_deref() == Some("PROMPT");
        assert!(prompted, "{config:?}");
        assert!(run.args.iter().any(|a| a == "/spec/phase"), "{config:?}");
    }
}

#[test]
fn anthropic_plan_strips_inherited_keys() {
    let run = run(&ProviderConfig::new(Provider::Anthropic, Auth::Plan));
    assert_eq!(run.program, "claude");
    assert!(run.args.iter().any(|a| a == "-p"));
    assert!(!run.args.iter().any(|a| a == "PROMPT"));
    assert_eq!(run.stdin.as_deref(), Some("PROMPT"));
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
fn effort_is_passed_to_claude_and_codex() {
    let mut config = ProviderConfig::new(Provider::Anthropic, Auth::Plan);
    config.effort = Some("xhigh".into());
    assert!(has_pair(&run(&config), "--effort", "xhigh"));

    config.provider = Provider::OpenAi;
    config.effort = Some("high".into());
    let codex = run(&config);
    assert!(has_pair(&codex, "-c", "model_reasoning_effort=\"high\""));
    assert!(!codex.args.contains(&"--effort".to_string()));
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
    assert!(!run.env_remove.contains(&"ANTHROPIC_AUTH_TOKEN".to_string()));
}

#[test]
fn openrouter_uses_claude_against_anthropic_compatible_endpoint() {
    let mut config = ProviderConfig::new(Provider::OpenRouter, key());
    let run = run(&config);
    assert_eq!(run.program, "claude");
    assert_eq!(
        run.env_value("ANTHROPIC_BASE_URL"),
        Some("https://openrouter.ai/api")
    );
    assert_eq!(run.env_value("ANTHROPIC_AUTH_TOKEN"), Some("sk-test"));
    assert_eq!(run.env_value("ANTHROPIC_API_KEY"), Some(""));
    // Removals run after the env is set, so they must not cancel it.
    assert!(!run.env_remove.contains(&"ANTHROPIC_AUTH_TOKEN".to_string()));

    config.model = Some("openai/gpt-5".into());
    assert_eq!(
        self::run(&config).env_value("ANTHROPIC_MODEL"),
        Some("openai/gpt-5")
    );
    config.auth = Auth::Plan;
    assert!(config.validate().is_err());
}

#[test]
fn deepseek_plan_is_rejected() {
    let config = ProviderConfig::new(Provider::Deepseek, Auth::Plan);
    let job = Job {
        prompt: "",
        workdir: Path::new("/wt"),
        extra_dirs: &[],
        writable_dirs: &[],
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
    assert_eq!(plan.stdin.as_deref(), Some("PROMPT"));
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
fn cloudflare_defines_a_responses_provider_with_a_default_model() {
    let mut config = ProviderConfig::new(Provider::Cloudflare, key());
    config.base_url = Some("https://api.cloudflare.com/client/v4/accounts/abc/ai/v1".into());
    let run = run(&config);
    assert_eq!(run.program, "codex");
    assert!(has_pair(
        &run,
        "-c",
        "model_providers.aiwc.base_url=\"https://api.cloudflare.com/client/v4/accounts/abc/ai/v1\""
    ));
    assert!(has_pair(&run, "--model", "@cf/openai/gpt-oss-120b"));
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
fn gemini_runs_through_antigravity() {
    let mut config = ProviderConfig::new(Provider::Gemini, Auth::Plan);
    config.model = Some("gemini-3.8-flash-high".into());
    config.effort = Some("high".into());
    let plan = run(&config);
    assert_eq!(plan.program, "agy");
    assert!(has_pair(&plan, "-p", "PROMPT"));
    assert!(has_pair(&plan, "--output-format", "stream-json"));
    assert!(
        plan.args
            .iter()
            .any(|a| a == "--dangerously-skip-permissions")
    );
    assert!(has_pair(&plan, "--add-dir", "/spec/phase"));
    assert!(has_pair(&plan, "--model", "gemini-3.8-flash-high"));
    assert!(has_pair(&plan, "--effort", "high"));
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

#[test]
fn only_codex_is_told_about_writable_dirs() {
    let codex = run(&ProviderConfig::new(Provider::OpenAi, Auth::Plan));
    assert!(has_pair(&codex, "--add-dir", "/cache/target-1"));

    for provider in [Provider::Anthropic, Provider::Gemini] {
        let other = run(&ProviderConfig::new(provider, Auth::Plan));
        assert!(
            !other.args.iter().any(|a| a == "/cache/target-1"),
            "{provider:?}"
        );
    }
}

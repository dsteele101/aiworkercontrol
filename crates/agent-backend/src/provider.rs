//! Providers, their settings, and the CLI invocation each one needs.

use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Deepseek's Anthropic-compatible endpoint, used when no base URL is set.
const DEEPSEEK_BASE_URL: &str = "https://api.deepseek.com/anthropic";
/// The model used for Deepseek when none is set.
const DEEPSEEK_MODEL: &str = "deepseek-chat";
/// OpenRouter's Anthropic-compatible endpoint, used when no base URL is set.
pub(crate) const OPENROUTER_BASE_URL: &str = "https://openrouter.ai/api";
/// The model used for OpenRouter when none is set.
const OPENROUTER_MODEL: &str = "anthropic/claude-sonnet-4.5";
/// Env var carrying an OpenAI-compatible endpoint's key into `codex`.
const COMPATIBLE_KEY_ENV: &str = "AIWC_PROVIDER_API_KEY";

/// The model used for Cloudflare Workers AI when none is set.
const CLOUDFLARE_MODEL: &str = "@cf/openai/gpt-oss-120b";

/// An AI system a worker can use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    /// Anthropic (Claude).
    Anthropic,
    /// Google Gemini.
    Gemini,
    /// OpenAI.
    OpenAi,
    /// Deepseek.
    Deepseek,
    /// OpenRouter.
    OpenRouter,
    /// Any endpoint speaking the OpenAI Responses API.
    OpenAiCompatible,
    /// Mistral.
    Mistral,
    /// Cloudflare Workers AI.
    Cloudflare,
}

impl Provider {
    /// Every provider, in display order.
    pub const ALL: [Provider; 8] = [
        Provider::Anthropic,
        Provider::Gemini,
        Provider::OpenAi,
        Provider::Deepseek,
        Provider::OpenRouter,
        Provider::OpenAiCompatible,
        Provider::Mistral,
        Provider::Cloudflare,
    ];

    /// A human-readable name.
    pub fn label(self) -> &'static str {
        match self {
            Provider::Anthropic => "Anthropic (Claude)",
            Provider::Gemini => "Gemini (Antigravity)",
            Provider::OpenAi => "OpenAI",
            Provider::Deepseek => "Deepseek",
            Provider::OpenRouter => "OpenRouter",
            Provider::OpenAiCompatible => "OpenAI-compatible",
            Provider::Mistral => "Mistral (Vibe)",
            Provider::Cloudflare => "Cloudflare Workers AI",
        }
    }

    /// The CLI that runs this provider's agents.
    pub fn cli(self) -> Cli {
        match self {
            Provider::Anthropic | Provider::Deepseek | Provider::OpenRouter => Cli::Claude,
            Provider::OpenAi | Provider::OpenAiCompatible | Provider::Cloudflare => Cli::Codex,
            Provider::Gemini => Cli::Antigravity,
            Provider::Mistral => Cli::Vibe,
        }
    }

    /// Whether the provider can run on a plan login rather than an API key.
    ///
    /// ```
    /// use agent_backend::Provider;
    /// assert!(Provider::Anthropic.supports_plan());
    /// assert!(!Provider::Deepseek.supports_plan());
    /// ```
    pub fn supports_plan(self) -> bool {
        matches!(
            self,
            Provider::Anthropic | Provider::OpenAi | Provider::Gemini | Provider::Mistral
        )
    }
}

/// One of the vendor CLIs this crate drives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Cli {
    /// Claude Code, `claude`.
    Claude,
    /// OpenAI Codex, `codex`.
    Codex,
    /// Antigravity CLI, `agy`, which replaced the Gemini CLI for Google
    /// plan logins.
    Antigravity,
    /// Mistral Vibe, `vibe`.
    Vibe,
}

impl Cli {
    /// The executable name.
    pub fn program(self) -> &'static str {
        match self {
            Cli::Claude => "claude",
            Cli::Codex => "codex",
            Cli::Antigravity => "agy",
            Cli::Vibe => "vibe",
        }
    }

    /// The effort levels the CLI accepts, lowest first.
    ///
    /// ```
    /// use agent_backend::Cli;
    /// assert!(Cli::Claude.effort_levels().contains(&"max"));
    /// assert!(!Cli::Codex.effort_levels().contains(&"max"));
    /// ```
    pub fn effort_levels(self) -> &'static [&'static str] {
        match self {
            Cli::Claude => &["low", "medium", "high", "xhigh", "max"],
            Cli::Codex => &["minimal", "low", "medium", "high", "xhigh"],
            // From `agy --help` (1.4.2); its docs list only low to high.
            Cli::Antigravity => &["low", "medium", "high", "xhigh", "max"],
            // `vibe` has no effort setting.
            Cli::Vibe => &[],
        }
    }

    /// Env vars that would make the CLI use an API key instead of the plan
    /// login, so a plan run must not inherit them.
    fn key_env_vars(self) -> &'static [&'static str] {
        match self {
            Cli::Claude => &["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN"],
            Cli::Codex => &["CODEX_API_KEY", "OPENAI_API_KEY"],
            Cli::Antigravity => &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
            Cli::Vibe => &["MISTRAL_API_KEY"],
        }
    }
}

/// How a provider authenticates.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum Auth {
    /// The user's plan, via the CLI's own stored login.
    Plan,
    /// An API key. May be empty for a local OpenAI-compatible endpoint.
    ApiKey {
        /// The key.
        key: String,
    },
}

impl fmt::Debug for Auth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Auth::Plan => f.write_str("Plan"),
            Auth::ApiKey { .. } => f.write_str("ApiKey(<redacted>)"),
        }
    }
}

/// A model (and effort) a provider's worker may be given, and what it suits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelOption {
    /// The model to request; the CLI's default when `None`.
    #[serde(default)]
    pub model: Option<String>,
    /// The effort level; the CLI's default when `None`.
    #[serde(default)]
    pub effort: Option<String>,
    /// When to use it, for the agent that chooses between the options.
    #[serde(default)]
    pub description: Option<String>,
}

impl ModelOption {
    /// A short name: the model and effort, or "default".
    pub fn label(&self) -> String {
        let model = non_empty(&self.model).unwrap_or("default model");
        match non_empty(&self.effort) {
            Some(effort) => format!("{model}, {effort} effort"),
            None => model.to_string(),
        }
    }
}

/// Settings for one provider.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderConfig {
    /// Which provider this configures.
    pub provider: Provider,
    /// How it authenticates.
    pub auth: Auth,
    /// The model to request; the CLI's default when `None`.
    #[serde(default)]
    pub model: Option<String>,
    /// The effort level, one of the CLI's [`Cli::effort_levels`]; the CLI's
    /// default when `None`.
    #[serde(default)]
    pub effort: Option<String>,
    /// The endpoint, for Deepseek and OpenRouter (optional) and OpenAI-compatible
    /// and Cloudflare (required; Cloudflare's holds the account ID).
    #[serde(default)]
    pub base_url: Option<String>,
    /// A token that may read the account's analytics, for Cloudflare: the
    /// inference key cannot, and Workers AI reports usage only there.
    #[serde(default)]
    pub analytics_token: Option<String>,
    /// Other models a worker may be given instead of `model` when the work
    /// suits them better.
    #[serde(default)]
    pub alternatives: Vec<ModelOption>,
}

impl fmt::Debug for ProviderConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderConfig")
            .field("provider", &self.provider)
            .field("auth", &self.auth)
            .field("model", &self.model)
            .field("effort", &self.effort)
            .field("base_url", &self.base_url)
            .field(
                "analytics_token",
                &self.analytics_token.as_ref().map(|_| "<redacted>"),
            )
            .field("alternatives", &self.alternatives)
            .finish()
    }
}

/// A provider configuration that cannot be run.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// The provider has no plan login; it needs an API key.
    #[error("{} has no plan login; configure an API key", .0.label())]
    PlanUnsupported(Provider),
    /// The provider needs an API key and none was given.
    #[error("{} needs an API key", .0.label())]
    MissingKey(Provider),
    /// An OpenAI-compatible endpoint needs a base URL.
    #[error("{} needs a base URL", .0.label())]
    MissingBaseUrl(Provider),
    /// An OpenAI-compatible endpoint needs a model name.
    #[error("OpenAI-compatible provider needs a model")]
    MissingModel,
    /// The provider's CLI does not accept this effort level.
    #[error("{} does not accept effort level {:?}", .0.label(), .1)]
    UnsupportedEffort(Provider, String),
}

/// The work a single agent run should do.
#[derive(Debug, Clone, Copy)]
pub struct Job<'a> {
    /// The full prompt.
    pub prompt: &'a str,
    /// The directory the agent works in (a git worktree).
    pub workdir: &'a Path,
    /// Further directories the agent may read and write, such as the spec dir.
    pub extra_dirs: &'a [PathBuf],
    /// Directories the agent's commands must be able to write but that are
    /// not part of its context, such as a build cache. Only sandboxing CLIs
    /// (`codex`) are told about them.
    pub writable_dirs: &'a [PathBuf],
}

/// A process to run: program, arguments, environment and working directory.
#[derive(Clone, PartialEq, Eq)]
pub struct Invocation {
    /// The executable.
    pub program: String,
    /// Its arguments.
    pub args: Vec<String>,
    /// Env vars to set.
    pub env: Vec<(String, String)>,
    /// Env vars to remove from the inherited environment.
    pub env_remove: Vec<String>,
    /// The working directory; the caller's when `None`.
    pub cwd: Option<PathBuf>,
    /// Lines to write to stdin. The process gets no stdin when `None`.
    pub stdin: Option<String>,
    /// With `stdin`: the text marking the stdout line that answers it. The
    /// process is stopped once that line arrives.
    pub answer_marker: Option<String>,
}

impl Invocation {
    pub(crate) fn new(cli: Cli) -> Self {
        Self {
            program: cli.program().to_string(),
            args: Vec::new(),
            env: Vec::new(),
            env_remove: Vec::new(),
            cwd: None,
            stdin: None,
            answer_marker: None,
        }
    }

    fn arg(&mut self, arg: impl Into<String>) -> &mut Self {
        self.args.push(arg.into());
        self
    }

    fn set_env(&mut self, key: &str, value: impl Into<String>) -> &mut Self {
        self.env.push((key.to_string(), value.into()));
        self
    }

    /// The env value set for `key`, if any.
    pub fn env_value(&self, key: &str) -> Option<&str> {
        self.env
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

impl fmt::Debug for Invocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let env: Vec<_> = self.env.iter().map(|(k, _)| format!("{k}=…")).collect();
        f.debug_struct("Invocation")
            .field("program", &self.program)
            .field("args", &self.args)
            .field("env", &env)
            .field("env_remove", &self.env_remove)
            .field("cwd", &self.cwd)
            .field("stdin", &self.stdin)
            .field("answer_marker", &self.answer_marker)
            .finish()
    }
}

impl ProviderConfig {
    /// A configuration with no model or base URL override.
    pub fn new(provider: Provider, auth: Auth) -> Self {
        Self {
            provider,
            auth,
            model: None,
            effort: None,
            base_url: None,
            analytics_token: None,
            alternatives: Vec::new(),
        }
    }

    /// Every model a worker may be given: `model` and `effort` first, then
    /// the alternatives. `vibe` takes its model from its own config, so it
    /// has only the one.
    pub fn options(&self) -> Vec<ModelOption> {
        let default = ModelOption {
            model: self.model.clone(),
            effort: self.effort.clone(),
            description: None,
        };
        if self.provider.cli() == Cli::Vibe {
            return vec![default];
        }
        std::iter::once(default)
            .chain(self.alternatives.iter().cloned())
            .collect()
    }

    /// This configuration set to run `option` alone.
    pub fn with_option(&self, option: &ModelOption) -> Self {
        Self {
            model: option.model.clone(),
            effort: option.effort.clone(),
            alternatives: Vec::new(),
            ..self.clone()
        }
    }

    /// Checks the configuration can be run.
    ///
    /// ```
    /// use agent_backend::{Auth, ConfigError, Provider, ProviderConfig};
    ///
    /// let config = ProviderConfig::new(Provider::Deepseek, Auth::Plan);
    /// assert_eq!(config.validate(), Err(ConfigError::PlanUnsupported(Provider::Deepseek)));
    /// ```
    pub fn validate(&self) -> Result<(), ConfigError> {
        let provider = self.provider;
        match &self.auth {
            Auth::Plan if !provider.supports_plan() => {
                return Err(ConfigError::PlanUnsupported(provider));
            }
            Auth::ApiKey { key } if key.is_empty() && provider != Provider::OpenAiCompatible => {
                return Err(ConfigError::MissingKey(provider));
            }
            _ => {}
        }
        let efforts =
            std::iter::once(&self.effort).chain(self.alternatives.iter().map(|a| &a.effort));
        for effort in efforts {
            if let Some(effort) = non_empty(effort)
                && !provider.cli().effort_levels().contains(&effort)
            {
                return Err(ConfigError::UnsupportedEffort(provider, effort.to_string()));
            }
        }
        if matches!(provider, Provider::OpenAiCompatible | Provider::Cloudflare)
            && non_empty(&self.base_url).is_none()
        {
            return Err(ConfigError::MissingBaseUrl(provider));
        }
        if provider == Provider::OpenAiCompatible && non_empty(&self.model).is_none() {
            return Err(ConfigError::MissingModel);
        }
        Ok(())
    }

    /// The headless CLI run that carries out `job`.
    pub fn invocation(&self, job: &Job<'_>) -> Result<Invocation, ConfigError> {
        self.validate()?;
        let cli = self.provider.cli();
        let mut run = Invocation::new(cli);
        run.cwd = Some(job.workdir.to_path_buf());
        // Inherited keys would override the configured auth either way.
        run.env_remove = cli.key_env_vars().iter().map(|v| v.to_string()).collect();
        match cli {
            Cli::Claude => self.claude(&mut run, job),
            Cli::Codex => self.codex(&mut run, job),
            Cli::Antigravity => self.antigravity(&mut run, job),
            Cli::Vibe => self.vibe(&mut run, job),
        }
        // The caller removes these after setting `env`, which would undo any
        // that the provider set itself, such as Deepseek's auth token.
        let set: Vec<_> = run.env.iter().map(|(k, _)| k.clone()).collect();
        run.env_remove.retain(|v| !set.contains(v));
        Ok(run)
    }

    pub(crate) fn key(&self) -> Option<&str> {
        match &self.auth {
            Auth::Plan => None,
            Auth::ApiKey { key } => Some(key),
        }
    }

    fn claude(&self, run: &mut Invocation, job: &Job<'_>) {
        // The prompt goes on stdin: one argument may not exceed 128 KiB on
        // Linux, and a work item with its spec can.
        run.stdin = Some(job.prompt.to_string());
        run.arg("-p")
            .arg("--output-format")
            .arg("stream-json")
            .arg("--verbose")
            .arg("--permission-mode")
            .arg("acceptEdits")
            .arg("--allowedTools")
            .arg("Bash,WebFetch,WebSearch");
        for dir in job.extra_dirs {
            run.arg("--add-dir").arg(dir.display().to_string());
        }
        if let Some(effort) = non_empty(&self.effort) {
            run.arg("--effort").arg(effort);
        }
        let hosted = match self.provider {
            Provider::Deepseek => Some((DEEPSEEK_BASE_URL, DEEPSEEK_MODEL)),
            Provider::OpenRouter => Some((OPENROUTER_BASE_URL, OPENROUTER_MODEL)),
            _ => None,
        };
        if let Some((default_url, default_model)) = hosted {
            let model = non_empty(&self.model).unwrap_or(default_model);
            let base_url = non_empty(&self.base_url).unwrap_or(default_url);
            run.set_env("ANTHROPIC_BASE_URL", base_url)
                .set_env("ANTHROPIC_AUTH_TOKEN", self.key().unwrap_or_default())
                // These endpoints ask for the API key to be set but empty;
                // unset, `claude` could fall back to a stored login.
                .set_env("ANTHROPIC_API_KEY", "")
                .set_env("ANTHROPIC_MODEL", model)
                .set_env("ANTHROPIC_SMALL_FAST_MODEL", model);
            return;
        }
        if let Some(model) = non_empty(&self.model) {
            run.arg("--model").arg(model);
        }
        if let Some(key) = self.key() {
            run.set_env("ANTHROPIC_API_KEY", key);
        }
    }

    fn codex(&self, run: &mut Invocation, job: &Job<'_>) {
        run.arg("exec")
            .arg("--json")
            .arg("--cd")
            .arg(job.workdir.display().to_string())
            .arg("--sandbox")
            .arg("workspace-write")
            // Agents fetch card text and build dependencies over the network.
            .arg("-c")
            .arg("sandbox_workspace_write.network_access=true");
        for dir in job.extra_dirs.iter().chain(job.writable_dirs) {
            run.arg("--add-dir").arg(dir.display().to_string());
        }
        let default_model = (self.provider == Provider::Cloudflare).then_some(CLOUDFLARE_MODEL);
        if let Some(model) = non_empty(&self.model).or(default_model) {
            run.arg("--model").arg(model);
        }
        if let Some(effort) = non_empty(&self.effort) {
            run.arg("-c")
                .arg(format!("model_reasoning_effort={}", toml_string(effort)));
        }
        if matches!(
            self.provider,
            Provider::OpenAiCompatible | Provider::Cloudflare
        ) {
            let base_url = non_empty(&self.base_url).unwrap_or_default();
            run.arg("-c")
                .arg("model_provider=\"aiwc\"")
                .arg("-c")
                .arg(format!(
                    "model_providers.aiwc.name={}",
                    toml_string(self.provider.label())
                ))
                .arg("-c")
                .arg(format!(
                    "model_providers.aiwc.base_url={}",
                    toml_string(base_url)
                ))
                .arg("-c")
                .arg("model_providers.aiwc.wire_api=\"responses\"");
            if let Some(key) = self.key().filter(|k| !k.is_empty()) {
                run.arg("-c").arg(format!(
                    "model_providers.aiwc.env_key=\"{COMPATIBLE_KEY_ENV}\""
                ));
                run.set_env(COMPATIBLE_KEY_ENV, key);
            }
        } else if let Some(key) = self.key() {
            run.set_env("CODEX_API_KEY", key);
        }
        run.stdin = Some(job.prompt.to_string());
    }

    /// `agy` works in its current directory, the worktree.
    fn antigravity(&self, run: &mut Invocation, job: &Job<'_>) {
        run.arg("-p")
            .arg(job.prompt)
            .arg("--output-format")
            .arg("stream-json")
            // Headless runs cannot ask; unapproved tools such as shell
            // commands would be silently refused. Same as Gemini's yolo.
            .arg("--dangerously-skip-permissions");
        for dir in job.extra_dirs {
            run.arg("--add-dir").arg(dir.display().to_string());
        }
        if let Some(model) = non_empty(&self.model) {
            run.arg("--model").arg(model);
        }
        if let Some(effort) = non_empty(&self.effort) {
            run.arg("--effort").arg(effort);
        }
        if let Some(key) = self.key() {
            run.set_env("GEMINI_API_KEY", key);
        }
    }
}

impl ProviderConfig {
    /// `vibe` takes its model from its own `config.toml` and has no flag or
    /// env var for it, so the configured model is not passed.
    fn vibe(&self, run: &mut Invocation, job: &Job<'_>) {
        run.stdin = Some(job.prompt.to_string());
        run.arg("-p")
            .arg("--output")
            .arg("streaming")
            // Headless runs cannot ask, and cannot answer the folder trust prompt.
            .arg("--auto-approve")
            .arg("--trust");
        for dir in job.extra_dirs {
            run.arg("--add-dir").arg(dir.display().to_string());
        }
        if let Some(key) = self.key() {
            run.set_env("MISTRAL_API_KEY", key);
        }
    }
}

fn non_empty(value: &Option<String>) -> Option<&str> {
    value.as_deref().map(str::trim).filter(|v| !v.is_empty())
}

/// Quotes `value` as a TOML basic string for a `codex -c` override.
fn toml_string(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

#[cfg(test)]
mod tests {
    // Test code asserts rather than propagating; see Rust code review.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]

    use super::*;

    fn job() -> Job<'static> {
        Job {
            prompt: "do it",
            workdir: Path::new("/wt"),
            extra_dirs: &[],
            writable_dirs: &[],
        }
    }

    #[test]
    fn auth_debug_redacts_key() {
        let auth = Auth::ApiKey {
            key: "sk-secret".into(),
        };
        assert!(!format!("{auth:?}").contains("sk-secret"));
    }

    #[test]
    fn invocation_debug_redacts_env_values() {
        let config = ProviderConfig::new(
            Provider::Anthropic,
            Auth::ApiKey {
                key: "sk-secret".into(),
            },
        );
        let run = config.invocation(&job()).unwrap();
        assert!(!format!("{run:?}").contains("sk-secret"));
    }

    #[test]
    fn empty_key_rejected_except_for_compatible() {
        let empty = Auth::ApiKey { key: String::new() };
        let config = ProviderConfig::new(Provider::OpenAi, empty.clone());
        assert_eq!(
            config.validate(),
            Err(ConfigError::MissingKey(Provider::OpenAi))
        );

        let mut compatible = ProviderConfig::new(Provider::OpenAiCompatible, empty);
        compatible.base_url = Some("http://localhost:11434/v1".into());
        compatible.model = Some("qwen".into());
        assert_eq!(compatible.validate(), Ok(()));
    }

    #[test]
    fn compatible_needs_base_url_and_model() {
        let key = Auth::ApiKey { key: "k".into() };
        let mut config = ProviderConfig::new(Provider::OpenAiCompatible, key);
        assert_eq!(
            config.validate(),
            Err(ConfigError::MissingBaseUrl(Provider::OpenAiCompatible))
        );
        config.base_url = Some("http://x".into());
        assert_eq!(config.validate(), Err(ConfigError::MissingModel));
    }

    #[test]
    fn effort_must_be_one_of_the_cli_levels() {
        let mut config = ProviderConfig::new(Provider::OpenAi, Auth::Plan);
        config.effort = Some("max".into());
        assert_eq!(
            config.validate(),
            Err(ConfigError::UnsupportedEffort(
                Provider::OpenAi,
                "max".into()
            ))
        );
        config.provider = Provider::Anthropic;
        assert_eq!(config.validate(), Ok(()));
        config.effort = Some("minimal".into());
        assert!(config.validate().is_err());
    }

    #[test]
    fn options_start_with_the_default_and_run_alone() {
        let mut config = ProviderConfig::new(Provider::Anthropic, Auth::Plan);
        config.model = Some("sonnet".into());
        let big = ModelOption {
            model: Some("opus".into()),
            effort: Some("high".into()),
            description: Some("hard changes".into()),
        };
        config.alternatives.push(big.clone());
        let options = config.options();
        assert_eq!(options.len(), 2);
        assert_eq!(options[0].label(), "sonnet");
        assert_eq!(options[1].label(), "opus, high effort");

        let run = config.with_option(&big).invocation(&job()).unwrap();
        assert!(run.args.windows(2).any(|w| w == ["--model", "opus"]));
        assert!(config.with_option(&big).alternatives.is_empty());
    }

    #[test]
    fn alternative_efforts_are_checked_and_vibe_has_one_option() {
        let mut config = ProviderConfig::new(Provider::OpenAi, Auth::Plan);
        config.alternatives.push(ModelOption {
            model: None,
            effort: Some("max".into()),
            description: None,
        });
        assert!(config.validate().is_err());
        let mut vibe = ProviderConfig::new(Provider::Mistral, Auth::Plan);
        vibe.alternatives.push(ModelOption {
            model: Some("x".into()),
            effort: None,
            description: None,
        });
        assert_eq!(vibe.options().len(), 1);
    }

    #[test]
    fn mistral_runs_vibe_headless() {
        let mut config = ProviderConfig::new(Provider::Mistral, Auth::Plan);
        let plan = config.invocation(&job()).unwrap();
        assert_eq!(plan.program, "vibe");
        assert_eq!(
            plan.args,
            ["-p", "--output", "streaming", "--auto-approve", "--trust"]
        );
        assert_eq!(plan.stdin.as_deref(), Some("do it"));
        assert!(plan.env.is_empty());
        assert_eq!(plan.env_remove, ["MISTRAL_API_KEY"]);

        config.auth = Auth::ApiKey { key: "k".into() };
        let run = config.invocation(&job()).unwrap();
        assert_eq!(run.env_value("MISTRAL_API_KEY"), Some("k"));
    }

    #[test]
    fn vibe_accepts_no_effort() {
        let mut config = ProviderConfig::new(Provider::Mistral, Auth::Plan);
        config.effort = Some("high".into());
        assert!(config.validate().is_err());
    }

    #[test]
    fn toml_string_escapes_quotes() {
        assert_eq!(toml_string(r#"a"b\c"#), r#""a\"b\\c""#);
    }
}

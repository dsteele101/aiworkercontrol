//! Providers, their settings, and the CLI invocation each one needs.

use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Deepseek's Anthropic-compatible endpoint, used when no base URL is set.
const DEEPSEEK_BASE_URL: &str = "https://api.deepseek.com/anthropic";
/// The model used for Deepseek when none is set.
const DEEPSEEK_MODEL: &str = "deepseek-chat";
/// Env var carrying an OpenAI-compatible endpoint's key into `codex`.
const COMPATIBLE_KEY_ENV: &str = "AIWC_PROVIDER_API_KEY";

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
    /// Any endpoint speaking the OpenAI Responses API.
    OpenAiCompatible,
}

impl Provider {
    /// Every provider, in display order.
    pub const ALL: [Provider; 5] = [
        Provider::Anthropic,
        Provider::Gemini,
        Provider::OpenAi,
        Provider::Deepseek,
        Provider::OpenAiCompatible,
    ];

    /// A human-readable name.
    pub fn label(self) -> &'static str {
        match self {
            Provider::Anthropic => "Anthropic (Claude)",
            Provider::Gemini => "Gemini",
            Provider::OpenAi => "OpenAI",
            Provider::Deepseek => "Deepseek",
            Provider::OpenAiCompatible => "OpenAI-compatible",
        }
    }

    /// The CLI that runs this provider's agents.
    pub fn cli(self) -> Cli {
        match self {
            Provider::Anthropic | Provider::Deepseek => Cli::Claude,
            Provider::OpenAi | Provider::OpenAiCompatible => Cli::Codex,
            Provider::Gemini => Cli::Gemini,
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
            Provider::Anthropic | Provider::OpenAi | Provider::Gemini
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
    /// Gemini CLI, `gemini`.
    Gemini,
}

impl Cli {
    /// The executable name.
    pub fn program(self) -> &'static str {
        match self {
            Cli::Claude => "claude",
            Cli::Codex => "codex",
            Cli::Gemini => "gemini",
        }
    }

    /// Env vars that would make the CLI use an API key instead of the plan
    /// login, so a plan run must not inherit them.
    fn key_env_vars(self) -> &'static [&'static str] {
        match self {
            Cli::Claude => &["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN"],
            Cli::Codex => &["CODEX_API_KEY", "OPENAI_API_KEY"],
            Cli::Gemini => &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
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

/// Settings for one provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderConfig {
    /// Which provider this configures.
    pub provider: Provider,
    /// How it authenticates.
    pub auth: Auth,
    /// The model to request; the CLI's default when `None`.
    #[serde(default)]
    pub model: Option<String>,
    /// The endpoint, for Deepseek (optional) and OpenAI-compatible (required).
    #[serde(default)]
    pub base_url: Option<String>,
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
    #[error("OpenAI-compatible provider needs a base URL")]
    MissingBaseUrl,
    /// An OpenAI-compatible endpoint needs a model name.
    #[error("OpenAI-compatible provider needs a model")]
    MissingModel,
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
}

impl Invocation {
    fn new(cli: Cli) -> Self {
        Self {
            program: cli.program().to_string(),
            args: Vec::new(),
            env: Vec::new(),
            env_remove: Vec::new(),
            cwd: None,
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
            base_url: None,
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
        if provider == Provider::OpenAiCompatible {
            if non_empty(&self.base_url).is_none() {
                return Err(ConfigError::MissingBaseUrl);
            }
            if non_empty(&self.model).is_none() {
                return Err(ConfigError::MissingModel);
            }
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
            Cli::Gemini => self.gemini(&mut run, job),
        }
        Ok(run)
    }

    fn key(&self) -> Option<&str> {
        match &self.auth {
            Auth::Plan => None,
            Auth::ApiKey { key } => Some(key),
        }
    }

    fn claude(&self, run: &mut Invocation, job: &Job<'_>) {
        run.arg("-p")
            .arg(job.prompt)
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
        if self.provider == Provider::Deepseek {
            let model = non_empty(&self.model).unwrap_or(DEEPSEEK_MODEL);
            let base_url = non_empty(&self.base_url).unwrap_or(DEEPSEEK_BASE_URL);
            run.set_env("ANTHROPIC_BASE_URL", base_url)
                .set_env("ANTHROPIC_AUTH_TOKEN", self.key().unwrap_or_default())
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
        for dir in job.extra_dirs {
            run.arg("--add-dir").arg(dir.display().to_string());
        }
        if let Some(model) = non_empty(&self.model) {
            run.arg("--model").arg(model);
        }
        if self.provider == Provider::OpenAiCompatible {
            let base_url = non_empty(&self.base_url).unwrap_or_default();
            run.arg("-c")
                .arg("model_provider=\"aiwc\"")
                .arg("-c")
                .arg("model_providers.aiwc.name=\"OpenAI-compatible\"")
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
        run.arg(job.prompt);
    }

    fn gemini(&self, run: &mut Invocation, job: &Job<'_>) {
        run.arg("-p")
            .arg(job.prompt)
            .arg("--output-format")
            .arg("stream-json")
            .arg("--approval-mode")
            .arg("yolo")
            // Worktrees are new directories the user has never trusted.
            .arg("--skip-trust");
        for dir in job.extra_dirs {
            run.arg("--include-directories")
                .arg(dir.display().to_string());
        }
        if let Some(model) = non_empty(&self.model) {
            run.arg("--model").arg(model);
        }
        if let Some(key) = self.key() {
            run.set_env("GEMINI_API_KEY", key);
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
        assert_eq!(config.validate(), Err(ConfigError::MissingBaseUrl));
        config.base_url = Some("http://x".into());
        assert_eq!(config.validate(), Err(ConfigError::MissingModel));
    }

    #[test]
    fn toml_string_escapes_quotes() {
        assert_eq!(toml_string(r#"a"b\c"#), r#""a\"b\\c""#);
    }
}

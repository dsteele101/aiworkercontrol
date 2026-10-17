//! Settings persisted in `aiworkercontrol.toml`.

use std::fs;
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use agent_backend::{Auth, ConfigError, Provider, ProviderConfig};
use serde::{Deserialize, Serialize};
use worker_pool::{PoolConfig, Project, UsageLimits};

use crate::ServerError;

/// The settings file's name, inside the root directory.
pub const SETTINGS_FILE: &str = "aiworkercontrol.toml";

/// Everything the user configures in the UI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    /// How many workers to run.
    pub workers: usize,
    /// The provider workers use.
    pub active: Provider,
    /// Per-provider settings; one entry for each provider.
    pub providers: Vec<ProviderSettings>,
    /// The known projects. Relative paths are relative to the root.
    #[serde(default)]
    pub projects: Vec<Project>,
    /// Where to find the agent CLIs; `PATH` when unset.
    #[serde(default)]
    pub cli_dir: Option<PathBuf>,
    /// Agent runs an item may use before it goes on hold.
    #[serde(default = "default_max_turns")]
    pub max_turns: u32,
    /// When plan usage warns, stops runs, and fails over.
    #[serde(default)]
    pub limits: UsageLimits,
    /// Providers to fail over to at the stop level, in order.
    #[serde(default)]
    pub fallbacks: Vec<Provider>,
}

/// The default for [`Settings::max_turns`].
pub const DEFAULT_MAX_TURNS: u32 = 3;

fn default_max_turns() -> u32 {
    DEFAULT_MAX_TURNS
}

/// One provider's settings. Unlike [`ProviderConfig`], the API key is kept
/// while plan login is selected, so switching back does not lose it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderSettings {
    /// The provider.
    pub provider: Provider,
    /// Use the API key rather than the plan login.
    #[serde(default)]
    pub use_api_key: bool,
    /// The API key.
    #[serde(default)]
    pub api_key: String,
    /// The model; the CLI's default when empty.
    #[serde(default)]
    pub model: String,
    /// The effort level; the CLI's default when empty.
    #[serde(default)]
    pub effort: String,
    /// The endpoint, for Deepseek and OpenAI-compatible.
    #[serde(default)]
    pub base_url: String,
}

impl std::fmt::Debug for ProviderSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderSettings")
            .field("provider", &self.provider)
            .field("use_api_key", &self.use_api_key)
            .field(
                "api_key",
                &if self.api_key.is_empty() { "" } else { "<set>" },
            )
            .field("model", &self.model)
            .field("effort", &self.effort)
            .field("base_url", &self.base_url)
            .finish()
    }
}

impl ProviderSettings {
    fn new(provider: Provider) -> Self {
        Self {
            provider,
            use_api_key: !provider.supports_plan(),
            api_key: String::new(),
            model: String::new(),
            effort: String::new(),
            base_url: String::new(),
        }
    }

    /// The runnable configuration these settings describe.
    pub fn config(&self) -> ProviderConfig {
        let auth = if self.use_api_key || !self.provider.supports_plan() {
            Auth::ApiKey {
                key: self.api_key.trim().to_string(),
            }
        } else {
            Auth::Plan
        };
        let opt = |s: &str| Some(s.trim().to_string()).filter(|s| !s.is_empty());
        ProviderConfig {
            provider: self.provider,
            auth,
            model: opt(&self.model),
            effort: opt(&self.effort),
            base_url: opt(&self.base_url),
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            workers: 1,
            active: Provider::Anthropic,
            providers: Provider::ALL.map(ProviderSettings::new).to_vec(),
            projects: Vec::new(),
            cli_dir: None,
            max_turns: DEFAULT_MAX_TURNS,
            limits: UsageLimits::default(),
            fallbacks: Vec::new(),
        }
    }
}

impl Settings {
    /// Loads the settings from `root`, or the defaults if there is no file.
    pub fn load(root: &Path) -> Result<Self, ServerError> {
        let path = root.join(SETTINGS_FILE);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(source) => return Err(ServerError::Io { path, source }),
        };
        let mut settings: Self =
            toml::from_str(&text).map_err(|source| ServerError::Parse { path, source })?;
        for provider in Provider::ALL {
            if settings.provider(provider).is_none() {
                settings.providers.push(ProviderSettings::new(provider));
            }
        }
        Ok(settings)
    }

    /// Writes the settings to `root`, readable by the owner only since they
    /// hold API keys.
    pub fn save(&self, root: &Path) -> Result<(), ServerError> {
        let path = root.join(SETTINGS_FILE);
        let tmp = root.join(format!(".{SETTINGS_FILE}.tmp"));
        let text = toml::to_string_pretty(self).map_err(ServerError::Serialize)?;
        let io = |path: &Path| {
            let path = path.to_path_buf();
            move |source| ServerError::Io { path, source }
        };
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(io(&tmp))?;
        file.write_all(text.as_bytes()).map_err(io(&tmp))?;
        file.sync_all().map_err(io(&tmp))?;
        fs::rename(&tmp, &path).map_err(io(&path))
    }

    /// The settings of `provider`.
    pub fn provider(&self, provider: Provider) -> Option<&ProviderSettings> {
        self.providers.iter().find(|p| p.provider == provider)
    }

    /// The settings of `provider`, for editing.
    pub fn provider_mut(&mut self, provider: Provider) -> &mut ProviderSettings {
        let index = match self.providers.iter().position(|p| p.provider == provider) {
            Some(index) => index,
            None => {
                self.providers.push(ProviderSettings::new(provider));
                self.providers.len() - 1
            }
        };
        &mut self.providers[index]
    }

    /// The active provider's configuration, checked to be runnable.
    pub fn active_config(&self) -> Result<ProviderConfig, ConfigError> {
        let config = self
            .provider(self.active)
            .cloned()
            .unwrap_or_else(|| ProviderSettings::new(self.active))
            .config();
        config.validate()?;
        Ok(config)
    }

    /// The pool configuration, with project paths resolved against `root`.
    /// An unrunnable active provider is passed through as is; check
    /// [`Settings::active_config`] before starting.
    /// Fallbacks that are the active provider, repeated, or not runnable
    /// (no API key, say) are left out.
    pub fn pool_config(&self, root: &Path) -> PoolConfig {
        let config = |provider| {
            self.provider(provider)
                .cloned()
                .unwrap_or_else(|| ProviderSettings::new(provider))
                .config()
        };
        let provider = config(self.active);
        let mut fallbacks: Vec<ProviderConfig> = Vec::new();
        for &p in &self.fallbacks {
            let fallback = config(p);
            if p != self.active
                && !fallbacks.iter().any(|f| f.provider == p)
                && fallback.validate().is_ok()
            {
                fallbacks.push(fallback);
            }
        }
        let projects = self
            .projects
            .iter()
            .map(|p| Project {
                spec_dir: resolve(root, &p.spec_dir),
                repo: resolve(root, &p.repo),
                ..p.clone()
            })
            .collect();
        PoolConfig {
            workers: self.workers,
            provider,
            projects,
            cli_dir: self.cli_dir.clone(),
            max_turns: self.max_turns.max(1),
            limits: self.limits.clone(),
            fallbacks,
        }
    }
}

/// `path` against `root`, with `..` and symlinks resolved when it exists,
/// so `../phase` reads as `/home/me/dev/phase` in results and logs.
fn resolve(root: &Path, path: &Path) -> PathBuf {
    let joined = root.join(path);
    fs::canonicalize(&joined).unwrap_or(joined)
}

#[cfg(test)]
mod tests {
    // Test code asserts rather than propagating; see Rust code review.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]

    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn missing_file_gives_defaults_for_every_provider() {
        let dir = tempfile::tempdir().unwrap();
        let settings = Settings::load(dir.path()).unwrap();
        assert_eq!(settings.providers.len(), Provider::ALL.len());
        assert!(!settings.provider(Provider::Anthropic).unwrap().use_api_key);
        assert!(settings.provider(Provider::Deepseek).unwrap().use_api_key);
    }

    #[test]
    fn round_trips_and_is_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let mut settings = Settings {
            workers: 3,
            ..Settings::default()
        };
        settings.provider_mut(Provider::OpenAi).api_key = "sk-1".into();
        settings
            .projects
            .push(Project::new("phase", "spec/phase", "../phase"));
        settings.save(dir.path()).unwrap();

        let mode = fs::metadata(dir.path().join(SETTINGS_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(Settings::load(dir.path()).unwrap(), settings);
    }

    #[test]
    fn plan_mode_keeps_key_but_does_not_use_it() {
        let mut p = ProviderSettings::new(Provider::Gemini);
        p.api_key = "k".into();
        assert_eq!(p.config().auth, Auth::Plan);
        p.use_api_key = true;
        assert_eq!(p.config().auth, Auth::ApiKey { key: "k".into() });
    }

    #[test]
    fn pool_config_resolves_relative_paths() {
        let mut settings = Settings::default();
        settings
            .projects
            .push(Project::new("phase", "spec/phase", "/abs/phase"));
        let config = settings.pool_config(Path::new("/root"));
        assert_eq!(config.projects[0].spec_dir, Path::new("/root/spec/phase"));
        assert_eq!(config.projects[0].repo, Path::new("/abs/phase"));
    }

    #[test]
    fn pool_config_keeps_only_runnable_distinct_fallbacks() {
        let mut settings = Settings::default();
        settings.provider_mut(Provider::OpenAi).use_api_key = true; // no key
        settings.fallbacks = vec![
            Provider::Anthropic, // the active one
            Provider::Gemini,
            Provider::OpenAi,
            Provider::Gemini,
        ];
        let fallbacks: Vec<_> = settings
            .pool_config(Path::new("/root"))
            .fallbacks
            .iter()
            .map(|f| f.provider)
            .collect();
        assert_eq!(fallbacks, vec![Provider::Gemini]);
    }

    #[test]
    fn debug_hides_key() {
        let mut p = ProviderSettings::new(Provider::OpenAi);
        p.api_key = "sk-secret".into();
        assert!(!format!("{p:?}").contains("sk-secret"));
    }

    #[test]
    fn pool_config_normalizes_existing_paths() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("control");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(dir.path().join("phase")).unwrap();
        let mut settings = Settings::default();
        settings
            .projects
            .push(Project::new("phase", "spec/phase", "../phase"));
        let config = settings.pool_config(&root);
        assert_eq!(
            config.projects[0].repo,
            fs::canonicalize(dir.path().join("phase")).unwrap()
        );
    }
}

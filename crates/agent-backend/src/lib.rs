//! Headless runs of the official coding-agent CLIs.
//!
//! Each supported AI system is driven through its vendor's own CLI, so a plan
//! login (Claude, ChatGPT, Google) works exactly as it does interactively, and
//! the CLI reads the target repo's own agent instructions (`CLAUDE.md`,
//! `AGENTS.md`, `GEMINI.md`) by itself:
//!
//! | Provider | CLI | Plan login | API key |
//! |----------|-----|------------|---------|
//! | Anthropic | `claude` | yes | `ANTHROPIC_API_KEY` |
//! | Deepseek | `claude` | no | Deepseek's Anthropic-compatible endpoint |
//! | OpenRouter | `claude` | no | OpenRouter's Anthropic-compatible endpoint |
//! | OpenAI | `codex` | yes | `CODEX_API_KEY` |
//! | OpenAI-compatible | `codex` | no | custom provider (Responses API) |
//! | Gemini | `agy` (Antigravity) | yes | `GEMINI_API_KEY` |
//! | Mistral | `vibe` | yes | `MISTRAL_API_KEY` |
//!
//! This crate only describes processes ([`Invocation`]) and parses their
//! output ([`parse_line`]); running them is the caller's job.
//!
//! ```
//! use agent_backend::{Auth, Job, Provider, ProviderConfig};
//! use std::path::Path;
//!
//! let config = ProviderConfig::new(Provider::Anthropic, Auth::Plan);
//! let job = Job { prompt: "Fix the bug", workdir: Path::new("/wt"), extra_dirs: &[], writable_dirs: &[] };
//! let run = config.invocation(&job)?;
//! assert_eq!(run.program, "claude");
//! assert!(run.env_remove.contains(&"ANTHROPIC_API_KEY".to_string()));
//! # Ok::<(), agent_backend::ConfigError>(())
//! ```

mod auth;
mod events;
mod metrics;
mod provider;
mod usage;

pub use auth::{AuthStatus, login_invocation, parse_status, status_invocation};
pub use events::{AgentEvent, parse_line};
pub use metrics::RunMetrics;
pub use provider::{Auth, Cli, ConfigError, Invocation, Job, Provider, ProviderConfig};
pub use usage::{PlanUsage, UsageWindow};

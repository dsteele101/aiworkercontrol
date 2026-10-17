//! Plan logins: starting one, and checking whether one is in place.

use std::path::Path;

use crate::{Cli, Invocation};

/// Whether a CLI has a stored plan login.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthStatus {
    /// Logged in.
    LoggedIn,
    /// Not logged in.
    LoggedOut,
    /// The status could not be determined.
    Unknown,
}

/// The command that starts `cli`'s own plan login, printing the sign-in URL.
///
/// `None` for Gemini: its CLI has no login subcommand, and signs in with
/// Google the first time `gemini` runs interactively in a terminal.
///
/// ```
/// use agent_backend::{Cli, login_invocation};
///
/// let run = login_invocation(Cli::Claude).expect("claude has a login command");
/// assert_eq!(run.args, ["auth", "login", "--claudeai"]);
/// assert!(login_invocation(Cli::Gemini).is_none());
/// ```
pub fn login_invocation(cli: Cli) -> Option<Invocation> {
    let args: &[&str] = match cli {
        Cli::Claude => &["auth", "login", "--claudeai"],
        Cli::Codex => &["login"],
        Cli::Gemini => return None,
    };
    Some(simple(cli, args))
}

/// The command that reports `cli`'s login status; read its output with
/// [`parse_status`]. `None` for Gemini; use [`gemini_status`] instead.
pub fn status_invocation(cli: Cli) -> Option<Invocation> {
    let args: &[&str] = match cli {
        Cli::Claude => &["auth", "status", "--json"],
        Cli::Codex => &["login", "status"],
        Cli::Gemini => return None,
    };
    Some(simple(cli, args))
}

/// Reads the output of `cli`'s [`status_invocation`].
///
/// ```
/// use agent_backend::{AuthStatus, Cli, parse_status};
///
/// assert_eq!(parse_status(Cli::Codex, "Not logged in\n"), AuthStatus::LoggedOut);
/// assert_eq!(parse_status(Cli::Claude, r#"{"loggedIn": true}"#), AuthStatus::LoggedIn);
/// ```
pub fn parse_status(cli: Cli, output: &str) -> AuthStatus {
    match cli {
        Cli::Claude => serde_json::from_str::<serde_json::Value>(output)
            .ok()
            .and_then(|v| v.get("loggedIn").and_then(serde_json::Value::as_bool))
            .map_or(AuthStatus::Unknown, logged_in),
        Cli::Codex => {
            let text = output.trim();
            if text.starts_with("Not logged in") {
                AuthStatus::LoggedOut
            } else if text.starts_with("Logged in") {
                AuthStatus::LoggedIn
            } else {
                AuthStatus::Unknown
            }
        }
        Cli::Gemini => AuthStatus::Unknown,
    }
}

/// Gemini's login status, read from the Google credentials the CLI stores
/// under `<home>/.gemini`.
pub fn gemini_status(home: &Path) -> AuthStatus {
    logged_in(home.join(".gemini/oauth_creds.json").is_file())
}

fn logged_in(yes: bool) -> AuthStatus {
    if yes {
        AuthStatus::LoggedIn
    } else {
        AuthStatus::LoggedOut
    }
}

fn simple(cli: Cli, args: &[&str]) -> Invocation {
    Invocation {
        program: cli.program().to_string(),
        args: args.iter().map(|a| a.to_string()).collect(),
        env: Vec::new(),
        env_remove: Vec::new(),
        cwd: None,
    }
}

#[cfg(test)]
mod tests {
    // Test code asserts rather than propagating; see Rust code review.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]

    use super::*;

    /// Captured from `claude auth status --json` (2.1.302) when logged out.
    const CLAUDE_LOGGED_OUT: &str = r#"{
  "loggedIn": false,
  "authMethod": "none",
  "apiProvider": "firstParty"
}"#;

    #[test]
    fn claude_status_from_captured_output() {
        assert_eq!(
            parse_status(Cli::Claude, CLAUDE_LOGGED_OUT),
            AuthStatus::LoggedOut
        );
        assert_eq!(parse_status(Cli::Claude, "garbage"), AuthStatus::Unknown);
    }

    #[test]
    fn codex_status_logged_in() {
        assert_eq!(
            parse_status(Cli::Codex, "Logged in using ChatGPT\n"),
            AuthStatus::LoggedIn
        );
    }

    #[test]
    fn gemini_status_reads_credentials_file() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(gemini_status(home.path()), AuthStatus::LoggedOut);
        std::fs::create_dir_all(home.path().join(".gemini")).unwrap();
        std::fs::write(home.path().join(".gemini/oauth_creds.json"), "{}").unwrap();
        assert_eq!(gemini_status(home.path()), AuthStatus::LoggedIn);
    }
}

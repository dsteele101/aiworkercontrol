//! Plan logins: starting one, and checking whether one is in place.

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
/// `None` for Antigravity: `agy` has no login subcommand, and signs in with
/// Google the first time it runs interactively in a terminal.
///
/// ```
/// use agent_backend::{Cli, login_invocation};
///
/// let run = login_invocation(Cli::Claude).expect("claude has a login command");
/// assert_eq!(run.args, ["auth", "login", "--claudeai"]);
/// assert!(login_invocation(Cli::Antigravity).is_none());
/// ```
pub fn login_invocation(cli: Cli) -> Option<Invocation> {
    let args: &[&str] = match cli {
        Cli::Claude => &["auth", "login", "--claudeai"],
        Cli::Codex => &["login"],
        Cli::Antigravity => return None,
    };
    Some(simple(cli, args))
}

/// The command that reports `cli`'s login status; read its output with
/// [`parse_status`]. `agy` keeps its login in the system keyring and has no
/// status command, so for it this lists the models, which needs a login.
pub fn status_invocation(cli: Cli) -> Invocation {
    let args: &[&str] = match cli {
        Cli::Claude => &["auth", "status", "--json"],
        Cli::Codex => &["login", "status"],
        Cli::Antigravity => &["models"],
    };
    simple(cli, args)
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
        // `agy models` prints one `slug<TAB>name` line per model.
        Cli::Antigravity if output.lines().any(|l| l.contains('\t')) => AuthStatus::LoggedIn,
        Cli::Antigravity => AuthStatus::Unknown,
    }
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
        stdin: None,
        answer_marker: None,
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
    fn antigravity_status_from_model_list() {
        // Captured from `agy models` (1.4.2) while signed in, shortened.
        let listed =
            "Fetching available models...\ngemini-3.8-flash-high\tGemini 3.8 Flash (High)\n";
        assert_eq!(parse_status(Cli::Antigravity, listed), AuthStatus::LoggedIn);
        assert_eq!(
            parse_status(Cli::Antigravity, "Fetching available models...\n"),
            AuthStatus::Unknown
        );
    }
}

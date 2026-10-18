//! Plan logins started from the UI, and login status checks.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, PoisonError};

use agent_backend::{AuthStatus, Cli, Invocation};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{ChildStdin, Command};

/// Most output kept from a login process.
const OUTPUT_LIMIT: usize = 16 * 1024;

/// The login processes started from the UI, one per CLI at most.
#[derive(Debug, Default, Clone)]
pub(crate) struct Logins {
    sessions: Arc<Mutex<HashMap<Cli, Session>>>,
    stdin: Arc<tokio::sync::Mutex<HashMap<Cli, ChildStdin>>>,
}

/// What a login process has printed, and whether it is still running.
#[derive(Debug, Default, Clone)]
pub(crate) struct Session {
    pub output: String,
    pub running: bool,
}

impl Logins {
    /// The session of `cli`, if a login was started.
    pub fn session(&self, cli: Cli) -> Option<Session> {
        self.lock().get(&cli).cloned()
    }

    /// Starts `cli`'s login unless one is already running. Returns false if
    /// the CLI has no login command.
    pub fn start(&self, cli: Cli, cli_dir: Option<&Path>) -> bool {
        let Some(run) = agent_backend::login_invocation(cli) else {
            return false;
        };
        if self.session(cli).is_some_and(|s| s.running) {
            return true;
        }
        let mut child = match command(&run, cli_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
        {
            Ok(child) => child,
            Err(e) => {
                self.lock().insert(
                    cli,
                    Session {
                        output: format!("could not start {}: {e}", run.program),
                        running: false,
                    },
                );
                return true;
            }
        };
        self.lock().insert(
            cli,
            Session {
                output: String::new(),
                running: true,
            },
        );
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let stdin = child.stdin.take();
        let this = self.clone();
        tokio::spawn(async move {
            if let Some(stdin) = stdin {
                this.stdin.lock().await.insert(cli, stdin);
            }
            let out = this.capture(cli, stdout);
            let err = this.capture(cli, stderr);
            let _ = tokio::join!(out, err);
            let status = child.wait().await;
            this.stdin.lock().await.remove(&cli);
            let mut sessions = this.lock();
            if let Some(session) = sessions.get_mut(&cli) {
                session.running = false;
                let end = match status {
                    Ok(s) if s.success() => "\n[login finished]".to_string(),
                    Ok(s) => format!("\n[login exited: {s}]"),
                    Err(e) => format!("\n[login failed: {e}]"),
                };
                session.output.push_str(&end);
            }
        });
        true
    }

    /// Sends a line (such as a pasted authorization code) to `cli`'s
    /// running login.
    pub async fn send(&self, cli: Cli, line: &str) -> bool {
        let mut stdin = self.stdin.lock().await;
        let Some(pipe) = stdin.get_mut(&cli) else {
            return false;
        };
        let sent = pipe
            .write_all(format!("{}\n", line.trim()).as_bytes())
            .await;
        sent.is_ok() && pipe.flush().await.is_ok()
    }

    async fn capture(&self, cli: Cli, stream: Option<impl AsyncRead + Unpin>) {
        let Some(mut stream) = stream else { return };
        let mut buf = [0u8; 1024];
        while let Ok(n) = stream.read(&mut buf).await {
            if n == 0 {
                break;
            }
            let text = String::from_utf8_lossy(&buf[..n]);
            if let Some(session) = self.lock().get_mut(&cli)
                && session.output.len() < OUTPUT_LIMIT
            {
                session.output.push_str(&text);
            }
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<Cli, Session>> {
        self.sessions.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Whether `cli` has a stored plan login.
pub(crate) async fn status(cli: Cli, cli_dir: Option<&Path>) -> AuthStatus {
    let run = agent_backend::status_invocation(cli);
    match command(&run, cli_dir)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
    {
        // `claude` reports on stdout; `codex` 0.163 on stderr.
        Ok(out) => match agent_backend::parse_status(cli, &String::from_utf8_lossy(&out.stdout)) {
            AuthStatus::Unknown => {
                agent_backend::parse_status(cli, &String::from_utf8_lossy(&out.stderr))
            }
            known => known,
        },
        Err(_) => AuthStatus::Unknown,
    }
}

fn command(run: &Invocation, cli_dir: Option<&Path>) -> Command {
    let program = match cli_dir {
        Some(dir) => dir.join(&run.program),
        None => PathBuf::from(&run.program),
    };
    let mut command = Command::new(program);
    command.args(&run.args);
    command
}

#[cfg(test)]
mod tests {
    // Test code asserts rather than propagating; see Rust code review.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]

    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[tokio::test]
    async fn status_read_from_stderr_when_stdout_is_empty() {
        // codex 0.163 prints `login status` to stderr.
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("codex");
        std::fs::write(&fake, "#!/bin/sh\necho 'Not logged in' >&2\nexit 1\n").unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            status(Cli::Codex, Some(dir.path())).await,
            AuthStatus::LoggedOut
        );
    }

    #[tokio::test]
    async fn missing_cli_is_unknown() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            status(Cli::Claude, Some(dir.path())).await,
            AuthStatus::Unknown
        );
    }
}

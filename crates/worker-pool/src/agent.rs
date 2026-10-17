//! Running an agent CLI and following its event stream.

use std::ffi::OsStr;
use std::path::Path;
use std::process::Stdio;

use agent_backend::{AgentEvent, Cli, Invocation};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

use crate::PoolError;

/// Bytes of stderr or setup output kept for a failure reason.
const STDERR_TAIL: usize = 2000;

/// How an agent run ended.
#[derive(Debug)]
pub(crate) struct Outcome {
    /// The process exited with status 0.
    pub exit_ok: bool,
    /// The agent's own verdict, if it reported one.
    pub finished: Option<(bool, Option<String>)>,
    /// The last text the agent wrote.
    pub last_text: Option<String>,
    /// The end of what the process wrote to stderr.
    pub stderr_tail: String,
}

impl Outcome {
    /// Success: a clean exit and no failure reported by the agent.
    pub fn succeeded(&self) -> bool {
        self.exit_ok && self.finished.as_ref().is_none_or(|(ok, _)| *ok)
    }

    /// The agent's summary of its work: its final message, else its last text.
    pub fn summary(&self) -> Option<&str> {
        self.finished
            .as_ref()
            .and_then(|(_, s)| s.as_deref())
            .or(self.last_text.as_deref())
    }
}

/// Runs a project's setup command with `sh -c` in `worktree`, with `env`
/// set, writing its output to `log`. Fails if it exits unsuccessfully.
pub(crate) async fn setup(
    script: &str,
    worktree: &Path,
    env: &[(&str, &OsStr)],
    log: &Path,
) -> Result<(), PoolError> {
    let output = Command::new("sh")
        .arg("-c")
        .arg(script)
        .current_dir(worktree)
        .envs(env.iter().copied())
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|source| PoolError::Spawn {
            program: "sh".into(),
            source,
        })?;
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    let entry = format!(
        "--- setup: {script} ---\n{text}\n--- setup {} ---\n",
        output.status
    );
    tokio::fs::write(log, entry)
        .await
        .map_err(PoolError::io("write", log))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(PoolError::Setup {
            status: output.status.to_string(),
            output: tail(&text),
        })
    }
}

/// Runs `run` (with the program looked up in `cli_dir` when given), feeding
/// each parsed event to `on_event` and appending raw output to `log`.
pub(crate) async fn run(
    run: &Invocation,
    cli: Cli,
    cli_dir: Option<&Path>,
    log: &Path,
    mut on_event: impl FnMut(AgentEvent),
) -> Result<Outcome, PoolError> {
    let program = match cli_dir {
        Some(dir) => dir.join(&run.program),
        None => run.program.clone().into(),
    };
    let mut command = Command::new(&program);
    command
        .args(&run.args)
        .envs(run.env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for key in &run.env_remove {
        command.env_remove(key);
    }
    if let Some(cwd) = &run.cwd {
        command.current_dir(cwd);
    }
    let mut child = command.spawn().map_err(|source| PoolError::Spawn {
        program: program.display().to_string(),
        source,
    })?;

    let mut log_file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .await
        .map_err(PoolError::io("open", log))?;
    let mut stderr = child.stderr.take();
    let stderr_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        if let Some(stderr) = stderr.as_mut() {
            // A read error just ends the capture; stderr is diagnostic only.
            let _ = stderr.read_to_end(&mut buf).await;
        }
        buf
    });

    let mut finished = None;
    let mut last_text = None;
    if let Some(stdout) = child.stdout.take() {
        let mut lines = BufReader::new(stdout).lines();
        while let Some(line) = lines
            .next_line()
            .await
            .map_err(PoolError::io("read", log))?
        {
            log_file
                .write_all(format!("{line}\n").as_bytes())
                .await
                .map_err(PoolError::io("write", log))?;
            for event in agent_backend::parse_line(cli, &line) {
                match &event {
                    AgentEvent::Text(t) => last_text = Some(t.clone()),
                    AgentEvent::Finished { ok, summary } => {
                        finished = Some((*ok, summary.clone()));
                    }
                    AgentEvent::ToolUse { .. } | AgentEvent::Notice(_) => {}
                }
                on_event(event);
            }
        }
    }

    let status = child.wait().await.map_err(PoolError::io("wait", log))?;
    let stderr = stderr_task.await.unwrap_or_default();
    log_file
        .write_all(b"\n--- stderr ---\n")
        .await
        .and(log_file.write_all(&stderr).await)
        .map_err(PoolError::io("write", log))?;

    Ok(Outcome {
        exit_ok: status.success(),
        finished,
        last_text,
        stderr_tail: tail(&String::from_utf8_lossy(&stderr)),
    })
}

/// The last [`STDERR_TAIL`] bytes of `text`, cut on a char boundary.
fn tail(text: &str) -> String {
    let start = text.len().saturating_sub(STDERR_TAIL);
    let start = (start..text.len())
        .find(|i| text.is_char_boundary(*i))
        .unwrap_or(text.len());
    text[start..].trim().to_string()
}

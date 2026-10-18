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
/// How long a usage report may take before it is given up on.
const REPORT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Runs a short command, such as a usage report, and returns its stdout.
/// `Ok(None)` when it fails or takes too long; the caller says why.
pub(crate) async fn report(
    run: &Invocation,
    cli_dir: Option<&Path>,
) -> Result<Option<String>, PoolError> {
    let program = match cli_dir {
        Some(dir) => dir.join(&run.program),
        None => run.program.clone().into(),
    };
    let mut command = Command::new(&program);
    command
        .args(&run.args)
        .envs(run.env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .kill_on_drop(true);
    for key in &run.env_remove {
        command.env_remove(key);
    }
    let output = tokio::time::timeout(REPORT_TIMEOUT, command.output()).await;
    match output {
        Ok(Ok(out)) if out.status.success() => {
            Ok(Some(String::from_utf8_lossy(&out.stdout).into_owned()))
        }
        Ok(Ok(_)) | Err(_) => Ok(None),
        Ok(Err(source)) => Err(PoolError::Spawn {
            program: program.display().to_string(),
            source,
        }),
    }
}

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
        // Its own process group, so the agent's children (background builds,
        // test runs) can be stopped along with it; see `ProcessGroup`.
        .process_group(0)
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
    let group = child.id().map(ProcessGroup);

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

    // Wait for the agent while reading its output. Background processes it
    // started may hold its stdout and stderr open, so the pipes only close
    // once its group is killed, right after the agent itself exits.
    let stdout = child.stdout.take();
    let exited = async {
        let status = child.wait().await;
        drop(group);
        status
    };
    let mut finished = None;
    let mut last_text = None;
    let read = async {
        let Some(stdout) = stdout else {
            return Ok(());
        };
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
                    AgentEvent::ToolUse { .. }
                    | AgentEvent::Notice(_)
                    | AgentEvent::Usage(_)
                    | AgentEvent::Status(_)
                    | AgentEvent::Background(_) => {}
                }
                on_event(event);
            }
        }
        Ok::<(), PoolError>(())
    };
    let (status, read) = tokio::join!(exited, read);
    read?;
    let status = status.map_err(PoolError::io("wait", log))?;
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

/// A running agent's process group, killed whole when dropped: when the run
/// ends, so nothing it left running in the background outlives it, and when
/// the run is abandoned (the app shutting down), so the agent does not run
/// on unsupervised. `kill_on_drop` alone would reach only the agent itself.
#[derive(Debug)]
struct ProcessGroup(u32);

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        // The group id is the agent's pid (`process_group(0)`). Errors are
        // ignored: an empty group means there is nothing left to stop.
        let _ = std::process::Command::new("kill")
            .args(["-KILL", "--", &format!("-{}", self.0)])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

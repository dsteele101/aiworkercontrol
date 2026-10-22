//! Running an agent CLI and following its event stream.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::ffi::OsStr;
use std::future::Future;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

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
    /// Stopped by the watchdog: no output and next to no CPU use for the
    /// stall time, e.g. a background wait that can never end.
    pub stalled: bool,
    /// Stopped because its provider went past the stop level.
    pub halted: bool,
}

impl Outcome {
    /// Success: a clean exit and no failure reported by the agent. A stalled
    /// run succeeds only if its last turn did, i.e. it hung after its work.
    pub fn succeeded(&self) -> bool {
        if self.halted {
            return false;
        }
        if self.stalled {
            return matches!(self.finished, Some((true, _)));
        }
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
    let spawn_error = |source| PoolError::Spawn {
        program: program.display().to_string(),
        source,
    };
    if let (Some(input), Some(marker)) = (&run.stdin, &run.answer_marker) {
        let answered = tokio::time::timeout(REPORT_TIMEOUT, converse(command, input, marker));
        return match answered.await {
            Ok(Ok(out)) => Ok(out),
            Ok(Err(source)) => Err(spawn_error(source)),
            Err(_) => Ok(None),
        };
    }
    let output = tokio::time::timeout(REPORT_TIMEOUT, command.output()).await;
    match output {
        Ok(Ok(out)) if out.status.success() => {
            Ok(Some(String::from_utf8_lossy(&out.stdout).into_owned()))
        }
        Ok(Ok(_)) | Err(_) => Ok(None),
        Ok(Err(source)) => Err(spawn_error(source)),
    }
}

/// Writes `input` to `command`'s stdin and collects its stdout up to the
/// line containing `marker`, then stops it. Closing stdin early would end a
/// server like `codex app-server` before it answered. `None` if the output
/// ends without the marker.
async fn converse(
    mut command: Command,
    input: &str,
    marker: &str,
) -> std::io::Result<Option<String>> {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let (Some(mut stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        return Ok(None);
    };
    stdin.write_all(input.as_bytes()).await?;
    let mut lines = BufReader::new(stdout).lines();
    let mut seen = String::new();
    while let Some(line) = lines.next_line().await? {
        seen.push_str(&line);
        seen.push('\n');
        if line.contains(marker) {
            // Killed on drop with `child`, once the answer is in hand.
            return Ok(Some(seen));
        }
    }
    Ok(None)
}

/// Runs `run`, logging its output to `log` and passing each event to
/// `on_event`. A run that writes nothing for `stall_after` while its whole
/// process tree uses under 1% of a CPU in that time is stopped as stalled.
/// A run is also stopped, whatever it is doing, once `halt` completes.
pub(crate) async fn run(
    run: &Invocation,
    cli: Cli,
    cli_dir: Option<&Path>,
    log: &Path,
    stall_after: Duration,
    halt: impl Future<Output = ()>,
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
    let pid = child.id();
    let group = pid.map(ProcessGroup);

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
    let mut stalled = false;
    let mut halted = false;
    tokio::pin!(halt);
    let read = async {
        let Some(stdout) = stdout else {
            return Ok(());
        };
        let mut lines = BufReader::new(stdout).lines();
        let mut watchdog = Watchdog::new(stall_after);
        let mut tick = tokio::time::interval(watchdog.every);
        loop {
            let line = tokio::select! {
                line = lines.next_line() => line.map_err(PoolError::io("read", log))?,
                () = &mut halt, if !halted => {
                    halted = true;
                    log_file
                        .write_all(b"\n--- stopped: its provider went past the stop level ---\n")
                        .await
                        .map_err(PoolError::io("write", log))?;
                    if let Some(pid) = pid {
                        kill_tree(pid);
                    }
                    continue;
                }
                _ = tick.tick() => {
                    if !stalled && pid.is_some_and(|pid| watchdog.stalled(pid)) {
                        stalled = true;
                        let note = format!(
                            "\n--- stopped: no output and next to no CPU use for {} minutes ---\n",
                            stall_after.as_secs() / 60
                        );
                        log_file
                            .write_all(note.as_bytes())
                            .await
                            .map_err(PoolError::io("write", log))?;
                        if let Some(pid) = pid {
                            kill_tree(pid);
                        }
                    }
                    continue;
                }
            };
            let Some(line) = line else {
                break;
            };
            watchdog.output();
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
                    | AgentEvent::Model(_)
                    | AgentEvent::Metrics(_)
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
        stalled,
        halted,
    })
}

/// Watches a run for stalling: no output for its stall time, while its
/// process tree's CPU time hardly grew over that time.
#[derive(Debug)]
struct Watchdog {
    stall_after: Duration,
    every: Duration,
    last_output: Instant,
    /// CPU time samples of the process tree, oldest first.
    samples: VecDeque<(Instant, u64)>,
}

impl Watchdog {
    fn new(stall_after: Duration) -> Self {
        Self {
            stall_after,
            // Often enough to act within a few percent of the stall time.
            every: (stall_after / 20).clamp(Duration::from_millis(100), Duration::from_secs(60)),
            last_output: Instant::now(),
            samples: VecDeque::new(),
        }
    }

    fn output(&mut self) {
        self.last_output = Instant::now();
    }

    /// Takes a sample of `pid`'s tree and says whether the run has stalled.
    /// Without a readable `/proc` nothing is known, so nothing stalls.
    fn stalled(&mut self, pid: u32) -> bool {
        let now = Instant::now();
        let Some(cpu) = tree_cpu_ticks(pid) else {
            return false;
        };
        self.samples.push_back((now, cpu));
        while self
            .samples
            .front()
            .is_some_and(|(t, _)| now.duration_since(*t) > self.stall_after + 2 * self.every)
        {
            self.samples.pop_front();
        }
        if now.duration_since(self.last_output) < self.stall_after {
            return false;
        }
        // The CPU time at the start of the window, from the latest sample
        // taken at least `stall_after` ago.
        let Some(&(_, before)) = self
            .samples
            .iter()
            .rev()
            .find(|(t, _)| now.duration_since(*t) >= self.stall_after)
        else {
            return false;
        };
        // Clock ticks are 1/100 s on Linux; 1% of the window is a tick per
        // second of it.
        cpu.saturating_sub(before) < self.stall_after.as_secs().max(1)
    }
}

/// One process, from `/proc/<pid>/stat`.
#[derive(Debug, Clone, Copy)]
struct ProcStat {
    ppid: u32,
    pgrp: u32,
    /// User and system time, its own and its reaped children's, in ticks.
    cpu: u64,
}

fn proc_stat(pid: u32) -> Option<ProcStat> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name, in parentheses, may itself contain spaces.
    let rest = &text[text.rfind(')')? + 2..];
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let num = |i: usize| fields.get(i)?.parse::<u64>().ok();
    Some(ProcStat {
        ppid: u32::try_from(num(1)?).ok()?,
        pgrp: u32::try_from(num(2)?).ok()?,
        cpu: num(11)? + num(12)? + num(13)? + num(14)?,
    })
}

/// `root` and its descendants, found by following parent links.
fn tree(root: u32) -> Vec<(u32, ProcStat)> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let all: HashMap<u32, ProcStat> = entries
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
        .filter_map(|pid| Some((pid, proc_stat(pid)?)))
        .collect();
    let mut found = Vec::new();
    let mut frontier = vec![root];
    while let Some(pid) = frontier.pop() {
        if let Some(stat) = all.get(&pid) {
            found.push((pid, *stat));
            frontier.extend(all.iter().filter(|(_, s)| s.ppid == pid).map(|(p, _)| *p));
        }
    }
    found
}

/// The CPU time of `root`'s whole process tree, or `None` if `root` can't
/// be read.
fn tree_cpu_ticks(root: u32) -> Option<u64> {
    proc_stat(root)?;
    Some(tree(root).iter().map(|(_, s)| s.cpu).sum())
}

/// Kills `root`, its descendants and their process groups. Claude Code
/// starts background commands in process groups of their own, which killing
/// the agent's group alone would miss. The app's own group is spared.
fn kill_tree(root: u32) {
    let own = proc_stat(std::process::id()).map(|s| s.pgrp);
    let procs = tree(root);
    let groups: BTreeSet<u32> = procs
        .iter()
        .map(|(_, s)| s.pgrp)
        .filter(|g| Some(*g) != own)
        .collect();
    let mut targets: Vec<String> = groups.iter().map(|g| format!("-{g}")).collect();
    targets.extend(procs.iter().map(|(pid, _)| pid.to_string()));
    if targets.is_empty() {
        return;
    }
    // Errors are ignored: a process already gone needs no stopping.
    let _ = std::process::Command::new("kill")
        .arg("-KILL")
        .arg("--")
        .args(&targets)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
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
        // While the agent still runs, its descendants are found through it,
        // including those in process groups of their own.
        kill_tree(self.0);
        // The group id is the agent's pid (`process_group(0)`). Errors are
        // ignored: an empty group means there is nothing left to stop.
        let _ = std::process::Command::new("kill")
            .args(["-KILL", "--", &format!("-{}", self.0)])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

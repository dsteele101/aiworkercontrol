//! One agent run on an item: a first attempt in a fresh worktree, or a
//! rework of a pull request in the worktree it came from.

use std::fmt::Write as _;
use std::path::Path;
use std::sync::{Mutex, PoisonError};
use std::time::Instant;

use agent_backend::{Job, Provider};
use work_queue::{State as ItemState, WorkItem};

use super::trace::Trace;
use super::worker::PR_LINE;
use super::{PoolConfig, Shared, unix_now};
use crate::{PoolError, Project, PullRequests, agent, git, github, prompt};

/// Which kind of run an item gets.
#[derive(Debug, Clone)]
pub(super) enum Kind {
    /// A first attempt, or a retry, in a fresh worktree from the base.
    New,
    /// Addressing feedback on the open pull request, in the item's worktree.
    Rework {
        /// The pull request.
        pr_url: String,
        /// The commit last pushed to it; the rework must add to it.
        pushed_head: String,
        /// What reviewers or CI asked for.
        feedback: String,
        /// The pull request's branch on the push remote, if it is not the
        /// item's own: then the rework starts from it and adds to it.
        remote_branch: Option<String>,
    },
}

/// How a successful run leaves the item.
#[derive(Debug)]
pub(super) enum Finish {
    /// Done: the project does not publish pull requests.
    Close { report: String },
    /// Pushed to an open pull request: waits for review.
    Review {
        report: String,
        pr_url: String,
        /// The pull request's title when this run opened it; `None` for a
        /// rework, which leaves the recorded one as is.
        pr_title: Option<String>,
        head: String,
    },
    /// Committed but not published: needs a person.
    Hold { report: String, reason: String },
}

/// Whether the run may end without adding a commit. A rework may: the
/// feedback can be wrong, already addressed, or beyond the branch (a CI
/// outage), and the agent then rightly changes nothing. It must say so in a
/// summary, though; silence is indistinguishable from work left uncommitted.
fn may_leave_unchanged(kind: &Kind, outcome: &agent::Outcome) -> bool {
    matches!(kind, Kind::Rework { .. }) && outcome.summary().is_some_and(|s| !s.trim().is_empty())
}

impl Shared {
    pub(super) async fn run(
        &self,
        id: usize,
        item: &WorkItem,
        project: &Project,
        config: &PoolConfig,
        kind: &Kind,
        trace: &Mutex<Trace>,
    ) -> Result<Finish, PoolError> {
        let body = self.queue.read(ItemState::InProgress, item)?;
        let worktree = project.worktree_path(item);
        let branch = Project::branch(item);
        let (prompt, fresh, since) = match kind {
            Kind::New => {
                git::prepare_worktree(&project.repo, &project.base_ref, &worktree, &branch).await?;
                let prompt = prompt::build(project, item, &body)?;
                (prompt, true, project.base_ref.clone())
            }
            Kind::Rework {
                pr_url,
                pushed_head,
                feedback,
                remote_branch,
            } => {
                let fresh = git::ensure_worktree(&project.repo, &worktree, &branch).await?;
                if let (Some(remote_branch), Some(pr)) = (remote_branch, &project.pr) {
                    let source = git::push_url(&project.repo, &pr.push_remote).await?;
                    git::fast_forward(&worktree, &source, remote_branch).await?;
                }
                let prompt = prompt::build_rework(project, item, &body, pr_url, feedback)?;
                (prompt, fresh, pushed_head.clone())
            }
        };

        let log = project.log_path(item);
        let outcome = self
            .run_agent(
                id, item, project, config, &prompt, &worktree, fresh, &log, trace,
            )
            .await?;
        if outcome.halted {
            return Err(PoolError::ProviderLimit(config.provider.provider.label()));
        }
        lock(trace).stalled = outcome.stalled;
        let stall_minutes = config.stall_after.as_secs() / 60;
        if !outcome.succeeded() {
            let reason = if outcome.stalled {
                format!("stalled: no output and next to no CPU use for {stall_minutes} minutes")
            } else {
                outcome
                    .finished
                    .as_ref()
                    .and_then(|(_, s)| s.clone())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| outcome.stderr_tail.clone())
            };
            return Err(PoolError::AgentFailed(reason));
        }
        // A run must add commits: to the base on a first attempt, to the
        // pushed head on a rework. The exception is a rework whose agent
        // explains why it has nothing to change.
        let added = git::commits_since(&worktree, &since).await?;
        let unchanged = added.is_empty();
        if unchanged && !may_leave_unchanged(kind, &outcome) {
            return Err(PoolError::NoCommits(branch));
        }
        if !unchanged {
            // The size of the change is a nicety; a failure to count it
            // must not fail the turn.
            let diff = git::diff_stat(&worktree, &since).await.ok();
            let mut trace = lock(trace);
            trace.commits = u32::try_from(added.len()).unwrap_or(u32::MAX);
            trace.diff = diff;
        }
        let commits = git::commits_since(&worktree, &project.base_ref).await?;

        let mut report = format!(
            "# {item}\n\n\
             - Project: {project_name}\n\
             - Provider: {provider}\n\
             - Branch: `{branch}` (from `{base}`)\n\
             - Worktree: `{worktree}`\n\
             - Log: `{log}`\n",
            project_name = project.name,
            provider = config.provider.provider.label(),
            base = project.base_ref,
            worktree = worktree.display(),
            log = log.display(),
        );
        if outcome.stalled {
            let _ = writeln!(
                report,
                "- Note: the agent finished its last turn, then sat idle for {stall_minutes} \
                 minutes waiting on something that never ended, and was stopped"
            );
        }
        let mut tail = String::from("\n## Commits\n\n");
        for commit in &commits {
            let _ = writeln!(tail, "- {commit}");
        }
        if let Some(summary) = outcome.summary() {
            let heading = match kind {
                Kind::New => "Agent summary",
                Kind::Rework { .. } => "Agent summary (rework)",
            };
            let _ = write!(tail, "\n## {heading}\n\n{}\n", summary.trim());
        }

        let Some(pr) = &project.pr else {
            report.push_str(&tail);
            return Ok(Finish::Close { report });
        };
        let published = match kind {
            Kind::New => {
                self.publish(
                    project, pr, item, &worktree, &branch, &commits, &outcome, config,
                )
                .await
            }
            Kind::Rework { pr_url, .. } if unchanged => Ok((pr_url.clone(), None)),
            Kind::Rework {
                pr_url,
                remote_branch,
                ..
            } => {
                let pushed = match remote_branch {
                    Some(remote_branch) => {
                        git::push_ahead(&worktree, &pr.push_remote, &branch, remote_branch).await
                    }
                    None => git::push(&worktree, &pr.push_remote, &branch).await,
                };
                pushed.map(|()| (pr_url.clone(), None))
            }
        };
        Ok(match published {
            Ok((pr_url, pr_title)) => {
                let _ = writeln!(report, "{PR_LINE}{pr_url}");
                report.push_str(&tail);
                let head = git::head(&worktree).await?;
                Finish::Review {
                    report,
                    pr_url,
                    pr_title,
                    head,
                }
            }
            Err(e) => {
                let reason = format!("its work is committed, but was not published: {e}");
                let _ = writeln!(report, "- Not published: {e}");
                report.push_str(&tail);
                Finish::Hold { report, reason }
            }
        })
    }

    /// Whether `provider` is past a stop level, and not allowed to run on
    /// regardless.
    pub(super) fn stopped(&self, provider: Provider) -> bool {
        let stop = self.config.borrow().limits.clone();
        let state = self.lock();
        let hit = state
            .usage
            .get(&provider)
            .and_then(|u| stop.stop_hit(u, unix_now()));
        hit.is_some() && !state.picker.overrides.contains(&provider)
    }

    /// Completes once `provider` goes past a stop level.
    async fn over_limit(&self, provider: Provider) {
        // Subscribed before the check, so no change in between is missed.
        let mut changed = self.version.subscribe();
        loop {
            if self.stopped(provider) {
                return;
            }
            if changed.changed().await.is_err() {
                std::future::pending::<()>().await;
            }
        }
    }

    /// Prepares the run's log, build cache and (in a fresh worktree) setup,
    /// then runs the agent with `prompt` in `worktree`.
    #[allow(clippy::too_many_arguments)] // One call site; a struct would only rename them.
    async fn run_agent(
        &self,
        id: usize,
        item: &WorkItem,
        project: &Project,
        config: &PoolConfig,
        prompt: &str,
        worktree: &Path,
        fresh: bool,
        log: &Path,
        trace: &Mutex<Trace>,
    ) -> Result<agent::Outcome, PoolError> {
        tokio::fs::write(log, "")
            .await
            .map_err(PoolError::io("create", log))?;
        // A draft left by an earlier attempt must not be mistaken for this one.
        let pr_draft = project.pr_draft_path(item);
        match tokio::fs::remove_file(&pr_draft).await {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                return Err(PoolError::io("remove", &pr_draft)(e));
            }
            _ => {}
        }
        let build_cache = project.build_cache(id);
        tokio::fs::create_dir_all(&build_cache)
            .await
            .map_err(PoolError::io("create", &build_cache))?;
        let build_jobs = project.build_jobs.max(1).to_string();
        let env = [
            ("AIWC_REPO", project.repo.as_os_str()),
            ("AIWC_WORKTREE", worktree.as_os_str()),
            ("CARGO_TARGET_DIR", build_cache.as_os_str()),
            ("CARGO_BUILD_JOBS", build_jobs.as_ref()),
        ];
        if fresh && let Some(setup) = project.setup.as_deref().filter(|s| !s.trim().is_empty()) {
            agent::setup(setup, worktree, &env, log).await?;
        }

        let spec_dir = std::path::absolute(&project.spec_dir)
            .map_err(PoolError::io("resolve", &project.spec_dir))?;
        let mut extra_dirs = vec![spec_dir];
        if project.pr.is_some() {
            let dir = project.pr_draft_dir();
            tokio::fs::create_dir_all(&dir)
                .await
                .map_err(PoolError::io("create", &dir))?;
            extra_dirs.push(dir);
        }
        let writable_dirs = [build_cache.clone()];
        let job = Job {
            prompt,
            workdir: worktree,
            extra_dirs: &extra_dirs,
            writable_dirs: &writable_dirs,
        };
        let mut invocation = config.provider.invocation(&job)?;
        invocation
            .env
            .push(("CARGO_TARGET_DIR".into(), build_cache.display().to_string()));
        invocation.env.push(("CARGO_BUILD_JOBS".into(), build_jobs));
        let started = Instant::now();
        let outcome = agent::run(
            &invocation,
            config.provider.provider.cli(),
            config.cli_dir.as_deref(),
            log,
            config.stall_after,
            self.over_limit(config.provider.provider),
            |event| {
                lock(trace).observe(&event);
                self.push_event(id, event);
            },
        )
        .await;
        lock(trace).agent_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        outcome
    }

    /// Pushes the item's branch and opens (or finds) its pull request,
    /// titled and described by the agent's draft when it wrote one.
    #[allow(clippy::too_many_arguments)] // One call site; a struct would only rename them.
    async fn publish(
        &self,
        project: &Project,
        pr: &PullRequests,
        item: &WorkItem,
        worktree: &Path,
        branch: &str,
        commits: &[String],
        outcome: &agent::Outcome,
        config: &PoolConfig,
    ) -> Result<(String, Option<String>), PoolError> {
        let url = git::remote_url(&project.repo, &pr.push_remote).await?;
        let owner = github::owner_from_url(&url).ok_or_else(|| PoolError::NotGitHub {
            remote: pr.push_remote.clone(),
            url: url.clone(),
        })?;
        git::push(worktree, &pr.push_remote, branch).await?;

        let written = tokio::fs::read_to_string(project.pr_draft_path(item))
            .await
            .ok()
            .and_then(|text| github::Draft::parse(&text));
        let draft = written.unwrap_or_else(|| github::Draft {
            // Newest commit's subject, without its hash.
            title: commits
                .first()
                .and_then(|c| c.split_once(' '))
                .map_or_else(|| item.to_string(), |(_, subject)| subject.to_string()),
            body: outcome.summary().unwrap_or_default().trim().to_string(),
        });
        let gh = github::program(config.cli_dir.as_deref());
        let (url, title) =
            github::open_pr(&gh, &pr.repo, &pr.base, &owner, branch, &draft, pr.draft).await?;
        Ok((url, Some(title)))
    }
}

fn lock(trace: &Mutex<Trace>) -> std::sync::MutexGuard<'_, Trace> {
    // A trace is only ever added to, so a panic elsewhere leaves it usable.
    trace.lock().unwrap_or_else(PoisonError::into_inner)
}

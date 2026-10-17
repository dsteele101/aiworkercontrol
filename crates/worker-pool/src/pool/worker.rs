//! The worker loop: claim an item, carry it out, record the outcome.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::SystemTime;

use agent_backend::{AgentEvent, Job};
use work_queue::{State as ItemState, WorkItem};

use super::{POLL_INTERVAL, PoolConfig, Shared};
use crate::status::EVENT_TAIL;
use crate::{
    ActiveJob, Activity, ItemMeta, PoolError, Project, PullRequests, agent, git, github, prompt,
};

/// How a result file records its pull request; read back by `pull_request`.
pub(crate) const PR_LINE: &str = "- Pull request: ";

impl Shared {
    /// Whether worker `id` should exit. Decided under the state lock, together
    /// with removing it, so [`Shared::spawn_missing`] never races a retiring
    /// worker into leaving a slot empty or doubly filled.
    fn retire(&self, id: usize) -> bool {
        let mut state = self.lock();
        let retire = !*self.running.borrow() || id >= self.config.borrow().workers;
        if retire {
            state.workers.remove(&id);
        }
        retire
    }

    pub(super) async fn worker(self: Arc<Self>, id: usize) {
        loop {
            if self.retire(id) {
                self.changed();
                return;
            }
            let claimed = self.claim();
            if claimed.is_ok() && self.lock().last_error.take().is_some() {
                self.changed();
            }
            match claimed {
                Ok(Some((item, project, config))) => {
                    self.carry_out(id, item, project, config).await
                }
                Ok(None) => self.idle().await,
                Err(e) => {
                    self.lock().last_error = Some(e.to_string());
                    self.changed();
                    self.idle().await;
                }
            }
        }
    }

    async fn idle(&self) {
        tokio::select! {
            () = self.wake.notified() => {}
            () = tokio::time::sleep(POLL_INTERVAL) => {}
        }
    }

    fn claim(&self) -> Result<Option<(WorkItem, Project, PoolConfig)>, PoolError> {
        let config = self.config.borrow().clone();
        let item = self
            .queue
            .claim_next(|item| !config.projects.iter().any(|p| p.name == item.project))?;
        Ok(item.and_then(|item| {
            let project = config
                .projects
                .iter()
                .find(|p| p.name == item.project)?
                .clone();
            Some((item, project, config))
        }))
    }

    async fn carry_out(&self, id: usize, item: WorkItem, project: Project, config: PoolConfig) {
        self.set_activity(
            id,
            Activity::Working(ActiveJob {
                item: item.clone(),
                provider: config.provider.provider,
                started: SystemTime::now(),
                events: VecDeque::new(),
            }),
        );

        let outcome = match self.begin_turn(&item) {
            Ok(turn) => match self.run_item(id, &item, &project, &config).await {
                Ok(report) => self.finish(&item, &report).map_err(|e| (turn, e)),
                Err(e) => Err((turn, e)),
            },
            // Without a turn count the item cannot be tracked; hold it.
            Err(e) => Err((config.max_turns, e)),
        };
        if let Err((turn, e)) = outcome
            && let Err(bookkeeping) =
                self.after_failure(&item, &e.to_string(), turn, config.max_turns)
        {
            self.lock().last_error = Some(format!("{item}: {e}; then {bookkeeping}"));
        }
        self.set_activity(id, Activity::Idle);
    }

    /// Counts a new turn for an in-progress item and returns its number.
    fn begin_turn(&self, item: &WorkItem) -> Result<u32, PoolError> {
        let mut meta = ItemMeta::load(&self.queue, ItemState::InProgress, item)?;
        meta.turns += 1;
        meta.save(&self.queue, ItemState::InProgress, item)?;
        Ok(meta.turns)
    }

    /// Closes a successful item with its report.
    fn finish(&self, item: &WorkItem, report: &str) -> Result<(), PoolError> {
        let mut meta = ItemMeta::load(&self.queue, ItemState::InProgress, item)?;
        meta.last_error = None;
        meta.save(&self.queue, ItemState::InProgress, item)?;
        Ok(self.queue.complete(item, report)?)
    }

    /// Returns a failed item to `open` to be tried again, or puts it on hold
    /// once it has used its last turn.
    fn after_failure(
        &self,
        item: &WorkItem,
        reason: &str,
        turn: u32,
        max_turns: u32,
    ) -> Result<(), PoolError> {
        let mut meta = ItemMeta::load(&self.queue, ItemState::InProgress, item).unwrap_or_default();
        meta.last_error = Some(reason.to_string());
        if turn >= max_turns {
            meta.hold_reason = Some(format!(
                "used {turn} of {max_turns} turns; the last one failed: {reason}"
            ));
            meta.save(&self.queue, ItemState::InProgress, item)?;
            self.queue
                .transition(item, ItemState::InProgress, ItemState::Hold)?;
        } else {
            meta.save(&self.queue, ItemState::InProgress, item)?;
            self.queue.release(item)?;
        }
        Ok(())
    }

    async fn run_item(
        &self,
        id: usize,
        item: &WorkItem,
        project: &Project,
        config: &PoolConfig,
    ) -> Result<String, PoolError> {
        let body = self.queue.read(ItemState::InProgress, item)?;
        let prompt = prompt::build(project, item, &body)?;
        let worktree = project.worktree_path(item);
        let branch = Project::branch(item);
        git::prepare_worktree(&project.repo, &project.base_ref, &worktree, &branch).await?;

        let log = project.log_path(item);
        tokio::fs::write(&log, "")
            .await
            .map_err(PoolError::io("create", &log))?;
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
        let env = [
            ("AIWC_REPO", project.repo.as_os_str()),
            ("AIWC_WORKTREE", worktree.as_os_str()),
            ("CARGO_TARGET_DIR", build_cache.as_os_str()),
        ];
        if let Some(setup) = project.setup.as_deref().filter(|s| !s.trim().is_empty()) {
            agent::setup(setup, &worktree, &env, &log).await?;
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
            prompt: &prompt,
            workdir: &worktree,
            extra_dirs: &extra_dirs,
            writable_dirs: &writable_dirs,
        };
        let mut invocation = config.provider.invocation(&job)?;
        invocation
            .env
            .push(("CARGO_TARGET_DIR".into(), build_cache.display().to_string()));
        let outcome = agent::run(
            &invocation,
            config.provider.provider.cli(),
            config.cli_dir.as_deref(),
            &log,
            |event| self.push_event(id, event),
        )
        .await?;

        if !outcome.succeeded() {
            let reason = outcome
                .finished
                .as_ref()
                .and_then(|(_, s)| s.clone())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| outcome.stderr_tail.clone());
            return Err(PoolError::AgentFailed(reason));
        }
        let commits = git::commits_since(&worktree, &project.base_ref).await?;
        if commits.is_empty() {
            return Err(PoolError::NoCommits(branch));
        }

        let mut report = format!(
            "# {item}\n\n\
             - Project: {project_name}\n\
             - Provider: {provider}\n\
             - Branch: `{branch}` (from `{base}`)\n\
             - Worktree: `{worktree}`\n\
             - Log: `{log}`\n\n\
             ## Commits\n\n",
            project_name = project.name,
            provider = config.provider.provider.label(),
            base = project.base_ref,
            worktree = worktree.display(),
            log = log.display(),
        );
        for commit in &commits {
            let _ = writeln!(report, "- {commit}");
        }
        if let Some(summary) = outcome.summary() {
            let _ = write!(report, "\n## Agent summary\n\n{}\n", summary.trim());
        }
        if let Some(pr) = &project.pr {
            // The work is committed either way, so a failure here is reported
            // rather than failing the item and rerunning the agent.
            let line = match self
                .publish(
                    project, pr, item, &worktree, &branch, &commits, &outcome, config,
                )
                .await
            {
                Ok(url) => format!("{PR_LINE}{url}"),
                Err(e) => format!("- Pull request not opened: {e}"),
            };
            report = report.replacen("\n\n## Commits", &format!("\n{line}\n\n## Commits"), 1);
        }
        Ok(report)
    }

    /// Pushes the item's branch and opens (or finds) its pull request,
    /// titled and described by the agent's draft when it wrote one.
    #[allow(clippy::too_many_arguments)] // One call site; a struct would only rename them.
    async fn publish(
        &self,
        project: &Project,
        pr: &PullRequests,
        item: &WorkItem,
        worktree: &std::path::Path,
        branch: &str,
        commits: &[String],
        outcome: &agent::Outcome,
        config: &PoolConfig,
    ) -> Result<String, PoolError> {
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
        github::open_pr(&gh, &pr.repo, &pr.base, &owner, branch, &draft, pr.draft).await
    }

    fn set_activity(&self, id: usize, activity: Activity) {
        self.lock().workers.insert(id, activity);
        self.changed();
    }

    fn push_event(&self, id: usize, event: AgentEvent) {
        {
            let mut state = self.lock();
            if let Some(Activity::Working(job)) = state.workers.get_mut(&id) {
                if job.events.len() == EVENT_TAIL {
                    job.events.pop_front();
                }
                job.events.push_back(event);
            }
        }
        self.version.send_modify(|v| *v = v.wrapping_add(1));
    }
}

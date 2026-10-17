//! The worker loop: claim an item, carry it out, record the outcome.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::SystemTime;

use agent_backend::{AgentEvent, Job};
use work_queue::{State as ItemState, WorkItem};

use super::{POLL_INTERVAL, PoolConfig, Shared};
use crate::status::EVENT_TAIL;
use crate::{ActiveJob, Activity, PoolError, Project, agent, git, prompt};

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
        let item = {
            let state = self.lock();
            self.queue.claim_next(|item| {
                state.failed.contains_key(item)
                    || !config.projects.iter().any(|p| p.name == item.project)
            })?
        };
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

        let outcome = match self.run_item(id, &item, &project, &config).await {
            Ok(report) => self.queue.complete(&item, &report).map_err(PoolError::from),
            Err(e) => Err(e),
        };
        if let Err(e) = outcome {
            // Releasing can only fail if the file vanished from `inprogress`;
            // the failure is still recorded so the item is not retried blindly.
            let reason = match self.queue.release(&item) {
                Ok(()) => e.to_string(),
                Err(release) => format!("{e}; also could not reopen: {release}"),
            };
            self.lock().failed.insert(item, reason);
        }
        self.set_activity(id, Activity::Idle);
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

        let spec_dir = std::path::absolute(&project.spec_dir)
            .map_err(PoolError::io("resolve", &project.spec_dir))?;
        let extra_dirs = [spec_dir];
        let job = Job {
            prompt: &prompt,
            workdir: &worktree,
            extra_dirs: &extra_dirs,
        };
        let invocation = config.provider.invocation(&job)?;
        let log = project.log_path(item);
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
        Ok(report)
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

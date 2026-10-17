//! One agent run on an item: a first attempt in a fresh worktree, or a
//! rework of a pull request in the worktree it came from.

use std::fmt::Write as _;
use std::path::Path;

use agent_backend::Job;
use work_queue::{State as ItemState, WorkItem};

use super::worker::PR_LINE;
use super::{PoolConfig, Shared};
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
        head: String,
    },
    /// Committed but not published: needs a person.
    Hold { report: String, reason: String },
}

impl Shared {
    pub(super) async fn run(
        &self,
        id: usize,
        item: &WorkItem,
        project: &Project,
        config: &PoolConfig,
        kind: &Kind,
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
            } => {
                let fresh = git::ensure_worktree(&project.repo, &worktree, &branch).await?;
                let prompt = prompt::build_rework(project, item, &body, pr_url, feedback)?;
                (prompt, fresh, pushed_head.clone())
            }
        };

        let log = project.log_path(item);
        let outcome = self
            .run_agent(id, item, project, config, &prompt, &worktree, fresh, &log)
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
        // A run must add commits: to the base on a first attempt, to the
        // pushed head on a rework.
        if git::commits_since(&worktree, &since).await?.is_empty() {
            return Err(PoolError::NoCommits(branch));
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
            Kind::Rework { pr_url, .. } => git::push(&worktree, &pr.push_remote, &branch)
                .await
                .map(|()| pr_url.clone()),
        };
        Ok(match published {
            Ok(pr_url) => {
                let _ = writeln!(report, "{PR_LINE}{pr_url}");
                report.push_str(&tail);
                let head = git::head(&worktree).await?;
                Finish::Review {
                    report,
                    pr_url,
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
        let env = [
            ("AIWC_REPO", project.repo.as_os_str()),
            ("AIWC_WORKTREE", worktree.as_os_str()),
            ("CARGO_TARGET_DIR", build_cache.as_os_str()),
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
        agent::run(
            &invocation,
            config.provider.provider.cli(),
            config.cli_dir.as_deref(),
            log,
            |event| self.push_event(id, event),
        )
        .await
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
}

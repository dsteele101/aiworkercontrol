//! Suggestions for the agent instructions: learning from failed runs,
//! evaluating the user's comments, and acting on their decisions.

use std::path::PathBuf;
use std::sync::{Arc, PoisonError};
use std::time::Duration;

use agent_backend::Job;
use work_queue::WorkItem;

use super::{PoolConfig, Shared, Supervisor, unix_now};
use crate::suggestions::{self, Notes, Response};
use crate::{
    Author, NOTES_FILE, NotesVersion, PoolError, Project, ProjectSuggestions, Remark, Suggestion,
    SuggestionStatus, agent, git,
};

/// How long an evaluating agent may take.
const EVALUATION_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// How much of the end of a failed run's log the evaluating agent sees.
const LOG_TAIL_CHARS: usize = 6000;

/// How much of a failure's error, or of a review's feedback, a suggestion
/// keeps.
const ERROR_CHARS: usize = 2000;

/// What a suggestion is drawn from.
#[derive(Debug)]
pub(super) enum Evidence {
    /// A run that failed, of the given kind, with its error.
    Failure { kind: &'static str, error: String },
    /// A review that asked for changes to the item's pull request, as the
    /// rework prompt carries it.
    Review { feedback: String },
}

impl Evidence {
    /// The kind a suggestion records, and the text behind it.
    fn parts(&self) -> (&str, &str) {
        match self {
            Self::Failure { kind, error } => (kind, error),
            Self::Review { feedback } => ("review", feedback),
        }
    }
}

impl Supervisor {
    /// Every project's suggestions to act on, and the rejected ones still
    /// kept, for projects that have any or have a notes history. Rejections
    /// past the retention time are forgotten first.
    pub fn suggestions(&self) -> Result<Vec<ProjectSuggestions>, PoolError> {
        let mut all = Vec::new();
        for project in self.config().projects {
            let shared = &self.shared;
            let listed = shared.view(&project.name, |notes| {
                let mut open: Vec<_> = notes
                    .suggestions
                    .iter()
                    .filter(|s| {
                        matches!(
                            s.status,
                            SuggestionStatus::Pending | SuggestionStatus::Evaluating
                        )
                    })
                    .cloned()
                    .collect();
                open.sort_by_key(|s| s.created_at);
                let mut rejected: Vec<_> = notes
                    .suggestions
                    .iter()
                    .filter(|s| s.status == SuggestionStatus::Rejected)
                    .cloned()
                    .collect();
                rejected.sort_by_key(|s| std::cmp::Reverse(s.decided_at));
                (open, rejected, notes.versions.len())
            })?;
            let (open, rejected, versions) = listed;
            if !open.is_empty() || !rejected.is_empty() || versions > 0 {
                all.push(ProjectSuggestions {
                    project: project.name,
                    open,
                    rejected,
                    versions,
                });
            }
        }
        Ok(all)
    }

    /// Adds suggestion `id` of `project` to its notes file.
    pub fn accept_suggestion(&self, project: &str, id: u32) -> Result<(), PoolError> {
        let path = self.notes_path(project)?;
        self.shared.with_notes(project, |notes| {
            let text = decidable(notes.find_mut(id)?)?.text.clone();
            integrate(notes, &path, id, &text)
        })
    }

    /// Adds suggestion `id` to the notes file with the user's `comment`
    /// added to it as a refinement.
    pub fn amend_suggestion(&self, project: &str, id: u32, comment: &str) -> Result<(), PoolError> {
        let path = self.notes_path(project)?;
        let comment = comment.trim();
        self.shared.with_notes(project, |notes| {
            let suggestion = decidable(notes.find_mut(id)?)?;
            if !comment.is_empty() {
                suggestion.remarks.push(Remark {
                    by: Author::User,
                    text: comment.to_string(),
                    at: unix_now(),
                });
                suggestion.text = format!("{}\n{comment}", suggestion.text.trim());
            }
            let text = suggestion.text.clone();
            integrate(notes, &path, id, &text)
        })
    }

    /// Turns suggestion `id` of `project` down. It is kept for the
    /// configured retention time, and [`Supervisor::restore_suggestion`]
    /// brings it back.
    pub fn reject_suggestion(&self, project: &str, id: u32) -> Result<(), PoolError> {
        self.shared.with_notes(project, |notes| {
            let suggestion = notes.find_mut(id)?;
            if suggestion.status != SuggestionStatus::Pending {
                return Err(PoolError::SuggestionState(id));
            }
            suggestion.status = SuggestionStatus::Rejected;
            suggestion.decided_at = Some(unix_now());
            Ok(())
        })?;
        self.shared.changed();
        Ok(())
    }

    /// Makes a rejected suggestion pending again.
    pub fn restore_suggestion(&self, project: &str, id: u32) -> Result<(), PoolError> {
        self.shared.with_notes(project, |notes| {
            let suggestion = notes.find_mut(id)?;
            if suggestion.status != SuggestionStatus::Rejected {
                return Err(PoolError::SuggestionState(id));
            }
            suggestion.status = SuggestionStatus::Pending;
            suggestion.decided_at = None;
            Ok(())
        })?;
        self.shared.changed();
        Ok(())
    }

    /// Sends suggestion `id` with the user's `comment` to the agent for
    /// evaluation, and returns at once. The agent keeps, revises or
    /// withdraws the suggestion; either way the answer is added to it, and
    /// the user decides on what comes back. Must be called inside a Tokio
    /// runtime.
    pub fn evaluate_suggestion(
        &self,
        project: &str,
        id: u32,
        comment: &str,
    ) -> Result<(), PoolError> {
        let project = self.named_project(project)?;
        let comment = comment.trim();
        self.shared.with_notes(&project.name, |notes| {
            let suggestion = notes.find_mut(id)?;
            if suggestion.status != SuggestionStatus::Pending {
                return Err(PoolError::SuggestionState(id));
            }
            suggestion.status = SuggestionStatus::Evaluating;
            suggestion.evaluation_error = None;
            suggestion.remarks.push(Remark {
                by: Author::User,
                text: comment.to_string(),
                at: unix_now(),
            });
            Ok(())
        })?;
        self.shared.changed();
        tokio::spawn(Arc::clone(&self.shared).evaluate_comment(project, id));
        Ok(())
    }

    /// The versions the notes file of `project` has had, newest first.
    pub fn notes_versions(&self, project: &str) -> Result<Vec<NotesVersion>, PoolError> {
        self.named_project(project)?;
        let mut versions = self
            .shared
            .with_notes(project, |n| Ok(n.versions.clone()))?;
        versions.reverse();
        Ok(versions)
    }

    /// Puts the notes file of `project` back as version `version` had it.
    /// Suggestions already accepted stay accepted; the file keeps the
    /// earlier version's text.
    pub fn revert_notes(&self, project: &str, version: u32) -> Result<(), PoolError> {
        let path = self.notes_path(project)?;
        self.shared
            .with_notes(project, |notes| notes.revert(&path, version, unix_now()))
    }

    fn named_project(&self, name: &str) -> Result<Project, PoolError> {
        self.config()
            .projects
            .into_iter()
            .find(|p| p.name == name)
            .ok_or_else(|| PoolError::UnknownProject(name.to_string()))
    }

    fn notes_path(&self, project: &str) -> Result<PathBuf, PoolError> {
        Ok(self.named_project(project)?.spec_dir.join(NOTES_FILE))
    }
}

/// `suggestion`, if the user may still accept it.
fn decidable(suggestion: &mut Suggestion) -> Result<&mut Suggestion, PoolError> {
    match suggestion.status {
        SuggestionStatus::Pending | SuggestionStatus::Rejected => Ok(suggestion),
        SuggestionStatus::Evaluating | SuggestionStatus::Accepted => {
            Err(PoolError::SuggestionState(suggestion.id))
        }
    }
}

/// Adds `text` to the notes file and marks suggestion `id` accepted.
fn integrate(
    notes: &mut Notes,
    path: &std::path::Path,
    id: u32,
    text: &str,
) -> Result<(), PoolError> {
    let now = unix_now();
    notes.append(path, text, format!("accepted suggestion {id}"), now)?;
    let suggestion = notes.find_mut(id)?;
    suggestion.status = SuggestionStatus::Accepted;
    suggestion.decided_at = Some(now);
    Ok(())
}

impl Shared {
    fn notes_dir(&self) -> PathBuf {
        self.queue.root().join("suggestions")
    }

    /// Reads `project`'s saved suggestions and notes history with `f`.
    /// Rejections past the retention time are forgotten first.
    pub(super) fn view<T>(
        &self,
        project: &str,
        f: impl FnOnce(&Notes) -> T,
    ) -> Result<T, PoolError> {
        let _held = self
            .notes_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let dir = self.notes_dir();
        let mut notes = Notes::load(&dir, project)?;
        let retention = self.config.borrow().suggestion_retention.as_secs();
        if notes.purge_rejected(retention, unix_now()) {
            notes.save(&dir, project)?;
        }
        Ok(f(&notes))
    }

    /// Runs `f` on `project`'s saved suggestions and notes history, then
    /// saves them. Nothing is saved if `f` fails.
    pub(super) fn with_notes<T>(
        &self,
        project: &str,
        f: impl FnOnce(&mut Notes) -> Result<T, PoolError>,
    ) -> Result<T, PoolError> {
        let _held = self
            .notes_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let dir = self.notes_dir();
        let mut notes = Notes::load(&dir, project)?;
        let retention = self.config.borrow().suggestion_retention.as_secs();
        let purged = notes.purge_rejected(retention, unix_now());
        let result = f(&mut notes)?;
        // Without a file there is nothing worth creating.
        if purged || !notes.suggestions.is_empty() || !notes.versions.is_empty() {
            notes.save(&dir, project)?;
        }
        Ok(result)
    }

    /// Shows an error of the suggestions feature on the dashboard. They are
    /// a nicety; a failure stops nothing.
    fn suggestion_error(&self, error: &PoolError) {
        self.lock().last_error = Some(format!("suggestions: {error}"));
        self.changed();
    }

    /// Has an agent look at what went wrong with `item`, and saves the
    /// lesson it suggests, if it sees one.
    pub(super) async fn learn_from(
        self: Arc<Self>,
        project: Project,
        item: WorkItem,
        body: String,
        evidence: Evidence,
        config: PoolConfig,
    ) {
        if self.stopped(config.provider.provider) {
            return;
        }
        let result = self
            .evaluate(&project, &item, &body, &evidence, &config)
            .await;
        match result {
            Ok(Some(text)) => {
                let (kind, detail) = evidence.parts();
                let detail: String = detail.chars().take(ERROR_CHARS).collect();
                let added = self.with_notes(&project.name, |notes| {
                    notes.add(&item.file, kind, &detail, &text, unix_now());
                    Ok(())
                });
                match added {
                    Ok(()) => self.changed(),
                    Err(e) => self.suggestion_error(&e),
                }
            }
            Ok(None) => {}
            Err(e) => self.suggestion_error(&e),
        }
    }

    async fn evaluate(
        &self,
        project: &Project,
        item: &WorkItem,
        body: &str,
        evidence: &Evidence,
        config: &PoolConfig,
    ) -> Result<Option<String>, PoolError> {
        let pending = self.view(&project.name, |notes| {
            notes
                .suggestions
                .iter()
                .filter(|s| s.status == SuggestionStatus::Pending)
                .cloned()
                .collect::<Vec<_>>()
        })?;
        let notes = read_notes(project);
        let prompt = match evidence {
            Evidence::Failure { kind, error } => {
                let log = std::fs::read_to_string(project.log_path(item)).unwrap_or_default();
                suggestions::failure_prompt(
                    &project.name,
                    &item.file,
                    body,
                    kind,
                    error,
                    tail(&log, LOG_TAIL_CHARS),
                    &notes,
                    &pending,
                )
            }
            Evidence::Review { feedback } => suggestions::review_prompt(
                &project.name,
                &item.file,
                body,
                feedback,
                &notes,
                &pending,
            ),
        };
        let name = format!("{}-{}-{}", project.name, item.stem(), evidence.parts().0);
        let response = self.ask(config, &prompt, &name).await?;
        Ok((response.verdict == "suggest")
            .then_some(response.suggestion)
            .flatten()
            .filter(|s| !s.trim().is_empty()))
    }

    /// Has an agent evaluate suggestion `id` of `project` against the
    /// comments on it, and records its answer.
    async fn evaluate_comment(self: Arc<Self>, project: Project, id: u32) {
        let config = self.config.borrow().clone();
        let asked = self
            .view(&project.name, |notes| {
                notes.suggestions.iter().find(|s| s.id == id).cloned()
            })
            .and_then(|s| s.ok_or(PoolError::NoSuggestion(id)))
            .map(|s| suggestions::comment_prompt(&project.name, &s, &read_notes(&project)));
        let answer = match asked {
            Ok(prompt) => {
                let name = format!("{}-suggestion-{id}", project.name);
                self.ask(&config, &prompt, &name).await
            }
            Err(e) => Err(e),
        };
        let recorded = self.with_notes(&project.name, |notes| {
            let suggestion = notes.find_mut(id)?;
            suggestion.status = SuggestionStatus::Pending;
            let at = unix_now();
            match &answer {
                Ok(response) => apply_answer(suggestion, response, at),
                Err(e) => suggestion.evaluation_error = Some(e.to_string()),
            }
            Ok(())
        });
        if let Err(e) = recorded {
            self.suggestion_error(&e);
        }
        self.changed();
    }

    /// Runs the active provider's agent with `prompt` where it can touch
    /// nothing of the project's, and reads its answer. `name` tells this
    /// run's log from the others'.
    async fn ask(
        &self,
        config: &PoolConfig,
        prompt: &str,
        name: &str,
    ) -> Result<Response, PoolError> {
        let message = self.ask_message(config, prompt, name).await?;
        Response::parse(&message)
            .ok_or_else(|| PoolError::BadAnswer(message.chars().take(200).collect()))
    }

    /// Like [`Shared::ask`], but returns the agent's final message as is.
    pub(super) async fn ask_message(
        &self,
        config: &PoolConfig,
        prompt: &str,
        name: &str,
    ) -> Result<String, PoolError> {
        let dir = self.notes_dir().join(".run");
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(PoolError::io("create", &dir))?;
        git::ensure_repo(&dir).await?;
        let log = dir.join(format!("{name}.log"));
        tokio::fs::write(&log, "")
            .await
            .map_err(PoolError::io("create", &log))?;
        let job = Job {
            prompt,
            workdir: &dir,
            extra_dirs: &[],
            writable_dirs: &[],
        };
        let invocation = config.provider.invocation(&job)?;
        let run = agent::run(
            &invocation,
            config.provider.provider.cli(),
            config.cli_dir.as_deref(),
            &log,
            config.stall_after,
            std::future::pending(),
            |_| {},
        );
        let outcome = tokio::time::timeout(EVALUATION_TIMEOUT, run)
            .await
            .map_err(|_| PoolError::AgentFailed("the evaluation timed out".into()))??;
        if !outcome.succeeded() {
            let reason = outcome.summary().unwrap_or(&outcome.stderr_tail);
            return Err(PoolError::AgentFailed(reason.to_string()));
        }
        Ok(outcome.summary().unwrap_or_default().to_string())
    }
}

/// Applies the agent's `response` to the `suggestion` it evaluated.
fn apply_answer(suggestion: &mut Suggestion, response: &Response, at: u64) {
    let reply = response.reply.as_deref().map(str::trim).unwrap_or_default();
    let revised = response
        .suggestion
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    match (response.verdict.as_str(), revised) {
        ("revise", Some(text)) => suggestion.text = text.to_string(),
        ("withdraw", _) => {
            suggestion.status = SuggestionStatus::Rejected;
            suggestion.decided_at = Some(at);
        }
        ("keep" | "revise", _) => {}
        (other, _) => {
            suggestion.evaluation_error = Some(format!("the agent answered {other:?}"));
        }
    }
    if !reply.is_empty() {
        suggestion.remarks.push(Remark {
            by: Author::Agent,
            text: reply.to_string(),
            at,
        });
    }
}

/// The text of `project`'s notes file; empty if it has none.
fn read_notes(project: &Project) -> String {
    std::fs::read_to_string(project.spec_dir.join(NOTES_FILE)).unwrap_or_default()
}

/// The last `chars` characters of `text`.
fn tail(text: &str, chars: usize) -> &str {
    let skip = text.chars().count().saturating_sub(chars);
    text.char_indices()
        .nth(skip)
        .map_or("", |(i, _)| &text[i..])
}

#[cfg(test)]
mod tests {
    // Test code asserts rather than propagating; see Rust code review.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]

    use super::*;

    fn suggestion() -> Suggestion {
        let mut notes = Notes::default();
        let id = notes.add("a.txt", "agent", "boom", "Run the tests.", 0);
        notes.find_mut(id).unwrap().clone()
    }

    fn response(verdict: &str, suggestion: Option<&str>, reply: &str) -> Response {
        Response {
            verdict: verdict.into(),
            suggestion: suggestion.map(str::to_string),
            reply: Some(reply.into()),
        }
    }

    #[test]
    fn revising_replaces_the_text_and_keeps_the_reply() {
        let mut s = suggestion();
        apply_answer(
            &mut s,
            &response("revise", Some("Run `just ci`."), "Fair."),
            9,
        );
        assert_eq!(s.text, "Run `just ci`.");
        assert_eq!(s.status, SuggestionStatus::Pending);
        assert_eq!(s.remarks.last().unwrap().by, Author::Agent);
    }

    #[test]
    fn withdrawing_rejects_it() {
        let mut s = suggestion();
        apply_answer(&mut s, &response("withdraw", None, "Not general."), 9);
        assert_eq!(s.status, SuggestionStatus::Rejected);
        assert_eq!(s.decided_at, Some(9));
    }

    #[test]
    fn an_unknown_verdict_is_reported() {
        let mut s = suggestion();
        apply_answer(&mut s, &response("maybe", None, ""), 9);
        assert!(s.evaluation_error.unwrap().contains("maybe"));
        assert!(s.remarks.is_empty());
    }

    #[test]
    fn tail_keeps_whole_characters() {
        assert_eq!(tail("héllo wörld", 5), "wörld");
        assert_eq!(tail("short", 50), "short");
        assert_eq!(tail("", 5), "");
    }
}

//! Suggestions for improving a project's agent instructions, drawn from the
//! agent failures the pool sees.
//!
//! When a run fails for a reason within the agent's control, an agent
//! evaluates the failure and may suggest a lesson for future runs. So does a
//! review that asks for changes to an item's pull request. A person
//! then accepts, rejects or comments on each suggestion. An accepted one is
//! added to the project's notes file ([`NOTES_FILE`]) in its spec directory,
//! where every later run is pointed at it.
//!
//! Each project's suggestions and every version its notes file has had are
//! kept in `suggestions/<project>.json` in the work directory, so the file can
//! be put back as it was at any earlier point.

mod evaluate;
mod notes;

use serde::{Deserialize, Serialize};

pub(crate) use evaluate::{Response, comment_prompt, eligible, failure_prompt, review_prompt};
pub use notes::NOTES_FILE;
pub(crate) use notes::Notes;

/// Where a suggestion stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SuggestionStatus {
    /// Waiting for a decision.
    Pending,
    /// The user commented and asked the agent to evaluate it; the agent's
    /// answer is on its way.
    Evaluating,
    /// Added to the notes file.
    Accepted,
    /// Turned down. Kept for the configured retention time, so the user can
    /// change their mind.
    Rejected,
}

/// Who wrote a [`Remark`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Author {
    /// The person using the dashboard.
    User,
    /// The agent evaluating the suggestion.
    Agent,
}

/// A comment on a suggestion, or the agent's answer to one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Remark {
    /// Who wrote it.
    pub by: Author,
    /// What it says.
    pub text: String,
    /// When, in Unix seconds.
    pub at: u64,
}

/// One suggested lesson for a project's agents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Suggestion {
    /// The suggestion's number, unique within its project.
    pub id: u32,
    /// The work item whose failure prompted it, as `file`.
    pub item: String,
    /// What prompted it: a failure of kind `agent`, `stalled` or
    /// `no_commits`, or `review` for changes a reviewer requested.
    pub failure: String,
    /// The failure's error, or the reviewer's feedback, cut short.
    pub error: String,
    /// The lesson, as it would appear in the notes file.
    pub text: String,
    /// When it was made, in Unix seconds.
    pub created_at: u64,
    /// Where it stands.
    pub status: SuggestionStatus,
    /// When it was accepted or rejected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_at: Option<u64>,
    /// The comments on it and the agent's answers, oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remarks: Vec<Remark>,
    /// Why the last evaluation by an agent failed, if it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evaluation_error: Option<String>,
}

/// One version of a project's notes file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotesVersion {
    /// The version's number, from 1, unique within its project.
    pub id: u32,
    /// When the file took this form, in Unix seconds.
    pub at: u64,
    /// What made it, e.g. `accepted suggestion 3`.
    pub note: String,
    /// The whole file.
    pub content: String,
}

/// A project's suggestions and notes history, as the dashboard shows them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectSuggestions {
    /// The project.
    pub project: String,
    /// The suggestions to act on or waiting on an agent, oldest first.
    pub open: Vec<Suggestion>,
    /// The rejected ones still within the retention time, newest first.
    pub rejected: Vec<Suggestion>,
    /// How many versions the notes file has had.
    pub versions: usize,
}

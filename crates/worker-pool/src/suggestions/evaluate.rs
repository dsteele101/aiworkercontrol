//! Deciding which failures to learn from, and what the evaluating agent is
//! asked and answers.

use serde::Deserialize;

use super::{Author, Suggestion};

/// Phrases that mark a failure as caused by the network or the provider's
/// service rather than by the agent.
const OUTSIDE_CAUSES: [&str; 22] = [
    "connection reset",
    "connection refused",
    "connection closed",
    "connection error",
    "econnreset",
    "econnrefused",
    "etimedout",
    "enotfound",
    "socket hang up",
    "unable to connect",
    "could not resolve",
    "name resolution",
    "network",
    "timed out",
    "rate limit",
    "rate_limit",
    "overloaded",
    "service unavailable",
    "bad gateway",
    "gateway timeout",
    "internal server error",
    "api error: 5",
];

/// Whether a failed run of kind `failure` with `error` is worth learning
/// from: the agent could have done differently. Setup, git, GitHub and spawn
/// failures, plan limits, and anything the network or the provider's service
/// caused are not.
pub(crate) fn eligible(failure: &str, error: &str) -> bool {
    if !matches!(failure, "agent" | "stalled" | "no_commits") {
        return false;
    }
    let error = error.to_lowercase();
    !OUTSIDE_CAUSES.iter().any(|cause| error.contains(cause))
}

/// The agent's answer to an evaluation.
#[derive(Debug, Deserialize)]
pub(crate) struct Response {
    /// `suggest`, `external` or `duplicate` for a failure; `keep`, `revise`
    /// or `withdraw` for a comment.
    pub verdict: String,
    /// The lesson to suggest, or the revised one.
    #[serde(default)]
    pub suggestion: Option<String>,
    /// What the agent says to the user.
    #[serde(default)]
    pub reply: Option<String>,
}

impl Response {
    /// Reads the answer out of the agent's final message: the JSON object in
    /// it, which the agent may have wrapped in a code fence or a sentence.
    pub fn parse(message: &str) -> Option<Self> {
        let start = message.find('{')?;
        let end = message.rfind('}')?;
        serde_json::from_str(message.get(start..=end)?).ok()
    }
}

const FORMAT_FAILURE: &str = "Answer with a single JSON object and nothing else:\n\
    {\"verdict\": \"suggest\" | \"external\" | \"duplicate\", \"suggestion\": \"...\"}\n\
    - \"suggest\": the agent could have avoided this failure with better instructions. \
      \"suggestion\" is one lesson for the project's notes file, written as an instruction \
      to a future agent: specific, short, and useful beyond this one work item.\n\
    - \"external\": the failure was not the agent's doing (network, the provider's service, \
      the environment). Leave \"suggestion\" out.\n\
    - \"duplicate\": the notes or a pending suggestion already cover it. Leave \"suggestion\" out.\n";

const FORMAT_COMMENT: &str = "Answer with a single JSON object and nothing else:\n\
    {\"verdict\": \"keep\" | \"revise\" | \"withdraw\", \"suggestion\": \"...\", \"reply\": \"...\"}\n\
    - \"keep\": the suggestion stands as written. Leave \"suggestion\" out.\n\
    - \"revise\": the comment is right. \"suggestion\" is the improved lesson, in full.\n\
    - \"withdraw\": the comment shows the suggestion is wrong or not worth keeping. \
      Leave \"suggestion\" out.\n\
    \"reply\" is one or two sentences to the user saying why.\n";

const READ_ONLY: &str = "Do not change any files, run commands or use tools; \
    answer from what is given here.\n";

/// What a notes file looks like to the agent, or that there is none.
fn notes_block(notes: &str) -> String {
    if notes.trim().is_empty() {
        "<notes>\n(none yet)\n</notes>\n\n".to_string()
    } else {
        format!("<notes>\n{}\n</notes>\n\n", notes.trim())
    }
}

/// The prompt for evaluating a failed run: what the item asked, how it
/// failed, the end of the agent's log, the notes so far, and the suggestions
/// still pending.
#[allow(clippy::too_many_arguments)] // One call site; a struct would only rename them.
pub(crate) fn failure_prompt(
    project: &str,
    item: &str,
    body: &str,
    failure: &str,
    error: &str,
    log_tail: &str,
    notes: &str,
    pending: &[Suggestion],
) -> String {
    let mut prompt = format!(
        "An AI coding agent failed a work item on the project \"{project}\". Decide whether \
         better instructions would have prevented the failure, and if so suggest one lesson \
         to add to the project's notes file, which every agent run is told to read.\n\n\
         <work-item name=\"{item}\">\n{body}\n</work-item>\n\n\
         The failure ({failure}):\n<error>\n{error}\n</error>\n\n\
         The end of the agent's log:\n<log>\n{log_tail}\n</log>\n\n",
        body = body.trim(),
        error = error.trim(),
        log_tail = log_tail.trim(),
    );
    prompt.push_str(&notes_block(notes));
    if !pending.is_empty() {
        prompt.push_str("Suggestions already waiting for the user:\n");
        for s in pending {
            prompt.push_str(&format!("- {}\n", s.text.replace('\n', " ")));
        }
        prompt.push('\n');
    }
    prompt.push_str(READ_ONLY);
    prompt.push_str(FORMAT_FAILURE);
    prompt
}

/// The prompt for evaluating `suggestion` in light of the user's comment on
/// it, which is its last remark.
pub(crate) fn comment_prompt(project: &str, suggestion: &Suggestion, notes: &str) -> String {
    let mut prompt = format!(
        "You suggested a lesson for the notes file of the project \"{project}\", which every \
         AI coding agent run on it is told to read. The user commented on your suggestion and \
         asks you to evaluate it again.\n\n\
         <suggestion>\n{text}\n</suggestion>\n\n",
        text = suggestion.text.trim(),
    );
    if !suggestion.failure.is_empty() {
        prompt.push_str(&format!(
            "It came from a failure of {item} ({kind}):\n<error>\n{error}\n</error>\n\n",
            item = suggestion.item,
            kind = suggestion.failure,
            error = suggestion.error.trim(),
        ));
    }
    prompt.push_str(&notes_block(notes));
    prompt.push_str("The conversation so far:\n");
    for remark in &suggestion.remarks {
        let who = match remark.by {
            Author::User => "user",
            Author::Agent => "you",
        };
        prompt.push_str(&format!("<{who}>{}</{who}>\n", remark.text.trim()));
    }
    prompt.push('\n');
    prompt.push_str(READ_ONLY);
    prompt.push_str(FORMAT_COMMENT);
    prompt
}

#[cfg(test)]
mod tests {
    // Test code asserts rather than propagating; see Rust code review.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]

    use super::*;
    use crate::suggestions::{Remark, SuggestionStatus};

    #[test]
    fn learns_only_from_failures_the_agent_could_avoid() {
        assert!(eligible(
            "agent",
            "tests still failing after three attempts"
        ));
        assert!(eligible("no_commits", "agent finished without committing"));
        assert!(eligible("stalled", "stalled: no output for 20 minutes"));
        assert!(!eligible("git", "git push failed"));
        assert!(!eligible("setup", "setup command failed"));
        assert!(!eligible("limit", "went past its stop level"));
        assert!(!eligible("agent", "API Error: connection reset by peer"));
        assert!(!eligible("agent", "Overloaded, try again later"));
        assert!(!eligible("agent", "request timed out"));
        assert!(!eligible("agent", "API Error: 503 upstream"));
    }

    #[test]
    fn reads_an_answer_wrapped_in_text() {
        let message =
            "Here you go:\n```json\n{\"verdict\":\"suggest\",\"suggestion\":\"Run it.\"}\n```";
        let response = Response::parse(message).unwrap();
        assert_eq!(response.verdict, "suggest");
        assert_eq!(response.suggestion.as_deref(), Some("Run it."));
        assert!(Response::parse("no json here").is_none());
    }

    #[test]
    fn comment_prompt_carries_the_conversation() {
        let suggestion = Suggestion {
            id: 1,
            item: "a.txt".into(),
            failure: "agent".into(),
            error: "boom".into(),
            text: "Run the tests first.".into(),
            created_at: 0,
            status: SuggestionStatus::Evaluating,
            decided_at: None,
            remarks: vec![Remark {
                by: Author::User,
                text: "Which tests?".into(),
                at: 1,
            }],
            evaluation_error: None,
        };
        let prompt = comment_prompt("phase", &suggestion, "");
        assert!(prompt.contains("<suggestion>\nRun the tests first.\n</suggestion>"));
        assert!(prompt.contains("<user>Which tests?</user>"));
        assert!(prompt.contains("(none yet)"));
        assert!(prompt.contains("\"withdraw\""));
    }
}

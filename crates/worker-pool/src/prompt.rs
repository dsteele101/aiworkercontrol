//! The prompt an agent receives for one work item.

use std::path::{Path, PathBuf};

use work_queue::WorkItem;

use crate::{PoolError, Project};

/// Builds the prompt for `item`, whose text is `body`. Spec files are listed
/// by absolute path rather than inlined: some are far too large to send
/// whole, and the agent reads what it needs.
pub(crate) fn build(project: &Project, item: &WorkItem, body: &str) -> Result<String, PoolError> {
    let branch = Project::branch(item);
    let base = &project.base_ref;
    let mut prompt = format!(
        "You are an autonomous coding agent working on the project \"{name}\".\n\
         Carry out this work item ({item}):\n\n\
         <work-item>\n{body}\n</work-item>\n\n",
        name = project.name,
        body = body.trim(),
    );

    prompt.push_str(&specs_section(project)?);
    prompt.push_str(&format!(
        "Also follow this repository's own agent instructions and contribution \
         rules (CLAUDE.md, AGENTS.md, GEMINI.md or similar) wherever they apply.\n\n\
         You are in a dedicated git worktree on branch `{branch}`, cut from `{base}`.\n\
         - Implement the work item completely, with tests, in the repository's style.\n\
         - Run the repository's own checks for what you changed. `CARGO_TARGET_DIR` \
           points at a build cache kept between your runs; leave it as it is.\n\
         - When the spec files ask you to record notes in a spec file, edit that \
           file in place in the spec directory.\n\
         - Commit your work on `{branch}` using the repository's commit message \
           conventions. Do not push, and do not switch branches.\n\
         - Finish with a short summary of what you changed and how you verified it.\n\
         {WAITING}"
    ));
    if let Some(pr) = &project.pr {
        prompt.push_str(&format!(
            "\nWhen your work is complete and committed, write the pull request for it \
             to {draft}: the title on the first line, then a blank line, then the \
             description. Follow the repository's pull request template and \
             contribution guidelines for both (e.g. .github/PULL_REQUEST_TEMPLATE.md, \
             CONTRIBUTING.md). The pull request will be opened against {repo} \
             `{base}` for you once you finish, so do not push or open it yourself.\n",
            draft = project.pr_draft_path(item).display(),
            repo = pr.repo,
            base = pr.base,
        ));
    }
    Ok(prompt)
}

/// Builds the prompt for reworking `item` in its existing worktree after
/// its pull request at `pr_url` received `feedback` on its latest commit.
pub(crate) fn build_rework(
    project: &Project,
    item: &WorkItem,
    body: &str,
    pr_url: &str,
    feedback: &str,
) -> Result<String, PoolError> {
    let branch = Project::branch(item);
    let mut prompt = format!(
        "You are an autonomous coding agent continuing work on the project \"{name}\".\n\
         You earlier carried out this work item ({item}):\n\n\
         <work-item>\n{body}\n</work-item>\n\n\
         That work is on branch `{branch}` in this worktree, proposed in the pull \
         request {pr_url}. Its latest commit received this feedback:\n\n\
         <review-feedback>\n{feedback}\n</review-feedback>\n\n",
        name = project.name,
        body = body.trim(),
        feedback = feedback.trim(),
    );
    prompt.push_str(&specs_section(project)?);
    prompt.push_str(&format!(
        "Also follow this repository's own agent instructions and contribution \
         rules (CLAUDE.md, AGENTS.md, GEMINI.md or similar) wherever they apply.\n\n\
         - Address each point of the feedback that is correct, and fix failing CI. \
           If you disagree with a point, leave the code as it is and say why in \
           your summary.\n\
         - Run the repository's own checks for what you changed. `CARGO_TARGET_DIR` \
           points at a build cache kept between your runs; leave it as it is.\n\
         - Commit the changes on `{branch}` as new commits, using the repository's \
           commit message conventions; do not rewrite or squash the existing ones. \
           Do not push, and do not switch branches: the pull request is updated \
           for you once you finish.\n\
         - Finish with a short summary of how you handled each point.\n\
         {WAITING}"
    ));
    Ok(prompt)
}

/// How to wait for long-running commands without hanging the run.
const WAITING: &str = "- When you start a command in the background, rely on your tool's \
     notification that it finished, or wait with a bounded loop (e.g. under \
     `timeout`). Never poll with `pgrep -f` or `pkill -f` for text from your own \
     command line: the pattern matches the polling shell itself, so the loop never \
     ends. Nothing must still be running when you give your summary.\n";

/// The list of the project's spec files, by absolute path.
fn specs_section(project: &Project) -> Result<String, PoolError> {
    let specs = spec_files(&project.spec_dir)?;
    if specs.is_empty() {
        return Ok(String::new());
    }
    let mut text = String::from(
        "The project's spec files give direction for this work. Read the ones \
         relevant to the task and follow them:\n",
    );
    for spec in specs {
        text.push_str(&format!("- {}\n", spec.display()));
    }
    text.push('\n');
    Ok(text)
}

/// The non-hidden files directly inside `dir`, sorted, as absolute paths.
fn spec_files(dir: &Path) -> Result<Vec<PathBuf>, PoolError> {
    let dir = std::path::absolute(dir).map_err(PoolError::io("resolve", dir))?;
    let entries = std::fs::read_dir(&dir).map_err(PoolError::io("list", &dir))?;
    let mut files = Vec::new();
    for entry in entries {
        let entry = entry.map_err(PoolError::io("list", &dir))?;
        let hidden = entry.file_name().to_string_lossy().starts_with('.');
        if !hidden && entry.path().is_file() {
            files.push(entry.path());
        }
    }
    files.sort();
    Ok(files)
}

#[cfg(test)]
mod tests {
    // Test code asserts rather than propagating; see Rust code review.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]

    use super::*;

    #[test]
    fn lists_specs_and_names_branch() {
        let spec = tempfile::tempdir().unwrap();
        std::fs::write(spec.path().join("B.md"), "").unwrap();
        std::fs::write(spec.path().join("A.txt"), "").unwrap();
        std::fs::write(spec.path().join(".hidden"), "").unwrap();
        let project = Project::new("phase", spec.path(), "/dev/phase");
        let item = WorkItem::new("phase", "timesifter.txt");

        let prompt = build(&project, &item, "  No extra turns.\n").unwrap();

        assert!(prompt.contains("<work-item>\nNo extra turns.\n</work-item>"));
        let a = prompt.find("A.txt").unwrap();
        let b = prompt.find("B.md").unwrap();
        assert!(a < b);
        assert!(!prompt.contains(".hidden"));
        assert!(prompt.contains("branch `ai/timesifter`, cut from `main`"));
    }

    #[test]
    fn asks_for_a_pr_draft_only_when_publishing() {
        let spec = tempfile::tempdir().unwrap();
        let mut project = Project::new("phase", spec.path(), "/dev/phase");
        let item = WorkItem::new("phase", "timesifter.txt");
        assert!(
            !build(&project, &item, "x")
                .unwrap()
                .contains("pull request")
        );

        project.pr = Some(crate::PullRequests {
            push_remote: "fork".into(),
            repo: "phase-rs/phase".into(),
            base: "main".into(),
            draft: false,
        });
        let prompt = build(&project, &item, "x").unwrap();
        assert!(prompt.contains("to /dev/phase-worktrees/.pr/timesifter.md: the title"));
        assert!(prompt.contains("against phase-rs/phase `main`"));
        assert!(prompt.contains("do not push or open it yourself"));
    }

    #[test]
    fn rework_prompt_carries_the_feedback() {
        let spec = tempfile::tempdir().unwrap();
        std::fs::write(spec.path().join("SPEC.md"), "").unwrap();
        let project = Project::new("phase", spec.path(), "/dev/phase");
        let item = WorkItem::new("phase", "timesifter.txt");
        let prompt = build_rework(
            &project,
            &item,
            "No extra turns.",
            "https://github.com/up/repo/pull/7",
            "### Review by maint (CHANGES_REQUESTED)\n\nRename it.",
        )
        .unwrap();
        assert!(prompt.contains("<work-item>\nNo extra turns.\n</work-item>"));
        assert!(prompt.contains("pull request https://github.com/up/repo/pull/7"));
        assert!(prompt.contains("<review-feedback>\n### Review by maint"));
        assert!(prompt.contains("SPEC.md"));
        assert!(prompt.contains("do not rewrite or squash the existing ones"));
    }

    #[test]
    fn missing_spec_dir_is_an_error() {
        let project = Project::new("p", "/nonexistent/spec", "/dev/p");
        let err = build(&project, &WorkItem::new("p", "a.txt"), "x").unwrap_err();
        assert!(matches!(err, PoolError::Io { op: "list", .. }));
    }
}

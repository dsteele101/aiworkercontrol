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

    let specs = spec_files(&project.spec_dir)?;
    if !specs.is_empty() {
        prompt.push_str(
            "The project's spec files give direction for this work. Read the ones \
             relevant to the task and follow them:\n",
        );
        for spec in specs {
            prompt.push_str(&format!("- {}\n", spec.display()));
        }
        prompt.push('\n');
    }

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
         - Finish with a short summary of what you changed and how you verified it.\n"
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
    fn missing_spec_dir_is_an_error() {
        let project = Project::new("p", "/nonexistent/spec", "/dev/p");
        let err = build(&project, &WorkItem::new("p", "a.txt"), "x").unwrap_err();
        assert!(matches!(err, PoolError::Io { op: "list", .. }));
    }
}

//! A project's saved suggestions and the history of its notes file.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{NotesVersion, Suggestion, SuggestionStatus};
use crate::PoolError;

/// The notes file's name, in the project's spec directory: the lessons the
/// user accepted, which every run is pointed at.
pub const NOTES_FILE: &str = "AGENT-NOTES.md";

const HEADER: &str = "# Agent notes\n\n\
    Lessons from earlier runs on this project that went wrong. Follow them.\n\n";

/// Everything saved for one project: its suggestions and notes versions.
#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) struct Notes {
    /// The number the next suggestion gets.
    next_id: u32,
    pub suggestions: Vec<Suggestion>,
    pub versions: Vec<NotesVersion>,
}

fn file(dir: &Path, project: &str) -> PathBuf {
    dir.join(format!("{project}.json"))
}

impl Notes {
    /// The saved notes of `project`, kept in `dir`; empty if none are.
    pub fn load(dir: &Path, project: &str) -> Result<Self, PoolError> {
        let path = file(dir, project);
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                serde_json::from_str(&text).map_err(|source| PoolError::BadMeta { path, source })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(PoolError::io("read", path)(e)),
        }
    }

    /// Saves them, replacing the file in one step.
    pub fn save(&self, dir: &Path, project: &str) -> Result<(), PoolError> {
        std::fs::create_dir_all(dir).map_err(PoolError::io("create", dir))?;
        let path = file(dir, project);
        let temp = path.with_extension("json.tmp");
        // Serializing these plain structs cannot fail.
        let text = serde_json::to_string_pretty(self).unwrap_or_default();
        std::fs::write(&temp, text).map_err(PoolError::io("write", &temp))?;
        std::fs::rename(&temp, &path).map_err(PoolError::io("replace", path))
    }

    /// Adds a pending suggestion and returns its number.
    pub fn add(&mut self, item: &str, failure: &str, error: &str, text: &str, now: u64) -> u32 {
        self.next_id += 1;
        self.suggestions.push(Suggestion {
            id: self.next_id,
            item: item.to_string(),
            failure: failure.to_string(),
            error: error.to_string(),
            text: text.trim().to_string(),
            created_at: now,
            status: SuggestionStatus::Pending,
            decided_at: None,
            remarks: Vec::new(),
            evaluation_error: None,
        });
        self.next_id
    }

    pub fn find_mut(&mut self, id: u32) -> Result<&mut Suggestion, PoolError> {
        self.suggestions
            .iter_mut()
            .find(|s| s.id == id)
            .ok_or(PoolError::NoSuggestion(id))
    }

    /// Forgets the suggestions rejected `retention_secs` ago or earlier.
    /// Returns whether any went.
    pub fn purge_rejected(&mut self, retention_secs: u64, now: u64) -> bool {
        let before = self.suggestions.len();
        self.suggestions.retain(|s| {
            s.status != SuggestionStatus::Rejected
                || s.decided_at
                    .is_none_or(|at| at.saturating_add(retention_secs) > now)
        });
        self.suggestions.len() != before
    }

    /// Adds `text` to the notes file at `path` as a new entry, recording the
    /// file's new version as `note`.
    pub fn append(
        &mut self,
        path: &Path,
        text: &str,
        note: String,
        now: u64,
    ) -> Result<(), PoolError> {
        let mut content = self.settle(path, now)?;
        if content.trim().is_empty() {
            content = HEADER.to_string();
        } else if !content.ends_with('\n') {
            content.push('\n');
        }
        let mut lines = text.trim().lines();
        content.push_str("- ");
        content.push_str(lines.next().unwrap_or_default());
        content.push('\n');
        for line in lines {
            if !line.trim().is_empty() {
                content.push_str("  ");
                content.push_str(line);
            }
            content.push('\n');
        }
        self.write(path, content, note, now)
    }

    /// Puts the notes file at `path` back as version `version` had it. The
    /// reverting is itself a new version, so nothing is lost.
    pub fn revert(&mut self, path: &Path, version: u32, now: u64) -> Result<(), PoolError> {
        let content = self
            .versions
            .iter()
            .find(|v| v.id == version)
            .map(|v| v.content.clone())
            .ok_or(PoolError::NoNotesVersion(version))?;
        self.settle(path, now)?;
        self.write(path, content, format!("reverted to version {version}"), now)
    }

    /// The file's current text, after making sure the history holds it: a
    /// file just edited by hand, or not yet tracked, becomes a version of its
    /// own, so changing it here never loses what was there.
    fn settle(&mut self, path: &Path, now: u64) -> Result<String, PoolError> {
        let content = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(PoolError::io("read", path)(e)),
        };
        match self.versions.last() {
            None => self.push_version(now, "before the first suggestion", content.clone()),
            Some(last) if last.content != content => {
                self.push_version(now, "edited outside the app", content.clone());
            }
            Some(_) => {}
        }
        Ok(content)
    }

    fn write(
        &mut self,
        path: &Path,
        content: String,
        note: String,
        now: u64,
    ) -> Result<(), PoolError> {
        std::fs::write(path, &content).map_err(PoolError::io("write", path))?;
        self.push_version(now, &note, content);
        Ok(())
    }

    fn push_version(&mut self, at: u64, note: &str, content: String) {
        let id = self.versions.last().map_or(1, |v| v.id + 1);
        self.versions.push(NotesVersion {
            id,
            at,
            note: note.to_string(),
            content,
        });
    }
}

#[cfg(test)]
mod tests {
    // Test code asserts rather than propagating; see Rust code review.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]

    use super::*;

    #[test]
    fn appends_entries_and_tracks_versions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(NOTES_FILE);
        let mut notes = Notes::default();

        notes
            .append(&path, "Run the tests.\nAll of them.", "first".into(), 10)
            .unwrap();
        notes
            .append(&path, "Commit early.", "second".into(), 20)
            .unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# Agent notes"));
        assert!(text.contains("- Run the tests.\n  All of them.\n- Commit early.\n"));
        let notes_seen: Vec<_> = notes.versions.iter().map(|v| v.note.as_str()).collect();
        assert_eq!(
            notes_seen,
            ["before the first suggestion", "first", "second"]
        );
    }

    #[test]
    fn keeps_a_hand_edit_as_its_own_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(NOTES_FILE);
        let mut notes = Notes::default();
        notes.append(&path, "One.", "first".into(), 10).unwrap();
        std::fs::write(&path, "# mine\n").unwrap();

        notes.append(&path, "Two.", "second".into(), 20).unwrap();

        let outside = notes
            .versions
            .iter()
            .find(|v| v.note == "edited outside the app")
            .unwrap();
        assert_eq!(outside.content, "# mine\n");
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .ends_with("# mine\n- Two.\n")
        );
    }

    #[test]
    fn reverts_to_an_earlier_version_and_records_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(NOTES_FILE);
        let mut notes = Notes::default();
        notes.append(&path, "One.", "first".into(), 10).unwrap();
        let after_first = std::fs::read_to_string(&path).unwrap();
        let first = notes.versions.last().unwrap().id;
        notes.append(&path, "Two.", "second".into(), 20).unwrap();

        notes.revert(&path, first, 30).unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), after_first);
        assert_eq!(
            notes.versions.last().unwrap().note,
            format!("reverted to version {first}")
        );
        let missing = notes.revert(&path, 99, 40).unwrap_err();
        assert!(matches!(missing, PoolError::NoNotesVersion(99)));
    }

    #[test]
    fn purges_only_expired_rejections() {
        let mut notes = Notes::default();
        let old = notes.add("a.txt", "agent", "e", "Old.", 0);
        let recent = notes.add("a.txt", "agent", "e", "Recent.", 0);
        let pending = notes.add("a.txt", "agent", "e", "Pending.", 0);
        for (id, at) in [(old, 100), (recent, 900)] {
            let s = notes.find_mut(id).unwrap();
            s.status = SuggestionStatus::Rejected;
            s.decided_at = Some(at);
        }

        assert!(notes.purge_rejected(500, 1000));

        let left: Vec<_> = notes.suggestions.iter().map(|s| s.id).collect();
        assert_eq!(left, [recent, pending]);
        assert!(!notes.purge_rejected(500, 1000));
    }

    #[test]
    fn saves_and_loads() {
        let dir = tempfile::tempdir().unwrap();
        let mut notes = Notes::default();
        let id = notes.add("a.txt", "agent", "boom", "  Do X.\n", 5);
        notes.save(dir.path(), "phase").unwrap();

        let mut loaded = Notes::load(dir.path(), "phase").unwrap();

        assert_eq!(loaded.find_mut(id).unwrap().text, "Do X.");
        assert!(
            Notes::load(dir.path(), "other")
                .unwrap()
                .suggestions
                .is_empty()
        );
    }
}

//! The queue itself: listing items and moving them between states.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use crate::{QueueError, State, WorkItem};

/// Suffix of an item's result file, a report of its latest run.
pub const RESULT_SUFFIX: &str = ".result.md";
/// Suffix of an item's metadata file, kept by whoever runs the queue.
pub const META_SUFFIX: &str = ".meta.json";
/// Files that sit beside an item and move with it.
const SIDECARS: [&str; 2] = [RESULT_SUFFIX, META_SUFFIX];

/// A work queue rooted at a directory holding `open/`, `inprogress/` and `closed/`.
#[derive(Debug, Clone)]
pub struct Queue {
    root: PathBuf,
}

impl Queue {
    /// Creates a queue rooted at `root`. Nothing is touched on disk until an
    /// operation runs; missing state directories read as empty.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The queue's root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The path `item` has, or would have, in `state`.
    ///
    /// ```
    /// use work_queue::{Queue, State, WorkItem};
    ///
    /// let queue = Queue::new("/work");
    /// let item = WorkItem::new("phase", "fix.txt");
    /// assert_eq!(queue.path(State::Open, &item), std::path::Path::new("/work/open/phase/fix.txt"));
    /// ```
    pub fn path(&self, state: State, item: &WorkItem) -> PathBuf {
        self.root
            .join(state.dir_name())
            .join(&item.project)
            .join(&item.file)
    }

    /// The path of `item`'s sidecar file with `suffix` (such as
    /// [`RESULT_SUFFIX`]) while the item is in `state`.
    ///
    /// ```
    /// use work_queue::{Queue, State, WorkItem, META_SUFFIX};
    ///
    /// let queue = Queue::new("/work");
    /// let item = WorkItem::new("phase", "fix.txt");
    /// assert_eq!(
    ///     queue.sidecar_path(State::Hold, &item, META_SUFFIX),
    ///     std::path::Path::new("/work/hold/phase/fix.meta.json"),
    /// );
    /// ```
    pub fn sidecar_path(&self, state: State, item: &WorkItem, suffix: &str) -> PathBuf {
        self.path(state, item)
            .with_file_name(format!("{}{suffix}", item.stem()))
    }

    /// Lists the items in `state`, sorted by project then file name.
    ///
    /// Hidden files (such as `.gitkeep`) and sidecar files are not items.
    pub fn list(&self, state: State) -> Result<Vec<WorkItem>, QueueError> {
        let state_dir = self.root.join(state.dir_name());
        let mut items = Vec::new();
        for project in read_dir_names(&state_dir, true)? {
            for file in read_dir_names(&state_dir.join(&project), false)? {
                if !SIDECARS.iter().any(|s| file.ends_with(s)) {
                    items.push(WorkItem::new(project.clone(), file));
                }
            }
        }
        items.sort();
        Ok(items)
    }

    /// Reads the contents of `item` in `state`.
    pub fn read(&self, state: State, item: &WorkItem) -> Result<String, QueueError> {
        let path = self.path(state, item);
        fs::read_to_string(&path).map_err(QueueError::io("read", path))
    }

    /// Claims the first open item for which `skip` returns false, moving it to
    /// `inprogress`. Returns `None` when there is nothing left to claim.
    ///
    /// An item another worker claims first is passed over, so concurrent
    /// callers never receive the same item.
    pub fn claim_next(
        &self,
        skip: impl Fn(&WorkItem) -> bool,
    ) -> Result<Option<WorkItem>, QueueError> {
        for item in self.list(State::Open)? {
            if skip(&item) {
                continue;
            }
            match self.move_item(&item, State::Open, State::InProgress) {
                Ok(()) => return Ok(Some(item)),
                Err(QueueError::Io { source, .. }) if source.kind() == ErrorKind::NotFound => {
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
        Ok(None)
    }

    /// Closes an in-progress item, writing `result` beside it as
    /// `<stem>.result.md`.
    pub fn complete(&self, item: &WorkItem, result: &str) -> Result<(), QueueError> {
        self.write_result(State::InProgress, item, result)?;
        self.transition(item, State::InProgress, State::Closed)
    }

    /// Writes `item`'s result file while it is in `state`.
    pub fn write_result(
        &self,
        state: State,
        item: &WorkItem,
        result: &str,
    ) -> Result<(), QueueError> {
        let path = self.sidecar_path(state, item, RESULT_SUFFIX);
        ensure_parent(&path)?;
        fs::write(&path, result).map_err(QueueError::io("write", path))
    }

    /// Moves `item` and its sidecar files from `from` to `to`.
    pub fn transition(&self, item: &WorkItem, from: State, to: State) -> Result<(), QueueError> {
        self.move_item(item, from, to)
    }

    /// Moves `item` from `from` to `inprogress` if it is still in `from`.
    /// Returns false if another worker took it first.
    pub fn take(&self, item: &WorkItem, from: State) -> Result<bool, QueueError> {
        match self.move_item(item, from, State::InProgress) {
            Ok(()) => Ok(true),
            Err(QueueError::Io { source, .. }) if source.kind() == ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Returns an in-progress item to `open` so it can be claimed again.
    pub fn release(&self, item: &WorkItem) -> Result<(), QueueError> {
        self.move_item(item, State::InProgress, State::Open)
    }

    /// Returns every in-progress item to `open`. Run at startup, when no
    /// worker can still own one.
    pub fn recover_orphans(&self) -> Result<Vec<WorkItem>, QueueError> {
        let orphans = self.list(State::InProgress)?;
        for item in &orphans {
            self.release(item)?;
        }
        Ok(orphans)
    }

    /// Moves the item file, then its sidecars. The item file goes first: its
    /// rename is what claims the item, so a lost race moves nothing.
    fn move_item(&self, item: &WorkItem, from: State, to: State) -> Result<(), QueueError> {
        let src = self.path(from, item);
        let dst = self.path(to, item);
        ensure_parent(&dst)?;
        fs::rename(&src, &dst).map_err(QueueError::io("move", src))?;
        for suffix in SIDECARS {
            let src = self.sidecar_path(from, item, suffix);
            match fs::rename(&src, self.sidecar_path(to, item, suffix)) {
                Err(e) if e.kind() != ErrorKind::NotFound => {
                    return Err(QueueError::io("move", src)(e));
                }
                _ => {}
            }
        }
        Ok(())
    }
}

fn ensure_parent(path: &Path) -> Result<(), QueueError> {
    match path.parent() {
        Some(parent) => fs::create_dir_all(parent).map_err(QueueError::io("create", parent)),
        None => Ok(()),
    }
}

/// Names of the non-hidden entries in `dir` that are directories (`dirs`) or
/// files (`!dirs`). A missing `dir` has no entries.
fn read_dir_names(dir: &Path, dirs: bool) -> Result<Vec<String>, QueueError> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(QueueError::io("list", dir)(e)),
    };
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(QueueError::io("list", dir))?;
        let file_type = entry
            .file_type()
            .map_err(QueueError::io("stat", entry.path()))?;
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if !name.starts_with('.') && file_type.is_dir() == dirs {
            names.push(name);
        }
    }
    Ok(names)
}

#[cfg(test)]
mod tests {
    // Test code asserts rather than propagating; see Rust code review.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]

    use super::*;

    fn queue_with(files: &[&str]) -> (tempfile::TempDir, Queue) {
        let dir = tempfile::tempdir().unwrap();
        for f in files {
            let path = dir.path().join(f);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "body").unwrap();
        }
        let queue = Queue::new(dir.path());
        (dir, queue)
    }

    #[test]
    fn list_skips_hidden_files_and_results() {
        let (_dir, queue) = queue_with(&[
            "closed/phase/.gitkeep",
            "closed/phase/a.txt",
            "closed/phase/a.result.md",
        ]);
        assert_eq!(
            queue.list(State::Closed).unwrap(),
            vec![WorkItem::new("phase", "a.txt")]
        );
    }

    #[test]
    fn list_of_missing_state_is_empty() {
        let (_dir, queue) = queue_with(&[]);
        assert!(queue.list(State::Open).unwrap().is_empty());
    }

    #[test]
    fn claim_next_honors_skip() {
        let (_dir, queue) = queue_with(&["open/p/a.txt", "open/p/b.txt"]);
        let item = queue.claim_next(|i| i.file == "a.txt").unwrap();
        assert_eq!(item, Some(WorkItem::new("p", "b.txt")));
    }

    #[test]
    fn complete_writes_result_beside_item() {
        let (dir, queue) = queue_with(&["inprogress/p/a.txt"]);
        let item = WorkItem::new("p", "a.txt");
        queue.complete(&item, "summary").unwrap();
        let result = fs::read_to_string(dir.path().join("closed/p/a.result.md")).unwrap();
        assert_eq!(result, "summary");
        assert!(queue.list(State::InProgress).unwrap().is_empty());
    }

    #[test]
    fn release_of_unclaimed_item_fails() {
        let (_dir, queue) = queue_with(&[]);
        let err = queue.release(&WorkItem::new("p", "a.txt")).unwrap_err();
        assert!(matches!(err, QueueError::Io { op: "move", .. }));
    }
}

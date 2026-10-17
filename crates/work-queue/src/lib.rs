//! File-based work queue.
//!
//! Work items are plain files laid out as `<root>/<state>/<project>/<file>`,
//! where `<state>` is `open`, `inprogress`, `inreview`, `closed` or `hold`.
//! An item's sidecar files (`<stem>.result.md`, `<stem>.meta.json`) sit
//! beside it and move with it. Moving an item between
//! states is a single `rename`, which is atomic on one filesystem, so several
//! workers can claim from the same queue without ever claiming the same item.
//!
//! ```
//! use work_queue::{Queue, State};
//!
//! let dir = tempfile::tempdir()?;
//! std::fs::create_dir_all(dir.path().join("open/phase"))?;
//! std::fs::write(dir.path().join("open/phase/fix.txt"), "Fix the bug")?;
//!
//! let queue = Queue::new(dir.path());
//! let item = queue.claim_next(|_| false)?.expect("one open item");
//! assert_eq!(queue.read(State::InProgress, &item)?, "Fix the bug");
//!
//! queue.complete(&item, "Done on branch ai/fix")?;
//! assert_eq!(queue.list(State::Closed)?, vec![item]);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

mod error;
mod item;
mod queue;

pub use error::QueueError;
pub use item::{State, WorkItem};
pub use queue::{META_SUFFIX, Queue, RESULT_SUFFIX};

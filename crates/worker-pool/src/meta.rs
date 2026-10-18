//! Per-item bookkeeping, kept in the item's `.meta.json` sidecar so it
//! survives restarts and moves with the item between states.

use agent_backend::Provider;
use serde::{Deserialize, Serialize};
use work_queue::{META_SUFFIX, Queue, State, WorkItem};

use crate::PoolError;

/// What the pool remembers about one item.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemMeta {
    /// Agent runs on the item since it was created or last returned from
    /// hold. Every run counts: the first, each rework, each failure.
    #[serde(default)]
    pub turns: u32,
    /// Why the latest run failed, if it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// Why the item was put on hold, while it is there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hold_reason: Option<String>,
    /// The pull request proposing the item's work, once opened.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_url: Option<String>,
    /// The commit last pushed to the pull request. Feedback counts only when
    /// it is on this commit, i.e. given since the last push.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pushed_head: Option<String>,
    /// Feedback waiting to be addressed by a rework run. While set, the item
    /// waits in review for a free worker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub needs_rework: Option<String>,
    /// The pushed commit whose feedback was last handed to a rework, so the
    /// same feedback is not acted on twice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answered_head: Option<String>,
    /// The provider whose run last pushed to the pull request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<Provider>,
}

impl ItemMeta {
    /// Reads `item`'s metadata in `state`; an item without any has the
    /// default (no turns yet).
    pub(crate) fn load(queue: &Queue, state: State, item: &WorkItem) -> Result<Self, PoolError> {
        let path = queue.sidecar_path(state, item, META_SUFFIX);
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                serde_json::from_str(&text).map_err(|source| PoolError::BadMeta { path, source })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(PoolError::io("read", path)(e)),
        }
    }

    /// Writes `item`'s metadata in `state`.
    pub(crate) fn save(
        &self,
        queue: &Queue,
        state: State,
        item: &WorkItem,
    ) -> Result<(), PoolError> {
        let path = queue.sidecar_path(state, item, META_SUFFIX);
        // Serializing this plain struct cannot fail.
        let text = serde_json::to_string_pretty(self).unwrap_or_default();
        std::fs::write(&path, text).map_err(PoolError::io("write", path))
    }
}

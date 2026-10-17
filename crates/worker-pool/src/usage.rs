//! Each provider's latest reported plan usage, kept in `usage.json` in the
//! work directory so a restart does not forget how close a plan is to its
//! limits.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use agent_backend::{PlanUsage, Provider};
use work_queue::Queue;

use crate::PoolError;

/// The file's name, in the queue's root.
const USAGE_FILE: &str = "usage.json";

/// One provider's latest reported usage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderUsage {
    /// The provider.
    pub provider: Provider,
    /// Its usage as last reported.
    pub usage: PlanUsage,
}

pub(crate) type UsageMap = HashMap<Provider, PlanUsage>;

pub(crate) fn path(queue: &Queue) -> PathBuf {
    queue.root().join(USAGE_FILE)
}

/// The recorded usage. A missing or unreadable file means nothing has been
/// reported yet; the next run's reports fill it in again.
pub(crate) fn load(path: &Path) -> UsageMap {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

pub(crate) fn save(path: &Path, usage: &UsageMap) -> Result<(), PoolError> {
    // Serializing a map of plain structs keyed by strings cannot fail.
    let text = serde_json::to_string_pretty(usage).unwrap_or_default();
    std::fs::write(path, text).map_err(PoolError::io("write", path.to_path_buf()))
}

/// The recorded usage, in [`Provider::ALL`] order.
pub(crate) fn listed(usage: &UsageMap) -> Vec<ProviderUsage> {
    Provider::ALL
        .iter()
        .filter_map(|p| {
            usage.get(p).map(|u| ProviderUsage {
                provider: *p,
                usage: u.clone(),
            })
        })
        .collect()
}

//! How much of a plan's allowance has been used, as a CLI reports it.

use serde::{Deserialize, Serialize};

/// A plan's usage in its rate-limit windows. A window the CLI did not report
/// is `None`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanUsage {
    /// The short window currently running, such as Claude's five hours.
    pub session: Option<UsageWindow>,
    /// The longer allowance, usually a week.
    pub weekly: Option<UsageWindow>,
}

/// One rate-limit window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageWindow {
    /// Percent of the window's allowance used, 0 to 100.
    pub percent: u8,
    /// When the window resets, in seconds since the Unix epoch, if known.
    pub resets_at: Option<u64>,
}

impl UsageWindow {
    /// The percent used as of `now` (Unix seconds): 0 once the window has
    /// reset.
    ///
    /// ```
    /// use agent_backend::UsageWindow;
    ///
    /// let window = UsageWindow { percent: 64, resets_at: Some(1_000) };
    /// assert_eq!(window.percent_at(999), 64);
    /// assert_eq!(window.percent_at(1_000), 0);
    /// ```
    pub fn percent_at(&self, now: u64) -> u8 {
        match self.resets_at {
            Some(reset) if reset <= now => 0,
            _ => self.percent,
        }
    }
}

impl PlanUsage {
    /// The highest percent used in any window as of `now` (Unix seconds).
    pub fn highest_at(&self, now: u64) -> u8 {
        [self.session, self.weekly]
            .iter()
            .flatten()
            .map(|w| w.percent_at(now))
            .max()
            .unwrap_or(0)
    }
}

//! Plan-usage levels: when to warn, and when to stop starting runs on a
//! provider.

use serde::{Deserialize, Serialize};

/// Plan-usage levels, in percent of a window's allowance. They apply to
/// each window a provider reports, so the fullest window decides.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageLimits {
    /// Levels at which the dashboard warns, ascending.
    #[serde(default = "default_warn")]
    pub warn: Vec<u8>,
    /// The level at which no new run starts on a provider, unless the user
    /// overrides it.
    #[serde(default = "default_stop")]
    pub stop: u8,
    /// Whether to move on to the next fallback provider at the stop level,
    /// rather than pausing the workers.
    #[serde(default = "default_failover")]
    pub failover: bool,
}

fn default_warn() -> Vec<u8> {
    vec![50, 75]
}

fn default_stop() -> u8 {
    85
}

fn default_failover() -> bool {
    true
}

impl Default for UsageLimits {
    fn default() -> Self {
        Self {
            warn: default_warn(),
            stop: default_stop(),
            failover: default_failover(),
        }
    }
}

impl UsageLimits {
    /// The highest warn level `percent` has reached, if any.
    ///
    /// ```
    /// use worker_pool::UsageLimits;
    ///
    /// let limits = UsageLimits::default();
    /// assert_eq!(limits.warning(49), None);
    /// assert_eq!(limits.warning(80), Some(75));
    /// ```
    pub fn warning(&self, percent: u8) -> Option<u8> {
        self.warn.iter().copied().filter(|w| percent >= *w).max()
    }

    /// Whether `percent` has reached the stop level.
    pub fn stopped(&self, percent: u8) -> bool {
        percent >= self.stop
    }
}

#[cfg(test)]
mod tests {
    // Test code asserts rather than propagating; see Rust code review.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]

    use super::*;

    #[test]
    fn warnings_pick_the_highest_level_reached() {
        let limits = UsageLimits::default();
        assert_eq!(limits.warning(50), Some(50));
        assert_eq!(limits.warning(75), Some(75));
        assert!(!limits.stopped(84));
        assert!(limits.stopped(85));
    }
}

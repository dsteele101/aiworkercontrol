//! Plan-usage levels: when to warn, and when to stop starting runs on a
//! provider.

use agent_backend::PlanUsage;
use serde::{Deserialize, Serialize};

/// Which of a provider's usage windows a level applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Span {
    /// The current (session) window.
    Window,
    /// The week.
    Week,
}

impl Span {
    /// How the window reads in a sentence.
    pub fn label(self) -> &'static str {
        match self {
            Self::Window => "current window",
            Self::Week => "week",
        }
    }
}

/// The first window of a plan that is past its stop level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StopHit {
    /// The window that is past its stop level.
    pub span: Span,
    /// Percent of the window used.
    pub percent: u8,
    /// The stop level it reached.
    pub level: u8,
}

/// Plan-usage levels, in percent of a window's allowance. The current
/// window and the week each have their own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageLimits {
    /// Levels at which the dashboard warns about the current window,
    /// ascending.
    #[serde(default = "default_warn")]
    pub warn: Vec<u8>,
    /// The level at which no new run starts on a provider, and runs already
    /// going on it are stopped, unless the user overrides it, for the
    /// current window.
    #[serde(default = "default_stop")]
    pub stop: u8,
    /// Levels at which the dashboard warns about the week, ascending.
    #[serde(default = "default_warn")]
    pub week_warn: Vec<u8>,
    /// The stop level for the week.
    #[serde(default = "default_stop")]
    pub week_stop: u8,
    /// Whether to move on to the next fallback provider at a stop level,
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
            week_warn: default_warn(),
            week_stop: default_stop(),
            failover: default_failover(),
        }
    }
}

impl UsageLimits {
    /// The stop level for `span`.
    pub fn stop_at(&self, span: Span) -> u8 {
        match span {
            Span::Window => self.stop,
            Span::Week => self.week_stop,
        }
    }

    /// The highest warn level `percent` has reached in `span`, if any.
    ///
    /// ```
    /// use worker_pool::{Span, UsageLimits};
    ///
    /// let limits = UsageLimits::default();
    /// assert_eq!(limits.warning(Span::Window, 49), None);
    /// assert_eq!(limits.warning(Span::Week, 80), Some(75));
    /// ```
    pub fn warning(&self, span: Span, percent: u8) -> Option<u8> {
        let levels = match span {
            Span::Window => &self.warn,
            Span::Week => &self.week_warn,
        };
        levels.iter().copied().filter(|w| percent >= *w).max()
    }

    /// Whether `percent` has reached the stop level of `span`.
    pub fn stopped(&self, span: Span, percent: u8) -> bool {
        percent >= self.stop_at(span)
    }

    /// The first window of `usage` past its stop level as of `now` (Unix
    /// seconds), the current window before the week.
    pub fn stop_hit(&self, usage: &PlanUsage, now: u64) -> Option<StopHit> {
        [(Span::Window, usage.session), (Span::Week, usage.weekly)]
            .into_iter()
            .find_map(|(span, window)| {
                let percent = window?.percent_at(now);
                self.stopped(span, percent).then(|| StopHit {
                    span,
                    percent,
                    level: self.stop_at(span),
                })
            })
    }

    /// The highest warn level reached in either window of `usage`, if any.
    pub fn plan_warning(&self, usage: &PlanUsage, now: u64) -> Option<u8> {
        [(Span::Window, usage.session), (Span::Week, usage.weekly)]
            .into_iter()
            .filter_map(|(span, window)| self.warning(span, window?.percent_at(now)))
            .max()
    }
}

#[cfg(test)]
mod tests {
    // Test code asserts rather than propagating; see Rust code review.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]

    use agent_backend::UsageWindow;

    use super::*;

    fn window(percent: u8) -> Option<UsageWindow> {
        Some(UsageWindow {
            percent,
            resets_at: None,
        })
    }

    #[test]
    fn warnings_pick_the_highest_level_reached() {
        let limits = UsageLimits::default();
        assert_eq!(limits.warning(Span::Window, 50), Some(50));
        assert_eq!(limits.warning(Span::Window, 75), Some(75));
        assert!(!limits.stopped(Span::Window, 84));
        assert!(limits.stopped(Span::Window, 85));
    }

    #[test]
    fn the_window_and_the_week_have_their_own_levels() {
        let limits = UsageLimits {
            week_warn: vec![30],
            week_stop: 60,
            ..UsageLimits::default()
        };
        let usage = PlanUsage {
            session: window(40),
            weekly: window(60),
        };
        let hit = limits.stop_hit(&usage, 0).unwrap();
        assert_eq!((hit.span, hit.percent, hit.level), (Span::Week, 60, 60));
        assert_eq!(limits.plan_warning(&usage, 0), Some(30));

        let usage = PlanUsage {
            session: window(40),
            weekly: window(59),
        };
        assert_eq!(limits.stop_hit(&usage, 0), None);
        let usage = PlanUsage {
            session: window(85),
            weekly: None,
        };
        assert_eq!(limits.stop_hit(&usage, 0).unwrap().span, Span::Window);
    }

    #[test]
    fn a_config_from_before_the_week_levels_still_loads() {
        let limits: UsageLimits = serde_json::from_str(r#"{"warn":[60],"stop":90}"#).unwrap();
        assert_eq!((limits.stop, limits.week_stop), (90, 85));
    }
}

//! Plan-usage levels: when to warn, when to stop starting runs on a
//! provider, and which provider to fail over to.

use std::collections::{HashMap, HashSet};

use agent_backend::{Provider, ProviderConfig};
use serde::{Deserialize, Serialize};

use crate::usage::UsageMap;

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

/// The provider the next run should use, and whether it is a fallback: the
/// active one, or with failover on, the first fallback not `suspended`,
/// unless usage has reached the stop level on it and the user has not
/// overridden that. An override lapses once the provider's usage drops back
/// below the stop level, i.e. its window resets. Suspensions lapse as soon
/// as the active provider can run again, so a fallback is retried the next
/// time it is needed. `Err` says why no provider can run.
pub(crate) fn choose(
    active: &ProviderConfig,
    fallbacks: &[ProviderConfig],
    limits: &UsageLimits,
    usage: &UsageMap,
    overrides: &mut HashSet<Provider>,
    suspended: &mut HashMap<Provider, String>,
    now: u64,
) -> Result<(ProviderConfig, bool), String> {
    let fallbacks = if limits.failover { fallbacks } else { &[] };
    let mut unavailable = Vec::new();
    for (i, candidate) in std::iter::once(active).chain(fallbacks).enumerate() {
        let provider = candidate.provider;
        let is_fallback = i > 0;
        if is_fallback && suspended.contains_key(&provider) {
            unavailable.push(format!("{} failed its last run", provider.label()));
            continue;
        }
        let used = usage.get(&provider).map_or(0, |u| u.highest_at(now));
        let usable = !limits.stopped(used);
        if usable {
            overrides.remove(&provider);
        }
        if usable || overrides.contains(&provider) {
            if !is_fallback {
                suspended.clear();
            }
            return Ok((candidate.clone(), is_fallback));
        }
        unavailable.push(format!(
            "{} at {used}% of its plan, past the {}% stop level",
            provider.label(),
            limits.stop
        ));
    }
    Err(format!("no provider can run: {}", unavailable.join("; ")))
}

#[cfg(test)]
mod tests {
    // Test code asserts rather than propagating; see Rust code review.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]

    use agent_backend::{Auth, PlanUsage, UsageWindow};

    use super::*;

    fn used(percent: u8) -> PlanUsage {
        PlanUsage {
            session: Some(UsageWindow {
                percent,
                resets_at: Some(1_000),
            }),
            weekly: None,
        }
    }

    fn config(provider: Provider) -> ProviderConfig {
        ProviderConfig::new(provider, Auth::Plan)
    }

    fn pick(
        limits: &UsageLimits,
        usage: &UsageMap,
        overrides: &mut HashSet<Provider>,
        now: u64,
    ) -> Result<Provider, String> {
        let fallbacks = [config(Provider::OpenAi), config(Provider::Gemini)];
        choose(
            &config(Provider::Anthropic),
            &fallbacks,
            limits,
            usage,
            overrides,
            &mut HashMap::new(),
            now,
        )
        .map(|(c, _)| c.provider)
    }

    #[test]
    fn fails_over_in_order_past_the_stop_level() {
        let limits = UsageLimits::default();
        let mut usage = UsageMap::from([(Provider::Anthropic, used(84))]);
        let mut overrides = HashSet::new();
        assert_eq!(
            pick(&limits, &usage, &mut overrides, 0),
            Ok(Provider::Anthropic)
        );
        usage.insert(Provider::Anthropic, used(85));
        assert_eq!(
            pick(&limits, &usage, &mut overrides, 0),
            Ok(Provider::OpenAi)
        );
        usage.insert(Provider::OpenAi, used(90));
        assert_eq!(
            pick(&limits, &usage, &mut overrides, 0),
            Ok(Provider::Gemini)
        );
        // Once the window resets, the active provider is back.
        assert_eq!(
            pick(&limits, &usage, &mut overrides, 1_000),
            Ok(Provider::Anthropic)
        );
    }

    #[test]
    fn without_failover_it_stops_until_overridden() {
        let limits = UsageLimits {
            failover: false,
            ..UsageLimits::default()
        };
        let usage = UsageMap::from([(Provider::Anthropic, used(87))]);
        let mut overrides = HashSet::new();
        assert_eq!(
            pick(&limits, &usage, &mut overrides, 0),
            Err(
                "no provider can run: Anthropic (Claude) at 87% of its plan, past the 85% stop level"
                    .into()
            )
        );
        overrides.insert(Provider::Anthropic);
        assert_eq!(
            pick(&limits, &usage, &mut overrides, 0),
            Ok(Provider::Anthropic)
        );
        // The override lapses when the window resets.
        assert_eq!(
            pick(&limits, &usage, &mut overrides, 1_000),
            Ok(Provider::Anthropic)
        );
        assert!(overrides.is_empty());
    }

    #[test]
    fn failed_fallbacks_are_skipped_until_the_active_provider_returns() {
        let limits = UsageLimits::default();
        let usage = UsageMap::from([(Provider::Anthropic, used(100))]);
        let fallbacks = [config(Provider::Gemini), config(Provider::OpenAi)];
        let mut overrides = HashSet::new();
        let mut suspended = HashMap::from([(Provider::Gemini, "login failed".to_string())]);
        let mut pick = |suspended: &mut HashMap<Provider, String>, now| {
            choose(
                &config(Provider::Anthropic),
                &fallbacks,
                &limits,
                &usage,
                &mut overrides,
                suspended,
                now,
            )
            .map(|(c, fallback)| (c.provider, fallback))
        };
        assert_eq!(pick(&mut suspended, 0), Ok((Provider::OpenAi, true)));

        suspended.insert(Provider::OpenAi, "quota".into());
        assert_eq!(
            pick(&mut suspended, 0),
            Err(
                "no provider can run: Anthropic (Claude) at 100% of its plan, past the 85% \
                 stop level; Gemini failed its last run; OpenAI failed its last run"
                    .into()
            )
        );
        // Once the active provider's window resets, it runs again and the
        // fallbacks get another chance the next time they are needed.
        assert_eq!(
            pick(&mut suspended, 1_000),
            Ok((Provider::Anthropic, false))
        );
        assert!(suspended.is_empty());
    }

    #[test]
    fn warnings_pick_the_highest_level_reached() {
        let limits = UsageLimits::default();
        assert_eq!(limits.warning(50), Some(50));
        assert_eq!(limits.warning(75), Some(75));
        assert!(!limits.stopped(84));
        assert!(limits.stopped(85));
    }
}

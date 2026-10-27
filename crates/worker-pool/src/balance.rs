//! Spreading runs across providers: which provider takes the next item.

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};

use agent_backend::{PlanUsage, Provider, ProviderConfig, UsageWindow};
use serde::{Deserialize, Serialize};

use crate::PoolConfig;
use crate::usage::UsageMap;

/// How long a provider stays set aside after a failed run, unless the
/// active provider runs again first under [`Scheme::Priority`].
pub(crate) const SUSPENSION_SECS: u64 = 30 * 60;
/// The length of a plan's current window, which both Claude and Antigravity
/// set at five hours.
const SESSION_SECS: u64 = 5 * 3_600;
/// The length of a plan's weekly window.
const WEEK_SECS: u64 = 7 * 86_400;

/// How the next item's provider is picked from the active provider and the
/// fallbacks, among those that may run: under their stop level (or
/// overridden) and not set aside after a failed run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scheme {
    /// The first in order: the active provider, then each fallback when
    /// the ones before it are stopped and failover is on.
    #[default]
    Priority,
    /// Each in turn.
    RoundRobin,
    /// In proportion to their weights, spread evenly.
    Weighted,
    /// The one furthest below its pace: the least used relative to how
    /// much of its plan window has passed.
    Pace,
}

impl Scheme {
    /// Every scheme, in display order.
    pub const ALL: [Scheme; 4] = [
        Scheme::Priority,
        Scheme::RoundRobin,
        Scheme::Weighted,
        Scheme::Pace,
    ];

    /// A human-readable name.
    pub fn label(self) -> &'static str {
        match self {
            Scheme::Priority => "Priority failover",
            Scheme::RoundRobin => "Round robin",
            Scheme::Weighted => "Weighted",
            Scheme::Pace => "Pace-based",
        }
    }
}

/// How work is spread across providers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Balancing {
    /// The scheme.
    #[serde(default)]
    pub scheme: Scheme,
    /// Each provider's share under [`Scheme::Weighted`]; 1 when unset.
    #[serde(default)]
    pub weights: HashMap<Provider, u32>,
    /// Whether a rework may run on any provider. When off, it waits for
    /// the provider that opened its pull request.
    #[serde(default = "default_rework_any_provider")]
    pub rework_any_provider: bool,
}

fn default_rework_any_provider() -> bool {
    true
}

impl Default for Balancing {
    fn default() -> Self {
        Self {
            scheme: Scheme::default(),
            weights: HashMap::new(),
            rework_any_provider: default_rework_any_provider(),
        }
    }
}

impl Balancing {
    /// `provider`'s weight, at least 1.
    pub fn weight(&self, provider: Provider) -> u32 {
        self.weights.get(&provider).copied().unwrap_or(1).max(1)
    }
}

/// A provider set aside after a failed run.
#[derive(Debug, Clone)]
pub(crate) struct Suspension {
    /// The run's error.
    pub error: String,
    /// When, in Unix seconds.
    pub since: u64,
}

/// A provider picked for a run, to [`Picker::commit`] once an item is
/// claimed for it.
#[derive(Debug, Clone)]
pub(crate) struct Pick {
    pub config: ProviderConfig,
    /// Whether it is a fallback rather than the active provider.
    pub fallback: bool,
    /// Its place in the candidates: the active provider, then fallbacks.
    index: usize,
    candidates: usize,
    /// The providers that could have run.
    available: Vec<Provider>,
}

/// The pool's memory for picking providers.
#[derive(Debug, Default)]
pub(crate) struct Picker {
    /// Providers the user lets run past the stop level until it resets.
    pub overrides: HashSet<Provider>,
    /// Providers set aside after a failed run.
    pub suspended: HashMap<Provider, Suspension>,
    /// Where round robin starts looking next.
    rotation: usize,
    /// Smooth weighted round robin's running credit per provider.
    credit: HashMap<Provider, i64>,
}

impl Picker {
    /// Picks the provider for the next run, or with `want`, checks that
    /// provider can run (when it is still configured). Overrides lapse once
    /// usage drops below the stop level, i.e. the window resets, and
    /// suspensions after [`SUSPENSION_SECS`]. `Err` says why nothing can
    /// run. Nothing is counted until [`Picker::commit`].
    pub fn choose(
        &mut self,
        config: &PoolConfig,
        usage: &UsageMap,
        now: u64,
        want: Option<Provider>,
    ) -> Result<Pick, String> {
        let (balancing, limits) = (&config.balancing, &config.limits);
        let fallbacks = if balancing.scheme != Scheme::Priority || limits.failover {
            config.fallbacks.as_slice()
        } else {
            &[]
        };
        let candidates: Vec<&ProviderConfig> =
            std::iter::once(&config.provider).chain(fallbacks).collect();
        self.suspended
            .retain(|_, s| now < s.since.saturating_add(SUSPENSION_SECS));

        let mut available = Vec::new();
        let mut unavailable = Vec::new();
        for (i, candidate) in candidates.iter().enumerate() {
            let provider = candidate.provider;
            if i > 0 && self.suspended.contains_key(&provider) {
                unavailable.push(format!("{} failed its last run", provider.label()));
                continue;
            }
            let hit = usage.get(&provider).and_then(|u| limits.stop_hit(u, now));
            if let Some(hit) = hit {
                if !self.overrides.contains(&provider) {
                    unavailable.push(format!(
                        "{} at {}% of its {}, past the {}% stop level",
                        provider.label(),
                        hit.percent,
                        hit.span.label(),
                        hit.level
                    ));
                    continue;
                }
            } else {
                self.overrides.remove(&provider);
            }
            available.push(i);
        }

        let wanted = want.and_then(|w| candidates.iter().position(|c| c.provider == w));
        let chosen = if let Some(w) = wanted {
            if !available.contains(&w) {
                return Err(format!("{} cannot run now", candidates[w].provider.label()));
            }
            w
        } else {
            let Some(&first) = available.first() else {
                return Err(format!("no provider can run: {}", unavailable.join("; ")));
            };
            let n = candidates.len();
            let provider = |i: usize| candidates[i].provider;
            let pick = match balancing.scheme {
                Scheme::Priority => None,
                Scheme::RoundRobin => available
                    .iter()
                    .copied()
                    .min_by_key(|&i| (i + n - self.rotation % n) % n),
                Scheme::Weighted => available.iter().copied().max_by_key(|&i| {
                    let credit = self.credit.get(&provider(i)).copied().unwrap_or(0);
                    (
                        credit + i64::from(balancing.weight(provider(i))),
                        Reverse(i),
                    )
                }),
                Scheme::Pace => available
                    .iter()
                    .copied()
                    .min_by_key(|&i| (pace(usage.get(&provider(i)), now), i)),
            };
            pick.unwrap_or(first)
        };
        if chosen == 0 && balancing.scheme == Scheme::Priority {
            // The active provider runs again, so fallbacks that failed get
            // another chance the next time one is needed.
            self.suspended.clear();
        }
        Ok(Pick {
            config: candidates[chosen].clone(),
            fallback: chosen > 0,
            index: chosen,
            candidates: candidates.len(),
            available: available.iter().map(|&i| candidates[i].provider).collect(),
        })
    }

    /// Counts `pick` as used, once an item was claimed for it.
    pub fn commit(&mut self, pick: &Pick, balancing: &Balancing) {
        self.rotation = (pick.index + 1) % pick.candidates.max(1);
        if balancing.scheme == Scheme::Weighted {
            let mut total = 0;
            for &p in &pick.available {
                let weight = i64::from(balancing.weight(p));
                total += weight;
                *self.credit.entry(p).or_default() += weight;
            }
            *self.credit.entry(pick.config.provider).or_default() -= total;
        }
    }
}

/// How far ahead of its pace a plan is, in tenths of a percent: its use
/// minus the share of the window that has passed, in the window where that
/// is highest. Below zero means under pace. A plan without usage reports,
/// or whose windows have reset since, counts as on pace.
fn pace(usage: Option<&PlanUsage>, now: u64) -> i64 {
    let ahead = |window: Option<UsageWindow>, length: u64| {
        let w = window?;
        let left = w
            .resets_at?
            .checked_sub(now)
            .filter(|l| *l > 0)?
            .min(length);
        let passed = (length - left) * 1_000 / length;
        Some(i64::from(w.percent) * 10 - i64::try_from(passed).ok()?)
    };
    usage
        .and_then(|u| {
            [ahead(u.session, SESSION_SECS), ahead(u.weekly, WEEK_SECS)]
                .into_iter()
                .flatten()
                .max()
        })
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    // Test code asserts rather than propagating; see Rust code review.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]

    use agent_backend::Auth;

    use super::*;
    use crate::UsageLimits;

    fn used(percent: u8) -> PlanUsage {
        PlanUsage {
            session: Some(UsageWindow {
                percent,
                resets_at: Some(1_000),
            }),
            weekly: None,
        }
    }

    fn pool(scheme: Scheme, providers: &[Provider]) -> PoolConfig {
        let config = |p| ProviderConfig::new(p, Auth::Plan);
        PoolConfig {
            workers: 1,
            provider: config(providers[0]),
            projects: Vec::new(),
            cli_dir: None,
            max_turns: 1,
            limits: UsageLimits::default(),
            fallbacks: providers[1..].iter().map(|p| config(*p)).collect(),
            balancing: Balancing {
                scheme,
                ..Balancing::default()
            },
            stall_after: crate::DEFAULT_STALL_AFTER,
            suggestion_retention: crate::DEFAULT_SUGGESTION_RETENTION,
            follow_all_comments: false,
            follow_ci_logs: false,
            claim_order: crate::ClaimOrder::default(),
        }
    }

    const THREE: [Provider; 3] = [Provider::Anthropic, Provider::OpenAi, Provider::Gemini];

    /// The providers of `n` claims in a row.
    fn sequence(
        picker: &mut Picker,
        config: &PoolConfig,
        usage: &UsageMap,
        n: usize,
    ) -> Vec<Provider> {
        (0..n)
            .map(|_| {
                let pick = picker.choose(config, usage, 0, None).unwrap();
                picker.commit(&pick, &config.balancing);
                pick.config.provider
            })
            .collect()
    }

    #[test]
    fn priority_fails_over_in_order_past_the_stop_level() {
        let config = pool(Scheme::Priority, &THREE);
        let mut picker = Picker::default();
        let mut usage = UsageMap::from([(Provider::Anthropic, used(84))]);
        let pick = |picker: &mut Picker, usage: &UsageMap, now| {
            picker
                .choose(&config, usage, now, None)
                .map(|p| p.config.provider)
        };
        assert_eq!(pick(&mut picker, &usage, 0), Ok(Provider::Anthropic));
        usage.insert(Provider::Anthropic, used(85));
        assert_eq!(pick(&mut picker, &usage, 0), Ok(Provider::OpenAi));
        usage.insert(Provider::OpenAi, used(90));
        assert_eq!(pick(&mut picker, &usage, 0), Ok(Provider::Gemini));
        // Once the window resets, the active provider is back.
        assert_eq!(pick(&mut picker, &usage, 1_000), Ok(Provider::Anthropic));
    }

    #[test]
    fn without_failover_priority_stops_until_overridden() {
        let mut config = pool(Scheme::Priority, &THREE);
        config.limits.failover = false;
        let usage = UsageMap::from([(Provider::Anthropic, used(87))]);
        let mut picker = Picker::default();
        assert_eq!(
            picker.choose(&config, &usage, 0, None).unwrap_err(),
            "no provider can run: Anthropic (Claude) at 87% of its current window, past the 85% stop level"
        );
        picker.overrides.insert(Provider::Anthropic);
        assert!(picker.choose(&config, &usage, 0, None).is_ok());
        // The override lapses when the window resets.
        assert!(picker.choose(&config, &usage, 1_000, None).is_ok());
        assert!(picker.overrides.is_empty());
    }

    #[test]
    fn failed_fallbacks_are_set_aside_until_the_active_provider_or_time_returns() {
        let config = pool(
            Scheme::Priority,
            &[Provider::Anthropic, Provider::Gemini, Provider::OpenAi],
        );
        // Claude's window resets after the suspensions would lapse.
        let resets = SUSPENSION_SECS * 2;
        let usage = UsageMap::from([(
            Provider::Anthropic,
            PlanUsage {
                session: Some(UsageWindow {
                    percent: 100,
                    resets_at: Some(resets),
                }),
                weekly: None,
            },
        )]);
        let mut picker = Picker::default();
        let suspend = |picker: &mut Picker, p| {
            picker.suspended.insert(
                p,
                Suspension {
                    error: "login failed".into(),
                    since: 0,
                },
            );
        };
        suspend(&mut picker, Provider::Gemini);
        let pick = picker.choose(&config, &usage, 0, None).unwrap();
        assert_eq!(
            (pick.config.provider, pick.fallback),
            (Provider::OpenAi, true)
        );

        suspend(&mut picker, Provider::OpenAi);
        assert_eq!(
            picker.choose(&config, &usage, 0, None).unwrap_err(),
            "no provider can run: Anthropic (Claude) at 100% of its current window, past the 85% \
             stop level; Gemini (Antigravity) failed its last run; OpenAI failed its last run"
        );
        // Half an hour on, they get another chance.
        let later = SUSPENSION_SECS - 1;
        assert!(picker.choose(&config, &usage, later, None).is_err());
        assert!(
            picker
                .choose(&config, &usage, SUSPENSION_SECS, None)
                .is_ok()
        );

        // And when the active provider runs again, suspensions clear.
        suspend(&mut picker, Provider::Gemini);
        let pick = picker.choose(&config, &usage, resets, None).unwrap();
        assert_eq!(
            (pick.config.provider, pick.fallback),
            (Provider::Anthropic, false)
        );
        assert!(picker.suspended.is_empty());
    }

    #[test]
    fn round_robin_takes_turns_and_skips_stopped_providers() {
        let config = pool(Scheme::RoundRobin, &THREE);
        let mut picker = Picker::default();
        let usage = UsageMap::new();
        assert_eq!(
            sequence(&mut picker, &config, &usage, 4),
            [
                Provider::Anthropic,
                Provider::OpenAi,
                Provider::Gemini,
                Provider::Anthropic
            ]
        );
        // Picking without claiming does not move the rotation.
        for _ in 0..5 {
            picker.choose(&config, &usage, 0, None).unwrap();
        }
        let stopped = UsageMap::from([(Provider::Gemini, used(99))]);
        assert_eq!(
            sequence(&mut picker, &config, &stopped, 3),
            [Provider::OpenAi, Provider::Anthropic, Provider::OpenAi]
        );
    }

    #[test]
    fn weighted_spreads_work_in_proportion() {
        let mut config = pool(Scheme::Weighted, &THREE);
        config.balancing.weights = HashMap::from([(Provider::Anthropic, 3), (Provider::OpenAi, 2)]);
        let mut picker = Picker::default();
        let picks = sequence(&mut picker, &config, &UsageMap::new(), 12);
        let count = |p| picks.iter().filter(|x| **x == p).count();
        assert_eq!(
            (
                count(Provider::Anthropic),
                count(Provider::OpenAi),
                count(Provider::Gemini)
            ),
            (6, 4, 2)
        );
        // Smooth: never the same provider three times running.
        assert!(
            picks.windows(3).all(|w| !(w[0] == w[1] && w[1] == w[2])),
            "{picks:?}"
        );
    }

    #[test]
    fn pace_prefers_the_plan_furthest_below_its_pace() {
        let config = pool(Scheme::Pace, &THREE);
        let mut picker = Picker::default();
        let now = 1_000_000;
        // Halfway through each 5-hour window.
        let window = |percent| PlanUsage {
            session: Some(UsageWindow {
                percent,
                resets_at: Some(now + SESSION_SECS / 2),
            }),
            weekly: None,
        };
        let usage = UsageMap::from([
            (Provider::Anthropic, window(60)),
            (Provider::OpenAi, window(20)),
            (Provider::Gemini, window(45)),
        ]);
        let pick = picker.choose(&config, &usage, now, None).unwrap();
        assert_eq!(pick.config.provider, Provider::OpenAi);

        // A provider without reports counts as on pace, behind one 30
        // points under it but ahead of one 10 points over it.
        assert_eq!(pace(None, now), 0);
        assert_eq!(pace(usage.get(&Provider::OpenAi), now), -300);
        assert_eq!(pace(usage.get(&Provider::Anthropic), now), 100);
    }

    #[test]
    fn a_wanted_provider_runs_only_if_it_may() {
        let config = pool(Scheme::RoundRobin, &THREE);
        let mut picker = Picker::default();
        let usage = UsageMap::from([(Provider::OpenAi, used(90))]);
        let pick = picker
            .choose(&config, &usage, 0, Some(Provider::Gemini))
            .unwrap();
        assert_eq!(pick.config.provider, Provider::Gemini);
        assert_eq!(
            picker
                .choose(&config, &usage, 0, Some(Provider::OpenAi))
                .unwrap_err(),
            "OpenAI cannot run now"
        );
        // A provider no longer configured is no reason to wait.
        assert!(
            picker
                .choose(&config, &usage, 0, Some(Provider::Deepseek))
                .is_ok()
        );
    }
}

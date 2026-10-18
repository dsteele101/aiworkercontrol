//! How much of a plan's allowance has been used, as a CLI reports it.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Auth, Cli, Invocation, ProviderConfig};

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

impl ProviderConfig {
    /// A run that reports the plan's usage without starting an agent, read
    /// with [`ProviderConfig::parse_usage_report`]. Only `agy` has one:
    /// its `/usage` command works headless and spends no quota. `None` for
    /// other CLIs, which report usage only during runs, and for API keys.
    pub fn usage_invocation(&self) -> Option<Invocation> {
        if self.provider.cli() != Cli::Antigravity || self.auth != Auth::Plan {
            return None;
        }
        let mut run = Invocation::new(Cli::Antigravity);
        run.args = ["-p", "/usage", "--output-format", "json"]
            .map(String::from)
            .to_vec();
        Some(run)
    }

    /// Reads the output of [`ProviderConfig::usage_invocation`].
    ///
    /// `agy` reports one weekly and one 5-hour bucket per group of models
    /// that share a quota. Only the Gemini group is read: the Claude and GPT
    /// models Antigravity also offers are being withdrawn.
    pub fn parse_usage_report(&self, output: &str) -> Option<PlanUsage> {
        let report: Value = serde_json::from_str(output).ok()?;
        let group = report
            .pointer("/command/data/groups")?
            .as_array()?
            .iter()
            .find(|g| {
                g.get("name")
                    .and_then(Value::as_str)
                    .is_some_and(|n| n.to_ascii_lowercase().contains("gemini"))
            })?;
        let buckets = group.get("buckets")?.as_array()?;
        let window = |name: &str| {
            let bucket = buckets
                .iter()
                .find(|b| b.get("window").and_then(Value::as_str) == Some(name))?;
            let left = bucket.get("remaining_fraction").and_then(Value::as_f64)?;
            Some(UsageWindow {
                // Clamped, so the cast cannot truncate.
                percent: ((1.0 - left) * 100.0).round().clamp(0.0, 100.0) as u8,
                resets_at: bucket
                    .get("reset_time")
                    .and_then(Value::as_str)
                    .and_then(unix_seconds),
            })
        };
        let usage = PlanUsage {
            session: window("5h"),
            weekly: window("weekly"),
        };
        (usage != PlanUsage::default()).then_some(usage)
    }
}

/// Seconds since the Unix epoch of a UTC timestamp such as
/// `2026-10-23T21:59:39Z`; fractional seconds are dropped.
fn unix_seconds(text: &str) -> Option<u64> {
    let text = text.strip_suffix('Z')?;
    let (date, time) = text.split_once('T')?;
    let mut date = date.splitn(3, '-').map(str::parse::<i64>);
    let (year, month, day) = (date.next()?.ok()?, date.next()?.ok()?, date.next()?.ok()?);
    let time = time.split('.').next()?;
    let mut time = time.splitn(3, ':').map(str::parse::<i64>);
    let (hour, min, sec) = (time.next()?.ok()?, time.next()?.ok()?, time.next()?.ok()?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    // Days from 1970-01-01 to the date, by Howard Hinnant's
    // `days_from_civil`.
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days * 86_400 + hour * 3_600 + min * 60 + sec).ok()
}

#[cfg(test)]
mod tests {
    // Test code asserts rather than propagating; see Rust code review.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]

    use super::*;
    use crate::Provider;

    /// `agy -p /usage --output-format json` (1.4.2), descriptions shortened.
    const AGY_USAGE: &str = r#"{"conversation_id":"","status":"SUCCESS","response":"…","duration_seconds":0,"num_turns":0,"command":{"name":"usage","data":{"description":"Within each group, models share a weekly limit and a 5-hour limit.","groups":[{"name":"Gemini Models","description":"Models within this group: Gemini Flash, Gemini Pro","buckets":[{"id":"gemini-weekly","name":"Weekly Limit Remaining","window":"weekly","remaining_fraction":0.8343654870986938,"reset_time":"2026-10-23T21:59:39Z"},{"id":"gemini-5h","name":"Five Hour Limit Remaining","window":"5h","remaining_fraction":0.9993641376495361,"reset_time":"2026-10-18T17:44:13Z"}]},{"name":"Claude and GPT models","description":"Models within this group: Claude Opus, Claude Sonnet, GPT-OSS","buckets":[{"id":"3p-weekly","name":"Weekly Limit Remaining","window":"weekly","remaining_fraction":0.5,"reset_time":"2026-10-25T12:52:59Z"},{"id":"3p-5h","name":"Five Hour Limit Remaining","window":"5h","remaining_fraction":1,"reset_time":"2026-10-18T17:52:59Z"}]}]}}}"#;

    fn agy() -> ProviderConfig {
        ProviderConfig::new(Provider::Gemini, Auth::Plan)
    }

    #[test]
    fn agy_report_gives_the_gemini_group() {
        assert_eq!(
            agy().parse_usage_report(AGY_USAGE),
            Some(PlanUsage {
                session: Some(UsageWindow {
                    percent: 0,
                    resets_at: Some(1_792_345_453),
                }),
                weekly: Some(UsageWindow {
                    percent: 17,
                    resets_at: Some(1_792_792_779),
                }),
            })
        );
        assert_eq!(agy().parse_usage_report("not json"), None);
    }

    #[test]
    fn only_agy_plans_are_polled() {
        let run = agy().usage_invocation().unwrap();
        assert_eq!(run.program, "agy");
        assert_eq!(run.args, ["-p", "/usage", "--output-format", "json"]);
        let keyed = ProviderConfig::new(Provider::Gemini, Auth::ApiKey { key: "k".into() });
        assert!(keyed.usage_invocation().is_none());
        assert!(
            ProviderConfig::new(Provider::Anthropic, Auth::Plan)
                .usage_invocation()
                .is_none()
        );
    }

    #[test]
    fn utc_timestamps() {
        assert_eq!(unix_seconds("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(unix_seconds("2000-03-01T00:00:00Z"), Some(951_868_800));
        assert_eq!(unix_seconds("2026-10-18T17:44:13.5Z"), Some(1_792_345_453));
        assert_eq!(unix_seconds("2026-13-01T00:00:00Z"), None);
        assert_eq!(unix_seconds("2026-10-18T17:44:13+02:00"), None);
    }
}

//! What one turn did, gathered while it runs, for the work history.

use std::collections::{BTreeMap, BTreeSet};

use agent_backend::{AgentEvent, RunMetrics};

use crate::git::DiffStat;

/// A turn's measurements. Filled in by the agent's events and by the run's
/// steps, and read once the turn ends, however it ended.
#[derive(Debug, Default)]
pub(super) struct Trace {
    pub models: BTreeSet<String>,
    pub metrics: RunMetrics,
    pub tool_calls: BTreeMap<String, u32>,
    pub stalled: bool,
    pub agent_ms: u64,
    pub commits: u32,
    pub diff: Option<DiffStat>,
}

impl Trace {
    /// Takes in what `event` says about the run.
    pub fn observe(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::Model(model) => {
                self.models.insert(model.clone());
            }
            AgentEvent::ToolUse { name, .. } => {
                *self.tool_calls.entry(name.clone()).or_default() += 1;
            }
            AgentEvent::Metrics(m) => {
                // A run can report more than once, e.g. after background
                // commands finish. Tokens and time cover each report's own
                // stretch, so they add up; the cost does not (below).
                let total = &mut self.metrics;
                total.input_tokens = sum(total.input_tokens, m.input_tokens);
                total.output_tokens = sum(total.output_tokens, m.output_tokens);
                total.cache_read_tokens = sum(total.cache_read_tokens, m.cache_read_tokens);
                total.cache_write_tokens = sum(total.cache_write_tokens, m.cache_write_tokens);
                // Claude's cost is the running total for the whole run.
                total.cost_micro_usd = total.cost_micro_usd.max(m.cost_micro_usd);
                total.duration_ms = sum(total.duration_ms, m.duration_ms);
                self.models.extend(m.models.iter().cloned());
            }
            AgentEvent::Text(_)
            | AgentEvent::Notice(_)
            | AgentEvent::Usage(_)
            | AgentEvent::Status(_)
            | AgentEvent::Background(_)
            | AgentEvent::Finished { .. } => {}
        }
    }
}

fn sum(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (None, None) => None,
        (a, b) => Some(a.unwrap_or(0) + b.unwrap_or(0)),
    }
}

#[cfg(test)]
mod tests {
    // Test code asserts rather than propagating; see Rust code review.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]

    use super::*;

    #[test]
    fn reports_add_up_and_tools_are_counted() {
        let mut trace = Trace::default();
        let report = |input| {
            AgentEvent::Metrics(RunMetrics {
                input_tokens: Some(input),
                cost_micro_usd: Some(input * 100),
                models: vec!["m".into()],
                ..RunMetrics::default()
            })
        };
        trace.observe(&AgentEvent::Model("m".into()));
        trace.observe(&report(5));
        trace.observe(&report(7));
        for _ in 0..2 {
            trace.observe(&AgentEvent::ToolUse {
                name: "Bash".into(),
                detail: String::new(),
            });
        }
        assert_eq!(trace.metrics.input_tokens, Some(12));
        // Each report's cost already includes the earlier ones'.
        assert_eq!(trace.metrics.cost_micro_usd, Some(700));
        assert_eq!(trace.metrics.output_tokens, None);
        assert_eq!(trace.models.len(), 1);
        assert_eq!(trace.tool_calls["Bash"], 2);
    }
}

//! What a run cost, as a CLI reports it when a turn ends.

use serde::{Deserialize, Serialize};

/// Token counts and cost for one reported turn. A field the CLI did not
/// report is `None` (or empty).
///
/// The CLIs count differently: Claude's `input_tokens` leaves out cached
/// tokens, Codex's includes them. Each is kept as reported, so compare
/// providers through `cost_micro_usd` or `output_tokens`, not input.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunMetrics {
    /// Tokens sent to the model.
    pub input_tokens: Option<u64>,
    /// Tokens the model wrote.
    pub output_tokens: Option<u64>,
    /// Input tokens served from the prompt cache.
    pub cache_read_tokens: Option<u64>,
    /// Input tokens written to the prompt cache.
    pub cache_write_tokens: Option<u64>,
    /// The CLI's own estimate of the cost, in millionths of a US dollar.
    pub cost_micro_usd: Option<u64>,
    /// The CLI's own measure of how long the turn took, in milliseconds.
    pub duration_ms: Option<u64>,
    /// The models that did the work, when the CLI names them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,
}

impl RunMetrics {
    /// Whether the CLI reported nothing at all.
    ///
    /// ```
    /// use agent_backend::RunMetrics;
    ///
    /// assert!(RunMetrics::default().is_empty());
    /// ```
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

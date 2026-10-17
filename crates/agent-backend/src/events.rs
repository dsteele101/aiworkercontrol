//! Parsing each CLI's line-delimited JSON output into common events.

use serde_json::Value;

use crate::{Cli, PlanUsage, UsageWindow};

/// Something an agent did, in a form common to every CLI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentEvent {
    /// Text the agent wrote.
    Text(String),
    /// The agent used a tool.
    ToolUse {
        /// The tool, e.g. `Bash` or `shell`.
        name: String,
        /// The salient argument: a command, a path, a query.
        detail: String,
    },
    /// A non-fatal problem, or output that was not an event.
    Notice(String),
    /// The plan's usage, as the CLI last reported it.
    Usage(PlanUsage),
    /// The run ended.
    Finished {
        /// Whether the agent reported success.
        ok: bool,
        /// The agent's final message or error, when the CLI reports one.
        summary: Option<String>,
    },
}

/// Parses one line of `cli`'s stdout. A line can carry several events, or
/// none; a line that is not JSON becomes a [`AgentEvent::Notice`].
///
/// ```
/// use agent_backend::{AgentEvent, Cli, parse_line};
///
/// let line = r#"{"type":"result","is_error":false,"result":"Fixed it"}"#;
/// assert_eq!(
///     parse_line(Cli::Claude, line),
///     vec![AgentEvent::Finished { ok: true, summary: Some("Fixed it".into()) }],
/// );
/// ```
pub fn parse_line(cli: Cli, line: &str) -> Vec<AgentEvent> {
    let line = line.trim();
    if line.is_empty() {
        return Vec::new();
    }
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return vec![AgentEvent::Notice(line.to_string())];
    };
    match cli {
        Cli::Claude => claude(&value),
        Cli::Codex => codex(&value),
        Cli::Gemini => gemini(&value),
    }
}

fn claude(v: &Value) -> Vec<AgentEvent> {
    match str_at(v, "type") {
        Some("assistant") => v
            .pointer("/message/content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|block| match str_at(block, "type") {
                Some("text") => text_event(str_at(block, "text")),
                Some("tool_use") => Some(AgentEvent::ToolUse {
                    name: str_at(block, "name").unwrap_or_default().to_string(),
                    detail: tool_detail(block.get("input")),
                }),
                _ => None,
            })
            .collect(),
        Some("rate_limit_event") => claude_usage(v).map(AgentEvent::Usage).into_iter().collect(),
        Some("result") => vec![AgentEvent::Finished {
            // `subtype` can read "success" on a failed run; `is_error` is the truth.
            ok: !v.get("is_error").and_then(Value::as_bool).unwrap_or(true),
            summary: str_at(v, "result").map(str::to_string),
        }],
        _ => Vec::new(),
    }
}

/// The plan usage in a `rate_limit_event`: its `unifiedWindows` give each
/// window's utilization as a fraction and its reset time in Unix seconds.
fn claude_usage(v: &Value) -> Option<PlanUsage> {
    let windows = v.pointer("/rate_limit_info/unifiedWindows")?;
    let window = |name: &str| {
        let w = windows.get(name)?;
        let used = w.get("utilization").and_then(Value::as_f64)?;
        Some(UsageWindow {
            // Clamped, so the cast cannot truncate.
            percent: (used * 100.0).round().clamp(0.0, 100.0) as u8,
            resets_at: w.get("resetsAt").and_then(Value::as_u64),
        })
    };
    let usage = PlanUsage {
        session: window("five_hour"),
        weekly: window("seven_day"),
    };
    (usage != PlanUsage::default()).then_some(usage)
}

fn codex(v: &Value) -> Vec<AgentEvent> {
    let item = v.get("item");
    let item_type = item.and_then(|i| str_at(i, "type"));
    let event = match (str_at(v, "type"), item_type) {
        (Some("item.started"), Some("command_execution")) => Some(AgentEvent::ToolUse {
            name: "shell".into(),
            detail: item
                .and_then(|i| str_at(i, "command"))
                .unwrap_or_default()
                .to_string(),
        }),
        (Some("item.completed"), Some("agent_message")) => {
            text_event(item.and_then(|i| str_at(i, "text")))
        }
        (Some("item.completed"), Some("file_change")) => Some(AgentEvent::ToolUse {
            name: "edit".into(),
            detail: item
                .and_then(|i| i.get("changes"))
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|c| str_at(c, "path"))
                .collect::<Vec<_>>()
                .join(", "),
        }),
        (Some("item.started"), Some("mcp_tool_call")) => Some(AgentEvent::ToolUse {
            name: item
                .and_then(|i| str_at(i, "tool"))
                .unwrap_or("mcp")
                .to_string(),
            detail: String::new(),
        }),
        (Some("item.started"), Some("web_search")) => Some(AgentEvent::ToolUse {
            name: "web_search".into(),
            detail: item
                .and_then(|i| str_at(i, "query"))
                .unwrap_or_default()
                .to_string(),
        }),
        (Some("item.completed"), Some("error")) => item
            .and_then(|i| str_at(i, "message"))
            .map(|m| AgentEvent::Notice(m.to_string())),
        (Some("error"), _) => str_at(v, "message").map(|m| AgentEvent::Notice(m.to_string())),
        (Some("turn.completed"), _) => Some(AgentEvent::Finished {
            ok: true,
            summary: None,
        }),
        (Some("turn.failed"), _) => Some(AgentEvent::Finished {
            ok: false,
            summary: v
                .pointer("/error/message")
                .and_then(Value::as_str)
                .map(str::to_string),
        }),
        _ => None,
    };
    event.into_iter().collect()
}

fn gemini(v: &Value) -> Vec<AgentEvent> {
    let event = match str_at(v, "type") {
        Some("message") if str_at(v, "role") == Some("assistant") => {
            text_event(str_at(v, "content"))
        }
        Some("tool_use") => Some(AgentEvent::ToolUse {
            name: str_at(v, "tool_name").unwrap_or_default().to_string(),
            detail: tool_detail(v.get("parameters")),
        }),
        Some("tool_result") if str_at(v, "status") == Some("error") => Some(AgentEvent::Notice(
            v.pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("tool failed")
                .to_string(),
        )),
        Some("error") => str_at(v, "message").map(|m| AgentEvent::Notice(m.to_string())),
        Some("result") => Some(AgentEvent::Finished {
            ok: str_at(v, "status") == Some("success"),
            summary: v
                .pointer("/error/message")
                .and_then(Value::as_str)
                .map(str::to_string),
        }),
        _ => None,
    };
    event.into_iter().collect()
}

fn str_at<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

fn text_event(text: Option<&str>) -> Option<AgentEvent> {
    text.filter(|t| !t.trim().is_empty())
        .map(|t| AgentEvent::Text(t.to_string()))
}

/// The most telling argument of a tool call.
fn tool_detail(input: Option<&Value>) -> String {
    const KEYS: [&str; 8] = [
        "command",
        "file_path",
        "absolute_path",
        "path",
        "pattern",
        "url",
        "query",
        "description",
    ];
    input
        .and_then(|i| KEYS.iter().find_map(|k| str_at(i, k)))
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
mod tests {
    // Test code asserts rather than propagating; see Rust code review.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]

    use super::*;

    /// The last two lines of `claude -p` 2.1.302 run with an invalid API
    /// key, trimmed to the fields the parser reads.
    const CLAUDE_FAILED_RUN: [&str; 2] = [
        r#"{"type":"assistant","message":{"model":"<synthetic>","content":[{"type":"text","text":"Invalid API key · Fix external API key"}]}}"#,
        r#"{"type":"result","subtype":"success","is_error":true,"result":"Invalid API key · Fix external API key"}"#,
    ];

    /// `gemini -p` 0.64.0 with an invalid API key, error message shortened.
    const GEMINI_FAILED_RUN: [&str; 3] = [
        r#"{"type":"init","timestamp":"2026-10-17T04:20:47.209Z","session_id":"4c27","model":"auto"}"#,
        r#"{"type":"message","timestamp":"2026-10-17T04:20:47.211Z","role":"user","content":"hi"}"#,
        r#"{"type":"result","timestamp":"2026-10-17T04:20:47.430Z","status":"error","error":{"type":"unknown","message":"API key not valid."}}"#,
    ];

    /// `codex exec --json` 0.163.0 against an unreachable endpoint.
    const CODEX_UNREACHABLE: [&str; 3] = [
        r#"{"type":"thread.started","thread_id":"01a1"}"#,
        r#"{"type":"turn.started"}"#,
        r#"{"type":"error","message":"Reconnecting... waiting for network (Connection failed: error sending request)"}"#,
    ];

    fn parse_all(cli: Cli, lines: &[&str]) -> Vec<AgentEvent> {
        lines.iter().flat_map(|l| parse_line(cli, l)).collect()
    }

    #[test]
    fn claude_is_error_overrides_success_subtype() {
        let events = parse_all(Cli::Claude, &CLAUDE_FAILED_RUN);
        let msg = "Invalid API key · Fix external API key".to_string();
        assert_eq!(
            events,
            vec![
                AgentEvent::Text(msg.clone()),
                AgentEvent::Finished {
                    ok: false,
                    summary: Some(msg)
                },
            ]
        );
    }

    /// A `rate_limit_event` from `claude -p` 2.1.302 on a plan login.
    const CLAUDE_RATE_LIMIT: &str = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","resetsAt":1792225800,"rateLimitType":"five_hour","overageStatus":"rejected","overageDisabledReason":"org_level_disabled","isUsingOverage":false,"unifiedWindows":{"five_hour":{"utilization":0.64,"resetsAt":1792225800},"seven_day":{"utilization":0.09,"resetsAt":1792800000}}},"uuid":"u","session_id":"s"}"#;

    #[test]
    fn claude_rate_limit_event_reports_both_windows() {
        assert_eq!(
            parse_line(Cli::Claude, CLAUDE_RATE_LIMIT),
            vec![AgentEvent::Usage(PlanUsage {
                session: Some(UsageWindow {
                    percent: 64,
                    resets_at: Some(1_792_225_800)
                }),
                weekly: Some(UsageWindow {
                    percent: 9,
                    resets_at: Some(1_792_800_000)
                }),
            })]
        );
        let bare = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed"}}"#;
        assert!(parse_line(Cli::Claude, bare).is_empty());
    }

    #[test]
    fn claude_tool_use_takes_command() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"cargo test","description":"run tests"}}]}}"#;
        assert_eq!(
            parse_line(Cli::Claude, line),
            vec![AgentEvent::ToolUse {
                name: "Bash".into(),
                detail: "cargo test".into()
            }]
        );
    }

    #[test]
    fn gemini_failed_result() {
        assert_eq!(
            parse_all(Cli::Gemini, &GEMINI_FAILED_RUN),
            vec![AgentEvent::Finished {
                ok: false,
                summary: Some("API key not valid.".into())
            }]
        );
    }

    #[test]
    fn gemini_assistant_message_and_tool() {
        let lines = [
            r#"{"type":"message","role":"assistant","content":"Looking","delta":true}"#,
            r#"{"type":"tool_use","tool_name":"read_file","tool_id":"t1","parameters":{"absolute_path":"/wt/a.rs"}}"#,
            r#"{"type":"result","status":"success","stats":{}}"#,
        ];
        assert_eq!(
            parse_all(Cli::Gemini, &lines),
            vec![
                AgentEvent::Text("Looking".into()),
                AgentEvent::ToolUse {
                    name: "read_file".into(),
                    detail: "/wt/a.rs".into()
                },
                AgentEvent::Finished {
                    ok: true,
                    summary: None
                },
            ]
        );
    }

    #[test]
    fn codex_reconnect_is_a_notice_not_a_failure() {
        assert_eq!(
            parse_all(Cli::Codex, &CODEX_UNREACHABLE),
            vec![AgentEvent::Notice(
                "Reconnecting... waiting for network (Connection failed: error sending request)"
                    .into()
            )]
        );
    }

    #[test]
    fn codex_items() {
        let lines = [
            r#"{"type":"item.started","item":{"id":"1","type":"command_execution","command":"bash -lc ls","status":"in_progress"}}"#,
            r#"{"type":"item.completed","item":{"id":"2","type":"file_change","changes":[{"path":"a.rs","kind":"update"},{"path":"b.rs","kind":"add"}]}}"#,
            r#"{"type":"item.completed","item":{"id":"3","type":"agent_message","text":"Done"}}"#,
            r#"{"type":"turn.failed","error":{"message":"quota"}}"#,
        ];
        assert_eq!(
            parse_all(Cli::Codex, &lines),
            vec![
                AgentEvent::ToolUse {
                    name: "shell".into(),
                    detail: "bash -lc ls".into()
                },
                AgentEvent::ToolUse {
                    name: "edit".into(),
                    detail: "a.rs, b.rs".into()
                },
                AgentEvent::Text("Done".into()),
                AgentEvent::Finished {
                    ok: false,
                    summary: Some("quota".into())
                },
            ]
        );
    }

    #[test]
    fn non_json_line_is_a_notice() {
        assert_eq!(
            parse_line(Cli::Codex, "Reading additional input from stdin..."),
            vec![AgentEvent::Notice(
                "Reading additional input from stdin...".into()
            )]
        );
        assert!(parse_line(Cli::Codex, "   ").is_empty());
    }
}

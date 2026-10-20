//! Parsing each CLI's line-delimited JSON output into common events.

use serde_json::Value;

use crate::{Cli, PlanUsage, RunMetrics, UsageWindow};

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
    /// The model the CLI says it is running, as soon as it says so.
    Model(String),
    /// Tokens and cost, as reported when a turn ends.
    Metrics(RunMetrics),
    /// The agent's own one-line account of where it stands, e.g.
    /// "fix written; waiting on the test build".
    Status(String),
    /// The commands the agent has left running in the background, by
    /// description; empty once none are.
    Background(Vec<String>),
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
        Cli::Antigravity => antigravity(&value),
        Cli::Vibe => vibe(&value),
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
        Some("system") => match str_at(v, "subtype") {
            Some("init") => str_at(v, "model")
                .map(|m| AgentEvent::Model(m.to_string()))
                .into_iter()
                .collect(),
            Some("post_turn_summary") => str_at(v, "status_detail")
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| AgentEvent::Status(s.to_string()))
                .into_iter()
                .collect(),
            Some("background_tasks_changed") => vec![AgentEvent::Background(
                v.get("tasks")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|t| str_at(t, "description"))
                    .map(str::to_string)
                    .collect(),
            )],
            _ => Vec::new(),
        },
        Some("result") => claude_metrics(v)
            .map(AgentEvent::Metrics)
            .into_iter()
            .chain([AgentEvent::Finished {
                // `subtype` can read "success" on a failed run; `is_error` is the truth.
                ok: !v.get("is_error").and_then(Value::as_bool).unwrap_or(true),
                summary: str_at(v, "result").map(str::to_string),
            }])
            .collect(),
        _ => Vec::new(),
    }
}

/// The tokens, cost and models in a `result` line.
fn claude_metrics(v: &Value) -> Option<RunMetrics> {
    let usage = v.get("usage");
    let count = |key: &str| usage.and_then(|u| u.get(key)).and_then(Value::as_u64);
    let metrics = RunMetrics {
        input_tokens: count("input_tokens"),
        output_tokens: count("output_tokens"),
        cache_read_tokens: count("cache_read_input_tokens"),
        cache_write_tokens: count("cache_creation_input_tokens"),
        cost_micro_usd: v.get("total_cost_usd").and_then(micro_usd),
        duration_ms: v.get("duration_ms").and_then(Value::as_u64),
        models: v
            .get("modelUsage")
            .and_then(Value::as_object)
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default(),
    };
    (!metrics.is_empty()).then_some(metrics)
}

/// A dollar amount in millionths of a dollar.
fn micro_usd(dollars: &Value) -> Option<u64> {
    let dollars = dollars.as_f64().filter(|d| d.is_finite() && *d >= 0.0)?;
    // Non-negative and finite, and no real cost nears u64::MAX millionths.
    Some((dollars * 1_000_000.0).round() as u64)
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
        (Some("turn.completed"), _) => {
            let usage = v.get("usage");
            let count = |key: &str| usage.and_then(|u| u.get(key)).and_then(Value::as_u64);
            let metrics = RunMetrics {
                input_tokens: count("input_tokens"),
                output_tokens: count("output_tokens"),
                cache_read_tokens: count("cached_input_tokens"),
                ..RunMetrics::default()
            };
            let finished = AgentEvent::Finished {
                ok: true,
                summary: None,
            };
            return if metrics.is_empty() {
                vec![finished]
            } else {
                vec![AgentEvent::Metrics(metrics), finished]
            };
        }
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

/// `agy` streams an `init`, then a `step_update` per step change (a tool
/// starting or ending, or a slice of the reply), then a `result`. Reply
/// text arrives in fragments, so it is taken whole from the `result`.
fn antigravity(v: &Value) -> Vec<AgentEvent> {
    match str_at(v, "event") {
        Some("step_update") => {
            let Some(step) = v.get("step_update") else {
                return Vec::new();
            };
            let tool = step.get("tool_info");
            let event = match (str_at(step, "step_type"), str_at(step, "state")) {
                (Some("tool"), Some("ACTIVE")) => Some(AgentEvent::ToolUse {
                    name: str_at(step, "tool_name").unwrap_or_default().to_string(),
                    detail: tool_detail(tool.and_then(|t| t.get("parameters"))),
                }),
                (Some("tool"), Some("DONE")) => tool
                    .and_then(|t| t.pointer("/error/message"))
                    .and_then(Value::as_str)
                    .map(|m| AgentEvent::Notice(m.to_string())),
                _ => None,
            };
            event.into_iter().collect()
        }
        Some("result") => {
            let Some(result) = v.get("result") else {
                return Vec::new();
            };
            let response = str_at(result, "response")
                .map(str::trim)
                .filter(|r| !r.is_empty());
            let usage = result.get("usage");
            let count = |key: &str| usage.and_then(|u| u.get(key)).and_then(Value::as_u64);
            let metrics = RunMetrics {
                input_tokens: count("input_tokens"),
                output_tokens: count("output_tokens"),
                duration_ms: result
                    .get("duration_seconds")
                    .and_then(Value::as_f64)
                    .filter(|s| s.is_finite() && *s >= 0.0)
                    // Non-negative and finite, so the cast cannot wrap.
                    .map(|s| (s * 1000.0).round() as u64),
                ..RunMetrics::default()
            };
            let metrics = (!metrics.is_empty()).then_some(AgentEvent::Metrics(metrics));
            if str_at(result, "status") == Some("SUCCESS") {
                metrics
                    .into_iter()
                    .chain(text_event(response))
                    .chain([AgentEvent::Finished {
                        ok: true,
                        summary: response.map(str::to_string),
                    }])
                    .collect()
            } else {
                let error = result
                    .pointer("/error/message")
                    .or_else(|| result.get("error"))
                    .and_then(Value::as_str)
                    .or(str_at(result, "status"));
                metrics
                    .into_iter()
                    .chain([AgentEvent::Finished {
                        ok: false,
                        summary: error.map(str::to_string),
                    }])
                    .collect()
            }
        }
        _ => Vec::new(),
    }
}

/// `vibe` streams one entry per line: the user's `message`, `reasoning`, the
/// assistant's `message`, and an `effect` per tool call. It has no closing
/// `result` line, so a run ends by its exit status alone (non-zero when it
/// stops on a limit or fails).
fn vibe(v: &Value) -> Vec<AgentEvent> {
    match str_at(v, "type") {
        Some("message") if str_at(v, "role") == Some("assistant") => v
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|part| text_event(str_at(part, "text")))
            .collect(),
        Some("effect") => {
            let failed =
                v.pointer("/state/display/success").and_then(Value::as_bool) == Some(false);
            let error = v
                .pointer("/state/display/message")
                .and_then(Value::as_str)
                .filter(|_| failed)
                .map(|m| AgentEvent::Notice(m.to_string()));
            // Titles read `file_system.bash`; the tool is the last part.
            let title = str_at(v, "title").unwrap_or_default();
            let name = title.rsplit('.').next().unwrap_or(title);
            [AgentEvent::ToolUse {
                name: name.to_string(),
                detail: tool_detail(v.pointer("/detail/input")),
            }]
            .into_iter()
            .chain(error)
            .collect()
        }
        _ => Vec::new(),
    }
}

fn str_at<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

fn text_event(text: Option<&str>) -> Option<AgentEvent> {
    text.filter(|t| !t.trim().is_empty())
        .map(|t| AgentEvent::Text(t.to_string()))
}

/// The most telling argument of a tool call; failing a known one, the first
/// text argument (`agy` names them in CamelCase, e.g. `AbsolutePath`).
fn tool_detail(input: Option<&Value>) -> String {
    const KEYS: [&str; 9] = [
        "command",
        "file_path",
        "filePath",
        "absolute_path",
        "path",
        "pattern",
        "url",
        "query",
        "description",
    ];
    input
        .and_then(|i| {
            KEYS.iter()
                .find_map(|k| str_at(i, k))
                .or_else(|| i.as_object()?.values().find_map(Value::as_str))
        })
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

    /// `agy -p … --output-format stream-json` (1.4.2) reading a file and
    /// answering, with the paths shortened and the reply's middle fragments
    /// left out.
    const AGY_RUN: [&str; 6] = [
        r#"{"event":"init","conversation_id":"dae7","init":{"cwd":"/wt","tools":["view_file"],"permission_mode":"request-review"}}"#,
        r#"{"event":"step_update","step_update":{"conversation_id":"dae7","step_index":0,"state":"DONE","step_type":"user_input"}}"#,
        r#"{"event":"step_update","step_update":{"conversation_id":"dae7","step_index":2,"state":"ACTIVE","step_type":"tool","tool_name":"view_file","tool_info":{"name":"view_file","parameters":{"AbsolutePath":"/spec/NOTE.md"}}}}"#,
        r#"{"event":"step_update","step_update":{"conversation_id":"dae7","step_index":2,"state":"DONE","step_type":"tool","tool_name":"view_file","duration_seconds":0.08,"tool_info":{"name":"view_file","parameters":{"AbsolutePath":"/spec/NOTE.md"},"output":"2 lines, 21 bytes"}}}"#,
        r#"{"event":"step_update","step_update":{"conversation_id":"dae7","step_index":3,"state":"ACTIVE","step_type":"agent_response","text_delta":"The secret"}}"#,
        r#"{"event":"result","result":{"conversation_id":"dae7","status":"SUCCESS","response":"The secret word is \"pelican\".\n","duration_seconds":3.4,"num_turns":1,"usage":{"input_tokens":24507,"output_tokens":186}}}"#,
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

    /// Captured from `vibe -p ... --output streaming` (3.0.0), trimmed to the
    /// fields read.
    const VIBE_RUN: [&str; 4] = [
        r#"{"type":"message","role":"user","content":[{"type":"text","text":"Run echo hi"}]}"#,
        r#"{"type":"reasoning","text":"I'll run it."}"#,
        r#"{"type":"effect","title":"file_system.bash","detail":{"kind":"shell","input":{"command":"echo hi"}},"state":{"status":"completed","display":{"success":true,"message":"echo hi"}}}"#,
        r#"{"type":"message","role":"assistant","content":[{"type":"text","text":"ok"}]}"#,
    ];

    #[test]
    fn vibe_run_from_captured_output() {
        assert_eq!(
            parse_all(Cli::Vibe, &VIBE_RUN),
            vec![
                AgentEvent::ToolUse {
                    name: "bash".into(),
                    detail: "echo hi".into()
                },
                AgentEvent::Text("ok".into()),
            ]
        );
    }

    #[test]
    fn vibe_file_write_names_its_path() {
        let line = r#"{"type":"effect","title":"file_system.write_file","detail":{"input":{"filePath":"hello.txt","content":"hi"}}}"#;
        assert_eq!(
            parse_line(Cli::Vibe, line),
            vec![AgentEvent::ToolUse {
                name: "write_file".into(),
                detail: "hello.txt".into()
            }]
        );
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
    fn claude_status_and_background_tasks() {
        // From `claude -p` 2.1.302, ids and uuids left out.
        let lines = [
            r#"{"type":"system","subtype":"background_tasks_changed","tasks":[{"task_id":"bmd3","task_type":"local_bash","description":"Run the new Horobi integration tests"},{"task_id":"bsio","task_type":"local_bash","description":"Horobi integration test result"}]}"#,
            r#"{"type":"system","subtype":"post_turn_summary","status_category":"review_ready","status_detail":"Horobi targeting fix written; waiting on engine test build","needs_action":""}"#,
            r#"{"type":"system","subtype":"background_tasks_changed","tasks":[]}"#,
            r#"{"type":"system","subtype":"thinking_tokens"}"#,
            r#"{"type":"tool_progress","tool_name":"Bash","elapsed_time_seconds":30,"heartbeat":true}"#,
        ];
        assert_eq!(
            parse_all(Cli::Claude, &lines),
            vec![
                AgentEvent::Background(vec![
                    "Run the new Horobi integration tests".into(),
                    "Horobi integration test result".into()
                ]),
                AgentEvent::Status(
                    "Horobi targeting fix written; waiting on engine test build".into()
                ),
                AgentEvent::Background(Vec::new()),
            ]
        );
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
    fn antigravity_tool_then_reply() {
        let reply = "The secret word is \"pelican\".".to_string();
        assert_eq!(
            parse_all(Cli::Antigravity, &AGY_RUN),
            vec![
                AgentEvent::ToolUse {
                    name: "view_file".into(),
                    detail: "/spec/NOTE.md".into()
                },
                AgentEvent::Metrics(RunMetrics {
                    input_tokens: Some(24_507),
                    output_tokens: Some(186),
                    duration_ms: Some(3400),
                    ..RunMetrics::default()
                }),
                AgentEvent::Text(reply.clone()),
                AgentEvent::Finished {
                    ok: true,
                    summary: Some(reply)
                },
            ]
        );
    }

    #[test]
    fn antigravity_failures() {
        let lines = [
            r#"{"event":"step_update","step_update":{"state":"DONE","step_type":"tool","tool_name":"run_command","tool_info":{"name":"run_command","error":{"type":"denied","message":"permission denied"}}}}"#,
            r#"{"event":"result","result":{"status":"ERROR","error":{"type":"auth","message":"not signed in"}}}"#,
            r#"{"event":"result","result":{"status":"CANCELED"}}"#,
        ];
        assert_eq!(
            parse_all(Cli::Antigravity, &lines),
            vec![
                AgentEvent::Notice("permission denied".into()),
                AgentEvent::Finished {
                    ok: false,
                    summary: Some("not signed in".into())
                },
                AgentEvent::Finished {
                    ok: false,
                    summary: Some("CANCELED".into())
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
    fn claude_init_names_the_model_and_result_reports_metrics() {
        let init = r#"{"type":"system","subtype":"init","model":"claude-opus-4-7","tools":[]}"#;
        assert_eq!(
            parse_line(Cli::Claude, init),
            vec![AgentEvent::Model("claude-opus-4-7".into())]
        );
        let result = r#"{"type":"result","is_error":false,"result":"ok","duration_ms":4200,"total_cost_usd":0.0123456,"usage":{"input_tokens":10,"output_tokens":20,"cache_read_input_tokens":300,"cache_creation_input_tokens":40},"modelUsage":{"claude-opus-4-7":{},"claude-haiku-4-5":{}}}"#;
        assert_eq!(
            parse_line(Cli::Claude, result),
            vec![
                AgentEvent::Metrics(RunMetrics {
                    input_tokens: Some(10),
                    output_tokens: Some(20),
                    cache_read_tokens: Some(300),
                    cache_write_tokens: Some(40),
                    cost_micro_usd: Some(12_346),
                    duration_ms: Some(4200),
                    models: vec!["claude-haiku-4-5".into(), "claude-opus-4-7".into()],
                }),
                AgentEvent::Finished {
                    ok: true,
                    summary: Some("ok".into())
                },
            ]
        );
    }

    #[test]
    fn codex_turn_completed_reports_tokens() {
        let line = r#"{"type":"turn.completed","usage":{"input_tokens":100,"cached_input_tokens":60,"output_tokens":7}}"#;
        assert_eq!(
            parse_line(Cli::Codex, line),
            vec![
                AgentEvent::Metrics(RunMetrics {
                    input_tokens: Some(100),
                    output_tokens: Some(7),
                    cache_read_tokens: Some(60),
                    ..RunMetrics::default()
                }),
                AgentEvent::Finished {
                    ok: true,
                    summary: None
                },
            ]
        );
    }

    #[test]
    fn antigravity_result_reports_tokens_and_duration() {
        let events = parse_line(Cli::Antigravity, AGY_RUN[5]);
        assert_eq!(
            events[0],
            AgentEvent::Metrics(RunMetrics {
                input_tokens: Some(24_507),
                output_tokens: Some(186),
                duration_ms: Some(3400),
                ..RunMetrics::default()
            })
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

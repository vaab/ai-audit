use anyhow::{Context, Result};
use chrono::{DateTime, TimeDelta, Utc};
use regex::Regex;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;

use super::session::ToolUse;
use crate::OutputFormat;

#[derive(Debug, Serialize)]
pub struct PermissionEvent {
    pub timestamp: DateTime<Utc>,
    #[serde(flatten)]
    pub event_type: PermissionEventType,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PermissionEventType {
    Request {
        tool: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        input: Option<HashMap<String, Value>>,
    },
    Granted {
        destination: String,
        rules: Vec<String>,
    },
    DirectoryAccess {
        destination: String,
        directories: Vec<String>,
    },
    ModeChange {
        mode: String,
    },
}

fn parse_timestamp(ts_str: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(ts_str)
        .map(|dt| dt.with_timezone(&Utc))
        .context("Failed to parse timestamp")
}

/// Parse permission events from debug log content
pub fn parse_events(content: &str) -> Result<Vec<PermissionEvent>> {
    let mut events = Vec::new();

    // Regex for permission request (captures tool name)
    let request_re = Regex::new(
        r"^(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z).*executePermissionRequestHooks called for tool: (\w+)",
    )?;

    // Regex for permission grant (rules)
    let grant_re = Regex::new(
        r#"^(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z).*Applying permission update: Adding \d+ allow rule\(s\) to destination '([^']+)': \[([^\]]+)\]"#,
    )?;

    // Regex for directory access
    let dir_re = Regex::new(
        r#"^(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z).*Applying permission update: Adding \d+ directory with destination '([^']+)': \[([^\]]+)\]"#,
    )?;

    // Regex for mode change
    let mode_re = Regex::new(
        r#"^(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z).*Applying permission update: Setting mode to '([^']+)'"#,
    )?;

    for line in content.lines() {
        // Check for permission request
        if let Some(caps) = request_re.captures(line) {
            let timestamp = parse_timestamp(&caps[1])?;
            let tool = caps[2].to_string();
            events.push(PermissionEvent {
                timestamp,
                event_type: PermissionEventType::Request { tool, input: None },
            });
            continue;
        }

        // Check for permission grant
        if let Some(caps) = grant_re.captures(line) {
            let timestamp = parse_timestamp(&caps[1])?;
            let destination = caps[2].to_string();
            let rules_str = &caps[3];

            let rules: Vec<String> = rules_str
                .split("\",\"")
                .map(|s| s.trim_matches('"').to_string())
                .collect();

            events.push(PermissionEvent {
                timestamp,
                event_type: PermissionEventType::Granted { destination, rules },
            });
            continue;
        }

        // Check for directory access
        if let Some(caps) = dir_re.captures(line) {
            let timestamp = parse_timestamp(&caps[1])?;
            let destination = caps[2].to_string();
            let dirs_str = &caps[3];

            let directories: Vec<String> = dirs_str
                .split("\",\"")
                .map(|s| s.trim_matches('"').to_string())
                .collect();

            events.push(PermissionEvent {
                timestamp,
                event_type: PermissionEventType::DirectoryAccess {
                    destination,
                    directories,
                },
            });
            continue;
        }

        // Check for mode change
        if let Some(caps) = mode_re.captures(line) {
            let timestamp = parse_timestamp(&caps[1])?;
            let mode = caps[2].to_string();

            events.push(PermissionEvent {
                timestamp,
                event_type: PermissionEventType::ModeChange { mode },
            });
        }
    }

    Ok(events)
}

/// Enrich permission events with tool input from session log
///
/// Correlates permission REQUEST events with tool_use entries from
/// the session JSONL by matching tool name and timestamp (within tolerance).
pub fn enrich_with_session(events: &mut [PermissionEvent], tool_uses: &[ToolUse]) {
    // Tolerance for timestamp matching (permission hook fires shortly after tool_use)
    let tolerance = TimeDelta::milliseconds(500);

    for event in events.iter_mut() {
        if let PermissionEventType::Request { tool, input } = &mut event.event_type {
            // Find the most recent tool_use with same tool name before this timestamp
            let matching_use = tool_uses
                .iter()
                .filter(|tu| {
                    tu.tool == *tool
                        && tu.timestamp <= event.timestamp
                        && event.timestamp - tu.timestamp <= tolerance
                })
                .max_by_key(|tu| tu.timestamp);

            if let Some(tu) = matching_use {
                *input = Some(tu.input.clone());
            }
        }
    }
}

/// Display permission events to stdout
pub fn display_events(events: &[PermissionEvent], format: OutputFormat) {
    match format {
        OutputFormat::Json => display_json(events),
        OutputFormat::Nul => display_nul(events),
        OutputFormat::Human => display_human(events),
    }
}

fn display_json(events: &[PermissionEvent]) {
    use serde_json::json;

    for event in events {
        // Convert timestamp to UTC float seconds since epoch
        let ts = event.timestamp.timestamp() as f64
            + event.timestamp.timestamp_subsec_nanos() as f64 / 1_000_000_000.0;

        // Field order: timestamp first, then type, then other fields
        let obj = match &event.event_type {
            PermissionEventType::Request { tool, input } => {
                let mut obj = serde_json::json!({
                    "timestamp": ts,
                    "type": "request",
                    "tool": tool
                });
                if let Some(input_map) = input {
                    obj["input"] = serde_json::json!(input_map);
                }
                obj
            }
            PermissionEventType::Granted { destination, rules } => json!({
                "timestamp": ts,
                "type": "granted",
                "destination": destination,
                "rules": rules
            }),
            PermissionEventType::DirectoryAccess {
                destination,
                directories,
            } => json!({
                "timestamp": ts,
                "type": "dir_access",
                "destination": destination,
                "directories": directories
            }),
            PermissionEventType::ModeChange { mode } => json!({
                "timestamp": ts,
                "type": "mode_change",
                "mode": mode
            }),
        };
        println!("{}", obj);
    }
}

fn display_nul(events: &[PermissionEvent]) {
    use std::io::{self, Write};
    let stdout = io::stdout();
    let mut handle = stdout.lock();

    for event in events {
        // Convert timestamp to UTC float seconds since epoch
        let ts = event.timestamp.timestamp() as f64
            + event.timestamp.timestamp_subsec_nanos() as f64 / 1_000_000_000.0;

        let (event_type, details) = match &event.event_type {
            PermissionEventType::Request { tool, input } => {
                let detail = format_nul_request_detail(tool, input.as_ref());
                ("REQUEST", detail)
            }
            PermissionEventType::Granted { destination, rules } => {
                ("GRANTED", format!("{}\t{}", destination, rules.join(",")))
            }
            PermissionEventType::DirectoryAccess {
                destination,
                directories,
            } => (
                "DIR_ACCESS",
                format!("{}\t{}", destination, directories.join(",")),
            ),
            PermissionEventType::ModeChange { mode } => ("MODE", mode.clone()),
        };
        let _ = write!(handle, "{}\t{}\t{}\0", ts, event_type, details);
    }
}

/// Format request detail for NUL-separated output (full detail for machine parsing)
fn format_nul_request_detail(tool: &str, input: Option<&HashMap<String, Value>>) -> String {
    let input = match input {
        Some(i) => i,
        None => return tool.to_string(),
    };

    match tool {
        "Bash" => {
            if let Some(Value::String(cmd)) = input.get("command") {
                format!("{}\t{}", tool, cmd)
            } else {
                tool.to_string()
            }
        }
        "Read" | "Write" | "Edit" => {
            if let Some(Value::String(path)) = input.get("file_path") {
                format!("{}\t{}", tool, path)
            } else {
                tool.to_string()
            }
        }
        "Glob" => {
            if let Some(Value::String(pattern)) = input.get("pattern") {
                format!("{}\t{}", tool, pattern)
            } else {
                tool.to_string()
            }
        }
        "Grep" => {
            if let Some(Value::String(pattern)) = input.get("pattern") {
                format!("{}\t{}", tool, pattern)
            } else {
                tool.to_string()
            }
        }
        _ => tool.to_string(),
    }
}

/// Format request detail for human-readable output
fn format_request_detail(tool: &str, input: Option<&HashMap<String, Value>>) -> String {
    let input = match input {
        Some(i) => i,
        None => return tool.to_string(),
    };

    match tool {
        "Bash" => {
            if let Some(Value::String(cmd)) = input.get("command") {
                // Truncate long commands
                let display = if cmd.len() > 60 {
                    format!("{}...", &cmd[..57])
                } else {
                    cmd.clone()
                };
                format!("Bash: {}", display)
            } else {
                tool.to_string()
            }
        }
        "Read" | "Write" | "Edit" => {
            if let Some(Value::String(path)) = input.get("file_path") {
                format!("{}: {}", tool, path)
            } else {
                tool.to_string()
            }
        }
        "Glob" => {
            if let Some(Value::String(pattern)) = input.get("pattern") {
                format!("Glob: {}", pattern)
            } else {
                tool.to_string()
            }
        }
        "Grep" => {
            if let Some(Value::String(pattern)) = input.get("pattern") {
                format!("Grep: {}", pattern)
            } else {
                tool.to_string()
            }
        }
        _ => tool.to_string(),
    }
}

fn display_human(events: &[PermissionEvent]) {
    let mut skip_initial = true;

    for event in events {
        match &event.event_type {
            PermissionEventType::Request { tool, input } => {
                let detail = format_request_detail(tool, input.as_ref());
                println!(
                    "{:<24} {:<12} {}",
                    event.timestamp.format("%Y-%m-%d %H:%M:%S"),
                    "REQUEST",
                    detail
                );
            }
            PermissionEventType::Granted { destination, rules } => {
                // Skip initial userSettings load (typically has many rules)
                if skip_initial && destination == "userSettings" && rules.len() > 10 {
                    skip_initial = false;
                    println!(
                        "{:<24} {:<12} {} ({} pre-existing rules loaded)",
                        event.timestamp.format("%Y-%m-%d %H:%M:%S"),
                        "INIT",
                        destination,
                        rules.len()
                    );
                    continue;
                }

                let rules_display = if rules.len() <= 3 {
                    rules.join(", ")
                } else {
                    format!("{} (+{} more)", rules[..2].join(", "), rules.len() - 2)
                };
                println!(
                    "{:<24} {:<12} [{}] {}",
                    event.timestamp.format("%Y-%m-%d %H:%M:%S"),
                    "GRANTED",
                    destination,
                    rules_display
                );
            }
            PermissionEventType::DirectoryAccess {
                destination,
                directories,
            } => {
                println!(
                    "{:<24} {:<12} [{}] dirs: {}",
                    event.timestamp.format("%Y-%m-%d %H:%M:%S"),
                    "DIR_ACCESS",
                    destination,
                    directories.join(", ")
                );
            }
            PermissionEventType::ModeChange { mode } => {
                println!(
                    "{:<24} {:<12} mode -> {}",
                    event.timestamp.format("%Y-%m-%d %H:%M:%S"),
                    "MODE",
                    mode
                );
            }
        }
    }
}

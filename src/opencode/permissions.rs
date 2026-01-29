use anyhow::Result;
use chrono::{DateTime, TimeZone, Utc};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::fs;

use crate::OutputFormat;

#[derive(Debug, Clone, Serialize)]
pub struct PermissionEvent {
    pub timestamp: DateTime<Utc>,
    pub tool: String,
    pub pattern: String,
    pub action: String,
}

#[derive(Deserialize)]
struct PartFile {
    #[serde(rename = "sessionID")]
    session_id: String,
    #[serde(rename = "type")]
    part_type: String,
    tool: Option<String>,
    state: Option<ToolState>,
}

#[derive(Deserialize)]
struct ToolState {
    input: Option<serde_json::Value>,
    time: Option<TimeInfo>,
}

#[derive(Deserialize)]
struct TimeInfo {
    start: Option<i64>,
}

pub fn parse_events(session_id: &str) -> Result<Vec<PermissionEvent>> {
    let part_dir = super::part_dir();
    if !part_dir.exists() {
        return Ok(Vec::new());
    }

    let mut tool_calls: Vec<(DateTime<Utc>, String, String)> = Vec::new();

    for msg_entry in fs::read_dir(&part_dir)? {
        let msg_entry = msg_entry?;
        let msg_path = msg_entry.path();
        if !msg_path.is_dir() {
            continue;
        }

        for part_entry in fs::read_dir(&msg_path)? {
            let part_entry = part_entry?;
            let part_path = part_entry.path();
            if !part_path.extension().map_or(false, |e| e == "json") {
                continue;
            }

            if let Ok(content) = fs::read_to_string(&part_path) {
                if let Ok(part) = serde_json::from_str::<PartFile>(&content) {
                    if part.session_id != session_id || part.part_type != "tool" {
                        continue;
                    }

                    if let (Some(tool), Some(state)) = (part.tool, part.state) {
                        if let Some(time) = state.time {
                            if let Some(start_ms) = time.start {
                                let timestamp = Utc
                                    .timestamp_millis_opt(start_ms)
                                    .single()
                                    .unwrap_or_else(Utc::now);

                                let pattern = extract_pattern(&tool, &state.input);
                                tool_calls.push((timestamp, tool, pattern));
                            }
                        }
                    }
                }
            }
        }
    }

    let log_decisions = load_log_decisions()?;
    let mut events: Vec<PermissionEvent> = Vec::new();

    for (timestamp, tool, pattern) in tool_calls {
        let action = find_permission_decision(&log_decisions, &tool, &pattern, timestamp)
            .unwrap_or_else(|| "unknown".to_string());

        events.push(PermissionEvent {
            timestamp,
            tool,
            pattern,
            action,
        });
    }

    events.sort_by_key(|e| e.timestamp);
    Ok(events)
}

fn extract_pattern(tool: &str, input: &Option<serde_json::Value>) -> String {
    let input = match input {
        Some(v) => v,
        None => return String::new(),
    };

    match tool {
        "bash" => input
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        "read" | "write" | "edit" => input
            .get("filePath")
            .or_else(|| input.get("file_path"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        "glob" | "grep" => input
            .get("pattern")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        _ => serde_json::to_string(input).unwrap_or_default(),
    }
}

#[derive(Debug)]
struct LogDecision {
    timestamp: DateTime<Utc>,
    permission: String,
    pattern: String,
    action: String,
}

fn load_log_decisions() -> Result<Vec<LogDecision>> {
    let log_dir = super::log_dir();
    if !log_dir.exists() {
        return Ok(Vec::new());
    }

    let re = Regex::new(
        r#"^INFO\s+(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}).*service=permission permission=(\w+) pattern=(.+?) action=\{[^}]*"action":"(\w+)"[^}]*\} evaluated"#,
    )?;

    let mut decisions = Vec::new();

    let mut log_files: Vec<_> = fs::read_dir(&log_dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().map_or(false, |e| e == "log"))
        .collect();
    log_files.sort();

    for log_file in log_files {
        let content = match fs::read_to_string(&log_file) {
            Ok(c) => c,
            Err(_) => continue,
        };

        for line in content.lines() {
            if let Some(caps) = re.captures(line) {
                let ts_str = &caps[1];
                if let Ok(ts) =
                    DateTime::parse_from_str(&format!("{}+00:00", ts_str), "%Y-%m-%dT%H:%M:%S%:z")
                {
                    decisions.push(LogDecision {
                        timestamp: ts.with_timezone(&Utc),
                        permission: caps[2].to_string(),
                        pattern: caps[3].to_string(),
                        action: caps[4].to_string(),
                    });
                }
            }
        }
    }

    Ok(decisions)
}

fn find_permission_decision(
    decisions: &[LogDecision],
    tool: &str,
    pattern: &str,
    timestamp: DateTime<Utc>,
) -> Option<String> {
    let permission_type = match tool {
        "bash" => "bash",
        "read" => "read",
        "write" => "write",
        "edit" => "edit",
        "glob" => "glob",
        "grep" => "grep",
        _ => tool,
    };

    // Find a matching decision within a 5-second window
    let tolerance = chrono::Duration::seconds(5);

    for decision in decisions {
        if decision.permission != permission_type {
            continue;
        }

        let time_diff = if decision.timestamp > timestamp {
            decision.timestamp - timestamp
        } else {
            timestamp - decision.timestamp
        };

        if time_diff > tolerance {
            continue;
        }

        // Check if patterns match (log pattern might be truncated or slightly different)
        if decision.pattern == pattern
            || pattern.starts_with(&decision.pattern)
            || decision.pattern.starts_with(pattern)
        {
            return Some(decision.action.clone());
        }
    }

    None
}

pub fn display_events(events: &[PermissionEvent], format: OutputFormat) {
    match format {
        OutputFormat::Json => display_json(events),
        OutputFormat::Nul => display_nul(events),
        OutputFormat::Human => display_human(events),
    }
}

fn display_json(events: &[PermissionEvent]) {
    for event in events {
        let ts = event.timestamp.timestamp() as f64
            + event.timestamp.timestamp_subsec_nanos() as f64 / 1_000_000_000.0;
        println!(
            r#"{{"timestamp":{},"tool":"{}","pattern":"{}","action":"{}"}}"#,
            ts,
            event.tool,
            event.pattern.replace('\\', "\\\\").replace('"', "\\\""),
            event.action
        );
    }
}

fn display_nul(events: &[PermissionEvent]) {
    use std::io::{self, Write};
    let stdout = io::stdout();
    let mut handle = stdout.lock();

    for event in events {
        let ts = event.timestamp.timestamp() as f64
            + event.timestamp.timestamp_subsec_nanos() as f64 / 1_000_000_000.0;
        let _ = write!(
            handle,
            "{}\t{}\t{}\t{}\0",
            ts, event.tool, event.pattern, event.action
        );
    }
}

fn display_human(events: &[PermissionEvent]) {
    for event in events {
        let action_display = match event.action.as_str() {
            "allow" => "ALLOW",
            "ask" => "ASK",
            "deny" => "DENY",
            _ => &event.action,
        };

        let pattern_short = if event.pattern.len() > 60 {
            format!("{}...", &event.pattern[..57])
        } else {
            event.pattern.clone()
        };

        println!(
            "{:<24} {:<8} {:<12} {}",
            event.timestamp.format("%Y-%m-%d %H:%M:%S"),
            action_display,
            event.tool,
            pattern_short
        );
    }
}

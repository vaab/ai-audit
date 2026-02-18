use anyhow::Result;
use chrono::{DateTime, TimeZone, Utc};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::fs;

use crate::OutputFormat;

#[cfg(test)]
use rusqlite::Connection;

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
    // Collect tool calls from file-based scan
    let part_dir = super::part_dir();
    let file_calls = parse_events_from_files(&part_dir, session_id);

    // Collect tool calls from DB (if available)
    let db_calls = if super::db::db_exists() {
        super::db::open_db()
            .ok()
            .map(|conn| parse_events_from_db(&conn, session_id))
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    // Merge and dedup: DB wins on conflict
    let tool_calls = merge_tool_calls(file_calls, db_calls);

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

/// Extract tool calls from file-based part storage.
fn parse_events_from_files(
    part_dir: &std::path::Path,
    session_id: &str,
) -> Vec<(DateTime<Utc>, String, String)> {
    if !part_dir.exists() {
        return Vec::new();
    }

    let mut tool_calls = Vec::new();

    let msg_entries = match fs::read_dir(part_dir) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };

    for msg_entry in msg_entries {
        let msg_entry = match msg_entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let msg_path = msg_entry.path();
        if !msg_path.is_dir() {
            continue;
        }

        let part_entries = match fs::read_dir(&msg_path) {
            Ok(entries) => entries,
            Err(_) => continue,
        };

        for part_entry in part_entries {
            let part_entry = match part_entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            let part_path = part_entry.path();
            if part_path.extension().is_none_or(|e| e != "json") {
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

    tool_calls
}

/// Extract tool calls from the SQLite database for a session.
fn parse_events_from_db(
    conn: &rusqlite::Connection,
    session_id: &str,
) -> Vec<(DateTime<Utc>, String, String)> {
    let mut stmt = match conn
        .prepare("SELECT data FROM part WHERE session_id = ? ORDER BY time_created ASC")
    {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };

    let rows = match stmt.query_map([session_id], |row| {
        let data_str: String = row.get(0)?;
        Ok(data_str)
    }) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };

    let mut tool_calls = Vec::new();

    for row in rows {
        let data_str = match row {
            Ok(s) => s,
            Err(_) => continue,
        };

        let part: serde_json::Value = match serde_json::from_str(&data_str) {
            Ok(v) => v,
            Err(_) => continue,
        };

        if part.get("type").and_then(|v| v.as_str()) != Some("tool") {
            continue;
        }

        let tool = match part.get("tool").and_then(|v| v.as_str()) {
            Some(t) => t.to_string(),
            None => continue,
        };

        let state = match part.get("state") {
            Some(s) => s,
            None => continue,
        };

        let start_ms = match state
            .get("time")
            .and_then(|t| t.get("start"))
            .and_then(|v| v.as_i64())
        {
            Some(ms) => ms,
            None => continue,
        };

        let timestamp = Utc
            .timestamp_millis_opt(start_ms)
            .single()
            .unwrap_or_else(Utc::now);

        let input_val = state.get("input").cloned();
        let pattern = extract_pattern(&tool, &input_val);
        tool_calls.push((timestamp, tool, pattern));
    }

    tool_calls
}

/// Merge file-based and DB-based tool calls, deduplicating by
/// (timestamp, tool, pattern). DB wins on conflict.
fn merge_tool_calls(
    file_calls: Vec<(DateTime<Utc>, String, String)>,
    db_calls: Vec<(DateTime<Utc>, String, String)>,
) -> Vec<(DateTime<Utc>, String, String)> {
    use std::collections::HashSet;

    // Build a set of keys from DB calls (they win)
    let db_keys: HashSet<(i64, &str, &str)> = db_calls
        .iter()
        .map(|(ts, tool, pat)| (ts.timestamp_millis(), tool.as_str(), pat.as_str()))
        .collect();

    let mut merged: Vec<(DateTime<Utc>, String, String)> = Vec::new();

    // Add file calls that don't conflict with DB
    for call in &file_calls {
        let key = (call.0.timestamp_millis(), call.1.as_str(), call.2.as_str());
        if !db_keys.contains(&key) {
            merged.push(call.clone());
        }
    }

    // Add all DB calls
    merged.extend(db_calls);

    merged
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
        .filter(|p| p.extension().is_some_and(|e| e == "log"))
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
    let permission_type = tool;

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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::fs;
    use tempfile::tempdir;

    fn setup_test_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::opencode::db::create_schema(&conn).unwrap();
        conn
    }

    fn insert_part(
        conn: &Connection,
        id: &str,
        msg_id: &str,
        session_id: &str,
        ts: i64,
        data: &str,
    ) {
        conn.execute(
            "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![id, msg_id, session_id, ts, ts, data],
        )
        .unwrap();
    }

    #[test]
    fn test_parse_events_from_files_basic() {
        let temp = tempdir().unwrap();
        let part_dir = temp.path();

        // Create a part directory structure: part/<msg_id>/<part_id>.json
        let msg_dir = part_dir.join("msg_001");
        fs::create_dir_all(&msg_dir).unwrap();

        fs::write(
            msg_dir.join("prt_001.json"),
            r#"{"sessionID":"ses_test","type":"tool","tool":"bash","state":{"input":{"command":"ls -la"},"time":{"start":1705314600000}}}"#,
        ).unwrap();

        let calls = parse_events_from_files(part_dir, "ses_test");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].1, "bash");
        assert_eq!(calls[0].2, "ls -la");
    }

    #[test]
    fn test_parse_events_from_files_filters_session() {
        let temp = tempdir().unwrap();
        let part_dir = temp.path();

        let msg_dir = part_dir.join("msg_001");
        fs::create_dir_all(&msg_dir).unwrap();

        // Part for a different session
        fs::write(
            msg_dir.join("prt_001.json"),
            r#"{"sessionID":"ses_other","type":"tool","tool":"bash","state":{"input":{"command":"ls"},"time":{"start":1705314600000}}}"#,
        ).unwrap();

        let calls = parse_events_from_files(part_dir, "ses_test");
        assert!(calls.is_empty());
    }

    #[test]
    fn test_parse_events_from_db_basic() {
        let conn = setup_test_db();

        let data = r#"{"type":"tool","tool":"bash","state":{"input":{"command":"cargo test"},"time":{"start":1705314600000}}}"#;
        insert_part(&conn, "prt_001", "msg_001", "ses_db1", 1705314600000, data);

        let calls = parse_events_from_db(&conn, "ses_db1");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].1, "bash");
        assert_eq!(calls[0].2, "cargo test");
        assert_eq!(calls[0].0, Utc.timestamp_millis_opt(1705314600000).unwrap());
    }

    #[test]
    fn test_parse_events_from_db_skips_non_tool() {
        let conn = setup_test_db();

        let data = r#"{"type":"text","text":"Hello world"}"#;
        insert_part(&conn, "prt_001", "msg_001", "ses_db2", 1705314600000, data);

        let calls = parse_events_from_db(&conn, "ses_db2");
        assert!(calls.is_empty());
    }

    #[test]
    fn test_parse_events_from_db_filters_session() {
        let conn = setup_test_db();

        let data = r#"{"type":"tool","tool":"bash","state":{"input":{"command":"ls"},"time":{"start":1705314600000}}}"#;
        insert_part(
            &conn,
            "prt_001",
            "msg_001",
            "ses_other",
            1705314600000,
            data,
        );

        let calls = parse_events_from_db(&conn, "ses_db3");
        assert!(calls.is_empty());
    }

    #[test]
    fn test_parse_events_from_db_multiple_tools() {
        let conn = setup_test_db();

        let data1 = r#"{"type":"tool","tool":"bash","state":{"input":{"command":"ls"},"time":{"start":1705314600000}}}"#;
        let data2 = r#"{"type":"tool","tool":"read","state":{"input":{"filePath":"/tmp/test.rs"},"time":{"start":1705314601000}}}"#;
        insert_part(&conn, "prt_001", "msg_001", "ses_db4", 1705314600000, data1);
        insert_part(&conn, "prt_002", "msg_001", "ses_db4", 1705314601000, data2);

        let calls = parse_events_from_db(&conn, "ses_db4");
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].1, "bash");
        assert_eq!(calls[1].1, "read");
        assert_eq!(calls[1].2, "/tmp/test.rs");
    }

    #[test]
    fn test_merge_tool_calls_dedup_db_wins() {
        let ts = Utc.timestamp_millis_opt(1705314600000).unwrap();

        let file_calls = vec![
            (ts, "bash".to_string(), "ls -la".to_string()),
            (ts, "read".to_string(), "/tmp/file.rs".to_string()),
        ];

        let db_calls = vec![
            // Same timestamp+tool+pattern as file — DB wins (duplicate)
            (ts, "bash".to_string(), "ls -la".to_string()),
        ];

        let merged = merge_tool_calls(file_calls, db_calls);
        // Should have 2: the "read" from files + the "bash" from DB
        assert_eq!(merged.len(), 2);
    }

    #[test]
    fn test_merge_tool_calls_combines_unique() {
        let ts1 = Utc.timestamp_millis_opt(1705314600000).unwrap();
        let ts2 = Utc.timestamp_millis_opt(1705314601000).unwrap();

        let file_calls = vec![(ts1, "bash".to_string(), "ls".to_string())];
        let db_calls = vec![(ts2, "read".to_string(), "/tmp/f.rs".to_string())];

        let merged = merge_tool_calls(file_calls, db_calls);
        assert_eq!(merged.len(), 2);
    }

    #[test]
    fn test_merge_tool_calls_empty_sources() {
        let merged = merge_tool_calls(Vec::new(), Vec::new());
        assert!(merged.is_empty());

        let ts = Utc.timestamp_millis_opt(1705314600000).unwrap();
        let calls = vec![(ts, "bash".to_string(), "ls".to_string())];

        let merged = merge_tool_calls(calls.clone(), Vec::new());
        assert_eq!(merged.len(), 1);

        let merged = merge_tool_calls(Vec::new(), calls);
        assert_eq!(merged.len(), 1);
    }

    #[test]
    fn test_parse_events_from_db_skips_missing_time() {
        let conn = setup_test_db();

        // Tool part without time.start
        let data = r#"{"type":"tool","tool":"bash","state":{"input":{"command":"ls"}}}"#;
        insert_part(&conn, "prt_001", "msg_001", "ses_db5", 1705314600000, data);

        let calls = parse_events_from_db(&conn, "ses_db5");
        assert!(calls.is_empty());
    }

    #[test]
    fn test_parse_events_from_files_nonexistent_dir() {
        let calls = parse_events_from_files(std::path::Path::new("/nonexistent/path"), "ses_test");
        assert!(calls.is_empty());
    }
}

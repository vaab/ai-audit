use anyhow::Result;
use chrono::{DateTime, TimeZone, Utc};
use rusqlite::Connection;
use serde::Deserialize;
use serde_json::Value;
use std::fs;
use std::path::Path;

use crate::transcript::{EntryType, Role, TranscriptEntry};

/// Parse a full conversation transcript from an OpenCode session.
///
/// Reads from both file-based storage and SQLite database, merging and
/// deduplicating entries. If both sources have data, entries are merged
/// by timestamp+role+content. If only one source has data, that is used.
pub fn parse_transcript(session_id: &str) -> Result<Vec<TranscriptEntry>> {
    let storage_dir = crate::opencode_data_dir().join("storage");
    let file_entries = parse_transcript_from_dir(&storage_dir, session_id).ok();
    let db_entries = parse_transcript_from_db(session_id).ok();

    match (file_entries, db_entries) {
        (Some(fe), Some(de)) if !fe.is_empty() && !de.is_empty() => {
            Ok(merge_transcript_entries(fe, de))
        }
        (Some(fe), _) if !fe.is_empty() => Ok(fe),
        (_, Some(de)) if !de.is_empty() => Ok(de),
        // Both empty or both errored — try file-based for the error message
        _ => {
            let storage_dir = crate::opencode_data_dir().join("storage");
            parse_transcript_from_dir(&storage_dir, session_id)
        }
    }
}

/// Parse transcript from the SQLite database.
pub fn parse_transcript_from_db(session_id: &str) -> Result<Vec<TranscriptEntry>> {
    let conn = super::db::open_db()?;
    parse_transcript_from_conn(&conn, session_id)
}

/// Parse transcript from a database connection (testable).
pub fn parse_transcript_from_conn(
    conn: &Connection,
    session_id: &str,
) -> Result<Vec<TranscriptEntry>> {
    let messages = super::db::get_messages_for_session(conn, session_id)?;
    if messages.is_empty() {
        anyhow::bail!("No messages found for session: {}", session_id);
    }

    let mut entries = Vec::new();

    for (msg_id, data) in &messages {
        let role = match data
            .get("role")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
        {
            "user" => Role::User,
            "assistant" => Role::Assistant,
            "system" => Role::System,
            _ => continue,
        };

        let time_created_ms = data
            .get("time")
            .and_then(|t| t.get("created"))
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let msg_timestamp = Utc
            .timestamp_millis_opt(time_created_ms)
            .single()
            .unwrap_or_else(Utc::now);

        let parts = super::db::get_parts_for_message(conn, msg_id)?;

        for part in &parts {
            entries_from_part(part, &role, msg_timestamp, &mut entries);
        }
    }

    entries.sort_by_key(|e| e.timestamp);
    Ok(entries)
}

/// Convert a part JSON value into transcript entries.
///
/// Shared logic used by both file-based and DB-based parsing.
fn entries_from_part(
    part: &Value,
    role: &Role,
    msg_timestamp: DateTime<Utc>,
    entries: &mut Vec<TranscriptEntry>,
) {
    let part_type = part.get("type").and_then(|v| v.as_str()).unwrap_or("");

    // Get part timestamp if available, otherwise use message timestamp
    let part_timestamp = part
        .get("time")
        .and_then(|t| t.get("start"))
        .and_then(|v| v.as_i64())
        .and_then(|ms| Utc.timestamp_millis_opt(ms).single())
        .unwrap_or(msg_timestamp);

    match part_type {
        "text" => {
            if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                if !text.trim().is_empty() {
                    entries.push(TranscriptEntry {
                        timestamp: part_timestamp,
                        role: role.clone(),
                        entry_type: EntryType::Text,
                        content: text.to_string(),
                        tool_name: None,
                        tool_input: None,
                    });
                }
            }
        }
        "tool" => {
            let tool_name = part
                .get("tool")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();

            let state = part.get("state");
            let input = state.and_then(|s| s.get("input")).cloned();
            let output = state
                .and_then(|s| s.get("output"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            // ToolUse entry
            entries.push(TranscriptEntry {
                timestamp: part_timestamp,
                role: role.clone(),
                entry_type: EntryType::ToolUse,
                content: String::new(),
                tool_name: Some(tool_name),
                tool_input: input,
            });

            // ToolResult entry (use end time if available)
            let result_timestamp = part
                .get("time")
                .and_then(|t| t.get("end"))
                .and_then(|v| v.as_i64())
                .and_then(|ms| Utc.timestamp_millis_opt(ms).single())
                .unwrap_or(part_timestamp);

            entries.push(TranscriptEntry {
                timestamp: result_timestamp,
                role: role.clone(),
                entry_type: EntryType::ToolResult,
                content: output,
                tool_name: None,
                tool_input: None,
            });
        }
        "reasoning" => {
            if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                if !text.trim().is_empty() {
                    entries.push(TranscriptEntry {
                        timestamp: part_timestamp,
                        role: role.clone(),
                        entry_type: EntryType::Thinking,
                        content: text.to_string(),
                        tool_name: None,
                        tool_input: None,
                    });
                }
            }
        }
        // Skip internal metadata types
        "step-start" | "step-finish" | "compaction" => {}
        _ => {}
    }
}

/// Merge transcript entries from file and DB sources, deduplicating by
/// timestamp + role + entry_type + content. DB entries win on conflict.
fn merge_transcript_entries(
    file_entries: Vec<TranscriptEntry>,
    db_entries: Vec<TranscriptEntry>,
) -> Vec<TranscriptEntry> {
    use std::collections::HashSet;

    // Build a dedup key set from DB entries (the preferred source)
    // Use owned strings to avoid borrow issues.
    let db_keys: HashSet<(i64, String, String, String)> = db_entries
        .iter()
        .map(|e| {
            (
                e.timestamp.timestamp_millis(),
                e.role.as_str().to_string(),
                e.entry_type.as_str().to_string(),
                e.content.clone(),
            )
        })
        .collect();

    let mut merged = db_entries;

    // Add file entries that don't exist in DB
    for entry in file_entries {
        let key = (
            entry.timestamp.timestamp_millis(),
            entry.role.as_str().to_string(),
            entry.entry_type.as_str().to_string(),
            entry.content.clone(),
        );
        if !db_keys.contains(&key) {
            merged.push(entry);
        }
    }

    merged.sort_by_key(|e| e.timestamp);
    merged
}

fn parse_transcript_from_dir(storage_dir: &Path, session_id: &str) -> Result<Vec<TranscriptEntry>> {
    let message_dir = storage_dir.join("message").join(session_id);
    let part_dir = storage_dir.join("part");

    if !message_dir.exists() {
        anyhow::bail!("No messages found for session: {}", session_id);
    }

    // Collect and sort message files alphabetically (IDs are chronologically sortable)
    let mut msg_files: Vec<_> = fs::read_dir(&message_dir)?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "json"))
        .collect();
    msg_files.sort_by_key(|e| e.file_name());

    let mut entries = Vec::new();

    for msg_entry in &msg_files {
        let msg_path = msg_entry.path();
        let content = match fs::read_to_string(&msg_path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let message: MessageMeta = match serde_json::from_str(&content) {
            Ok(m) => m,
            Err(_) => continue,
        };

        let role = match message.role.as_deref() {
            Some("user") => Role::User,
            Some("assistant") => Role::Assistant,
            Some("system") => Role::System,
            _ => continue,
        };

        let msg_timestamp = Utc
            .timestamp_millis_opt(message.time.created)
            .single()
            .unwrap_or_else(Utc::now);

        // Read parts for this message
        let msg_part_dir = part_dir.join(&message.id);
        if !msg_part_dir.exists() {
            continue;
        }

        let mut part_files: Vec<_> = fs::read_dir(&msg_part_dir)?
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "json"))
            .collect();
        part_files.sort_by_key(|e| e.file_name());

        for part_entry in &part_files {
            let part_path = part_entry.path();
            let part_content = match fs::read_to_string(&part_path) {
                Ok(c) => c,
                Err(_) => continue,
            };

            let part: Value = match serde_json::from_str(&part_content) {
                Ok(v) => v,
                Err(_) => continue,
            };

            entries_from_part(&part, &role, msg_timestamp, &mut entries);
        }
    }

    // Already sorted by message/part file order, but ensure timestamp ordering
    entries.sort_by_key(|e| e.timestamp);
    Ok(entries)
}

#[derive(Debug, Deserialize)]
struct MessageMeta {
    id: String,
    role: Option<String>,
    time: TimeMeta,
}

#[derive(Debug, Deserialize)]
struct TimeMeta {
    created: i64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    /// Create minimal OpenCode storage for testing.
    fn create_test_storage(
        base: &Path,
        session_id: &str,
        messages: &[(&str, &str, i64, &[(&str, &str)])],
        // (msg_id, role, timestamp_ms, parts: [(part_id, part_json)])
    ) {
        let message_dir = base.join("message").join(session_id);
        let part_dir = base.join("part");
        fs::create_dir_all(&message_dir).unwrap();

        for (msg_id, role, ts, parts) in messages {
            let msg_json = format!(
                r#"{{"id":"{}","sessionID":"{}","role":"{}","time":{{"created":{}}}}}"#,
                msg_id, session_id, role, ts
            );
            fs::write(message_dir.join(format!("{}.json", msg_id)), msg_json).unwrap();

            let msg_part_dir = part_dir.join(msg_id);
            fs::create_dir_all(&msg_part_dir).unwrap();

            for (part_id, part_json) in *parts {
                fs::write(msg_part_dir.join(format!("{}.json", part_id)), part_json).unwrap();
            }
        }
    }

    #[test]
    fn test_parse_text_part() {
        let temp = tempdir().unwrap();
        let storage = temp.path();

        create_test_storage(
            storage,
            "ses_test1",
            &[(
                "msg_001",
                "user",
                1705314600000,
                &[(
                    "prt_001",
                    r#"{"id":"prt_001","sessionID":"ses_test1","messageID":"msg_001","type":"text","text":"Hello world"}"#,
                )],
            )],
        );

        let entries = parse_transcript_from_dir(storage, "ses_test1").unwrap();
        assert_eq!(entries.len(), 1);
        assert!(matches!(entries[0].role, Role::User));
        assert!(matches!(entries[0].entry_type, EntryType::Text));
        assert_eq!(entries[0].content, "Hello world");
    }

    #[test]
    fn test_parse_tool_part() {
        let temp = tempdir().unwrap();
        let storage = temp.path();

        let tool_json = r#"{
            "id": "prt_002",
            "sessionID": "ses_test1",
            "messageID": "msg_002",
            "type": "tool",
            "tool": "Bash",
            "state": {
                "status": "completed",
                "input": {"command": "ls"},
                "output": "file1.txt\nfile2.txt",
                "time": {"start": 1705314601000, "end": 1705314602000}
            },
            "time": {"start": 1705314601000, "end": 1705314602000}
        }"#;

        create_test_storage(
            storage,
            "ses_test1",
            &[(
                "msg_002",
                "assistant",
                1705314601000,
                &[("prt_002", tool_json)],
            )],
        );

        let entries = parse_transcript_from_dir(storage, "ses_test1").unwrap();
        assert_eq!(entries.len(), 2);

        // First: ToolUse
        assert!(matches!(entries[0].entry_type, EntryType::ToolUse));
        assert_eq!(entries[0].tool_name.as_deref(), Some("Bash"));
        assert!(entries[0].tool_input.is_some());

        // Second: ToolResult
        assert!(matches!(entries[1].entry_type, EntryType::ToolResult));
        assert_eq!(entries[1].content, "file1.txt\nfile2.txt");
    }

    #[test]
    fn test_parse_reasoning_part() {
        let temp = tempdir().unwrap();
        let storage = temp.path();

        let reasoning_json = r#"{
            "id": "prt_003",
            "sessionID": "ses_test1",
            "messageID": "msg_003",
            "type": "reasoning",
            "text": "Let me think about this...",
            "time": {"start": 1705314603000, "end": 1705314604000}
        }"#;

        create_test_storage(
            storage,
            "ses_test1",
            &[(
                "msg_003",
                "assistant",
                1705314603000,
                &[("prt_003", reasoning_json)],
            )],
        );

        let entries = parse_transcript_from_dir(storage, "ses_test1").unwrap();
        assert_eq!(entries.len(), 1);
        assert!(matches!(entries[0].entry_type, EntryType::Thinking));
        assert_eq!(entries[0].content, "Let me think about this...");
    }

    #[test]
    fn test_skip_step_parts() {
        let temp = tempdir().unwrap();
        let storage = temp.path();

        create_test_storage(
            storage,
            "ses_test1",
            &[(
                "msg_004",
                "assistant",
                1705314605000,
                &[
                    (
                        "prt_001",
                        r#"{"id":"prt_001","sessionID":"ses_test1","messageID":"msg_004","type":"step-start","snapshot":"abc"}"#,
                    ),
                    (
                        "prt_002",
                        r#"{"id":"prt_002","sessionID":"ses_test1","messageID":"msg_004","type":"text","text":"Actual content"}"#,
                    ),
                    (
                        "prt_003",
                        r#"{"id":"prt_003","sessionID":"ses_test1","messageID":"msg_004","type":"step-finish","reason":"done"}"#,
                    ),
                ],
            )],
        );

        let entries = parse_transcript_from_dir(storage, "ses_test1").unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].content, "Actual content");
    }

    #[test]
    fn test_ordering() {
        let temp = tempdir().unwrap();
        let storage = temp.path();

        create_test_storage(
            storage,
            "ses_test1",
            &[
                (
                    "msg_002",
                    "assistant",
                    1705314601000,
                    &[(
                        "prt_001",
                        r#"{"id":"prt_001","sessionID":"ses_test1","messageID":"msg_002","type":"text","text":"Response"}"#,
                    )],
                ),
                (
                    "msg_001",
                    "user",
                    1705314600000,
                    &[(
                        "prt_001",
                        r#"{"id":"prt_001","sessionID":"ses_test1","messageID":"msg_001","type":"text","text":"Question"}"#,
                    )],
                ),
            ],
        );

        let entries = parse_transcript_from_dir(storage, "ses_test1").unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries[0].timestamp < entries[1].timestamp);
        assert_eq!(entries[0].content, "Question");
        assert_eq!(entries[1].content, "Response");
    }

    // --- DB-based transcript tests ---

    fn setup_transcript_db() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        super::super::db::create_schema(&conn).unwrap();
        conn
    }

    fn insert_msg(conn: &rusqlite::Connection, id: &str, session_id: &str, role: &str, ts: i64) {
        let data = format!(r#"{{"role":"{}","time":{{"created":{}}}}}"#, role, ts);
        conn.execute(
            "INSERT INTO message (id, session_id, time_created, time_updated, data) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![id, session_id, ts, ts, data],
        ).unwrap();
    }

    fn insert_prt(
        conn: &rusqlite::Connection,
        id: &str,
        msg_id: &str,
        session_id: &str,
        ts: i64,
        data: &str,
    ) {
        conn.execute(
            "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![id, msg_id, session_id, ts, ts, data],
        ).unwrap();
    }

    #[test]
    fn test_parse_transcript_from_db_text() {
        let conn = setup_transcript_db();
        insert_msg(&conn, "msg_001", "ses_db1", "user", 1705314600000);
        insert_prt(
            &conn,
            "prt_001",
            "msg_001",
            "ses_db1",
            1705314600000,
            r#"{"type":"text","text":"Hello from DB"}"#,
        );

        let entries = parse_transcript_from_conn(&conn, "ses_db1").unwrap();
        assert_eq!(entries.len(), 1);
        assert!(matches!(entries[0].role, Role::User));
        assert!(matches!(entries[0].entry_type, EntryType::Text));
        assert_eq!(entries[0].content, "Hello from DB");
    }

    #[test]
    fn test_parse_transcript_from_db_tool() {
        let conn = setup_transcript_db();
        insert_msg(&conn, "msg_001", "ses_db2", "assistant", 1705314601000);
        insert_prt(
            &conn,
            "prt_001",
            "msg_001",
            "ses_db2",
            1705314601000,
            r#"{"type":"tool","tool":"Bash","state":{"input":{"command":"ls"},"output":"file.txt"},"time":{"start":1705314601000,"end":1705314602000}}"#,
        );

        let entries = parse_transcript_from_conn(&conn, "ses_db2").unwrap();
        assert_eq!(entries.len(), 2);
        assert!(matches!(entries[0].entry_type, EntryType::ToolUse));
        assert_eq!(entries[0].tool_name.as_deref(), Some("Bash"));
        assert!(matches!(entries[1].entry_type, EntryType::ToolResult));
        assert_eq!(entries[1].content, "file.txt");
    }

    #[test]
    fn test_parse_transcript_from_db_reasoning() {
        let conn = setup_transcript_db();
        insert_msg(&conn, "msg_001", "ses_db3", "assistant", 1705314603000);
        insert_prt(
            &conn,
            "prt_001",
            "msg_001",
            "ses_db3",
            1705314603000,
            r#"{"type":"reasoning","text":"Thinking deeply...","time":{"start":1705314603000}}"#,
        );

        let entries = parse_transcript_from_conn(&conn, "ses_db3").unwrap();
        assert_eq!(entries.len(), 1);
        assert!(matches!(entries[0].entry_type, EntryType::Thinking));
        assert_eq!(entries[0].content, "Thinking deeply...");
    }

    #[test]
    fn test_parse_transcript_from_db_skips_step_parts() {
        let conn = setup_transcript_db();
        insert_msg(&conn, "msg_001", "ses_db4", "assistant", 1705314605000);
        insert_prt(
            &conn,
            "prt_001",
            "msg_001",
            "ses_db4",
            1705314605000,
            r#"{"type":"step-start","snapshot":"abc"}"#,
        );
        insert_prt(
            &conn,
            "prt_002",
            "msg_001",
            "ses_db4",
            1705314605100,
            r#"{"type":"text","text":"Actual content"}"#,
        );
        insert_prt(
            &conn,
            "prt_003",
            "msg_001",
            "ses_db4",
            1705314605200,
            r#"{"type":"step-finish","reason":"done"}"#,
        );

        let entries = parse_transcript_from_conn(&conn, "ses_db4").unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].content, "Actual content");
    }

    #[test]
    fn test_parse_transcript_from_db_ordering() {
        let conn = setup_transcript_db();
        // Insert in reverse order — should still be sorted by timestamp
        insert_msg(&conn, "msg_002", "ses_db5", "assistant", 1705314601000);
        insert_msg(&conn, "msg_001", "ses_db5", "user", 1705314600000);
        insert_prt(
            &conn,
            "prt_002",
            "msg_002",
            "ses_db5",
            1705314601000,
            r#"{"type":"text","text":"Response"}"#,
        );
        insert_prt(
            &conn,
            "prt_001",
            "msg_001",
            "ses_db5",
            1705314600000,
            r#"{"type":"text","text":"Question"}"#,
        );

        let entries = parse_transcript_from_conn(&conn, "ses_db5").unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries[0].timestamp < entries[1].timestamp);
        assert_eq!(entries[0].content, "Question");
        assert_eq!(entries[1].content, "Response");
    }

    #[test]
    fn test_parse_transcript_from_db_empty_session() {
        let conn = setup_transcript_db();
        let result = parse_transcript_from_conn(&conn, "ses_missing");
        assert!(result.is_err());
    }

    // --- Merge tests ---

    #[test]
    fn test_merge_transcript_entries_dedup() {
        let ts = Utc.timestamp_millis_opt(1705314600000).unwrap();

        let file_entries = vec![TranscriptEntry {
            timestamp: ts,
            role: Role::User,
            entry_type: EntryType::Text,
            content: "Hello".to_string(),
            tool_name: None,
            tool_input: None,
        }];

        let db_entries = vec![TranscriptEntry {
            timestamp: ts,
            role: Role::User,
            entry_type: EntryType::Text,
            content: "Hello".to_string(),
            tool_name: None,
            tool_input: None,
        }];

        let merged = merge_transcript_entries(file_entries, db_entries);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].content, "Hello");
    }

    #[test]
    fn test_merge_transcript_entries_combines_unique() {
        let ts1 = Utc.timestamp_millis_opt(1705314600000).unwrap();
        let ts2 = Utc.timestamp_millis_opt(1705314601000).unwrap();

        let file_entries = vec![TranscriptEntry {
            timestamp: ts1,
            role: Role::User,
            entry_type: EntryType::Text,
            content: "From files only".to_string(),
            tool_name: None,
            tool_input: None,
        }];

        let db_entries = vec![TranscriptEntry {
            timestamp: ts2,
            role: Role::Assistant,
            entry_type: EntryType::Text,
            content: "From DB only".to_string(),
            tool_name: None,
            tool_input: None,
        }];

        let merged = merge_transcript_entries(file_entries, db_entries);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].content, "From files only");
        assert_eq!(merged[1].content, "From DB only");
    }

    #[test]
    fn test_merge_transcript_entries_db_wins_on_conflict() {
        let ts = Utc.timestamp_millis_opt(1705314600000).unwrap();

        // Same timestamp+role+type+content = duplicate, DB version kept
        let file_entries = vec![
            TranscriptEntry {
                timestamp: ts,
                role: Role::User,
                entry_type: EntryType::Text,
                content: "Same content".to_string(),
                tool_name: None,
                tool_input: None,
            },
            TranscriptEntry {
                timestamp: ts,
                role: Role::User,
                entry_type: EntryType::Text,
                content: "File-only extra".to_string(),
                tool_name: None,
                tool_input: None,
            },
        ];

        let db_entries = vec![TranscriptEntry {
            timestamp: ts,
            role: Role::User,
            entry_type: EntryType::Text,
            content: "Same content".to_string(),
            tool_name: None,
            tool_input: None,
        }];

        let merged = merge_transcript_entries(file_entries, db_entries);
        assert_eq!(merged.len(), 2);
        // "Same content" appears once (from DB), "File-only extra" also present
        let contents: Vec<&str> = merged.iter().map(|e| e.content.as_str()).collect();
        assert!(contents.contains(&"Same content"));
        assert!(contents.contains(&"File-only extra"));
    }
}

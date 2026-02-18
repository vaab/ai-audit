pub mod cache;
pub mod db;
pub mod permissions;
pub mod run;
pub mod transcript;

use anyhow::{Context, Result};
use chrono::{DateTime, TimeZone, Utc};
use serde::Deserialize;
use std::fs;
use std::path::PathBuf;

pub fn storage_dir() -> PathBuf {
    crate::opencode_data_dir().join("storage/directory-agents")
}

pub fn part_dir() -> PathBuf {
    crate::opencode_data_dir().join("storage/part")
}

pub fn log_dir() -> PathBuf {
    crate::opencode_data_dir().join("log")
}

pub fn get_session_info(session_id: &str) -> Result<SessionInfo> {
    // Try DB first (richer data), fall back to file-based
    if let Ok(info) = db::get_session_info_from_db(session_id) {
        return Ok(info);
    }
    get_session_info_from_file(session_id)
}

/// File-based session info (original logic).
fn get_session_info_from_file(session_id: &str) -> Result<SessionInfo> {
    let storage_dir = storage_dir();
    let session_file = storage_dir.join(format!("{}.json", session_id));

    let content = fs::read_to_string(&session_file)
        .with_context(|| format!("Session file not found: {}", session_file.display()))?;

    let session_data: SessionFile =
        serde_json::from_str(&content).context("Failed to parse session file")?;

    let timestamp = Utc
        .timestamp_millis_opt(session_data.updated_at)
        .single()
        .unwrap_or_else(Utc::now);

    Ok(SessionInfo {
        session_id: session_data.session_id,
        started_at: timestamp,
        updated_at: timestamp,
        project_dir: session_data.directory,
        title: String::new(),
        parent_id: None,
    })
}

#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub session_id: String,
    /// Timestamp of session creation
    pub started_at: DateTime<Utc>,
    /// Timestamp of last update
    pub updated_at: DateTime<Utc>,
    /// Project directory path
    pub project_dir: String,
    /// Session title (from session metadata)
    pub title: String,
    /// Parent session ID (present for sub-agent sessions)
    pub parent_id: Option<String>,
}

/// Session file from `storage/directory-agents/` (minimal metadata).
#[derive(Deserialize)]
struct SessionFile {
    #[serde(rename = "sessionID")]
    session_id: String,
    #[serde(rename = "updatedAt")]
    updated_at: i64,
    #[serde(default)]
    directory: String,
}

/// Full session file from `storage/session/<project_hash>/` (has directory, title, etc.).
#[derive(Deserialize)]
struct FullSessionFile {
    id: String,
    #[serde(default)]
    directory: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    time: SessionTime,
    /// Present in sub-agent sessions; points to the parent session.
    #[serde(rename = "parentID")]
    parent_id: Option<String>,
}

#[derive(Deserialize, Default)]
struct SessionTime {
    #[serde(default)]
    created: i64,
    #[serde(default)]
    updated: i64,
}

/// Check if a session's messages contain the given text.
///
/// Checks both file-based storage and SQLite database. Returns true if
/// either source finds a match.
pub fn session_contains_text(session_id: &str, needle: &str) -> bool {
    // Check file-based first
    let storage_dir = storage_dir();
    let part_dir = storage_dir.parent().unwrap_or(&storage_dir).join("part");
    let message_dir = storage_dir
        .parent()
        .unwrap_or(&storage_dir)
        .join("message")
        .join(session_id);

    if session_contains_text_in_dirs(&message_dir, &part_dir, needle) {
        return true;
    }

    // Fall back to DB
    db::session_contains_text_from_db(session_id, needle)
}

/// OpenCode tool names that write or edit files.
const WRITE_TOOL_NAMES: &[&str] = &["write", "edit", "multi_edit", "create"];

/// Check if a session contains any write/edit tool_use targeting the given file path.
///
/// Checks both file-based storage and SQLite database. Returns true if
/// either source finds a match.
pub fn session_edited_file(session_id: &str, target_path: &str) -> bool {
    // Check file-based first
    let storage_dir = storage_dir();
    let part_dir = storage_dir.parent().unwrap_or(&storage_dir).join("part");
    let message_dir = storage_dir
        .parent()
        .unwrap_or(&storage_dir)
        .join("message")
        .join(session_id);

    if session_edited_file_in_dirs(&message_dir, &part_dir, target_path) {
        return true;
    }

    // Fall back to DB
    db::session_edited_file_from_db(session_id, target_path)
}

/// Check if a part JSON represents a write/edit tool targeting the given file path.
fn part_edits_file(part: &serde_json::Value, target_path: &str) -> bool {
    let part_type = part.get("type").and_then(|t| t.as_str()).unwrap_or("");
    if part_type != "tool" {
        return false;
    }
    let tool_name = match part.get("tool").and_then(|t| t.as_str()) {
        Some(n) => n,
        None => return false,
    };
    if !WRITE_TOOL_NAMES.contains(&tool_name) {
        return false;
    }
    let input = match part.get("state").and_then(|s| s.get("input")) {
        Some(i) => i,
        None => return false,
    };
    // OpenCode uses camelCase primarily, but also check snake_case
    let tool_path = input
        .get("filePath")
        .or_else(|| input.get("file_path"))
        .and_then(|p| p.as_str());
    match tool_path {
        Some(p) => crate::file_path_matches(p, target_path),
        None => false,
    }
}

/// Internal: check message/part dirs for file edit match.
fn session_edited_file_in_dirs(
    message_dir: &std::path::Path,
    part_dir: &std::path::Path,
    target_path: &str,
) -> bool {
    if !message_dir.exists() {
        return false;
    }

    // Fast pre-filter: extract filename component for raw text check
    let filename = std::path::Path::new(target_path)
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or(target_path);

    let msg_files: Vec<_> = match fs::read_dir(message_dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "json"))
            .collect(),
        Err(_) => return false,
    };

    for msg_entry in &msg_files {
        let msg_path = msg_entry.path();
        let msg_id = match msg_path.file_stem() {
            Some(s) => s.to_string_lossy().to_string(),
            None => continue,
        };

        let msg_part_dir = part_dir.join(&msg_id);
        if !msg_part_dir.exists() {
            continue;
        }

        let part_entries = match fs::read_dir(&msg_part_dir) {
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

            let raw = match fs::read_to_string(&part_path) {
                Ok(c) => c,
                Err(_) => continue,
            };

            // Fast pre-filter: skip files where the filename can't appear
            if !raw.contains(filename) {
                continue;
            }

            let part: serde_json::Value = match serde_json::from_str(&raw) {
                Ok(v) => v,
                Err(_) => continue,
            };

            if part_edits_file(&part, target_path) {
                return true;
            }
        }
    }

    false
}

/// Check if the last `last_n` messages of a session contain the given text.
///
/// Checks both file-based storage and SQLite database. Returns true if
/// either source finds a match.
pub fn session_tail_contains_text(session_id: &str, needle: &str, last_n: usize) -> bool {
    // Check file-based first
    let storage_dir = storage_dir();
    let part_dir = storage_dir.parent().unwrap_or(&storage_dir).join("part");
    let message_dir = storage_dir
        .parent()
        .unwrap_or(&storage_dir)
        .join("message")
        .join(session_id);

    if session_contains_text_in_dirs_tail(&message_dir, &part_dir, needle, Some(last_n)) {
        return true;
    }

    // Fall back to DB
    db::session_tail_contains_text_from_db(session_id, needle, last_n)
}

/// Check if an OpenCode part JSON contains the needle in searchable fields.
///
/// Searches:
/// - `text` parts: the `text` field
/// - `tool` parts: the tool name and `state.input` (serialized)
/// - `tool` parts with output: `state.output` (serialized)
pub(crate) fn part_contains_needle(part: &serde_json::Value, needle: &str) -> bool {
    let part_type = part.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match part_type {
        "text" => part
            .get("text")
            .and_then(|t| t.as_str())
            .is_some_and(|t| t.contains(needle)),
        "tool" => {
            // Check tool name
            if part
                .get("tool")
                .and_then(|t| t.as_str())
                .is_some_and(|t| t.contains(needle))
            {
                return true;
            }
            // Check state.input (serialized)
            if let Some(input) = part.get("state").and_then(|s| s.get("input")) {
                if input.to_string().contains(needle) {
                    return true;
                }
            }
            // Check state.output (serialized)
            if let Some(output) = part.get("state").and_then(|s| s.get("output")) {
                if output.to_string().contains(needle) {
                    return true;
                }
            }
            false
        }
        _ => false,
    }
}

/// Internal: check message/part dirs for text match.
///
/// Only reads part file contents (not message JSON — the message ID comes
/// from the filename). Uses a raw text pre-filter to skip JSON parsing on
/// part files that cannot possibly match.
///
/// If `last_n` is `Some(n)`, only the last `n` messages (sorted by filename)
/// are searched. If `None`, all messages are searched.
fn session_contains_text_in_dirs(
    message_dir: &std::path::Path,
    part_dir: &std::path::Path,
    needle: &str,
) -> bool {
    session_contains_text_in_dirs_tail(message_dir, part_dir, needle, None)
}

/// Like `session_contains_text_in_dirs`, but optionally limited to the
/// last `last_n` messages only.
fn session_contains_text_in_dirs_tail(
    message_dir: &std::path::Path,
    part_dir: &std::path::Path,
    needle: &str,
    last_n: Option<usize>,
) -> bool {
    if !message_dir.exists() {
        return false;
    }

    let mut msg_files: Vec<_> = match fs::read_dir(message_dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "json"))
            .collect(),
        Err(_) => return false,
    };

    // Sort by filename (chronological order)
    msg_files.sort_by_key(|e| e.file_name());

    // Take only the last N messages if requested
    let msg_iter: Box<dyn Iterator<Item = &fs::DirEntry>> = match last_n {
        Some(n) => Box::new(msg_files.iter().rev().take(n)),
        None => Box::new(msg_files.iter()),
    };

    for msg_entry in msg_iter {
        let msg_path = msg_entry.path();

        // Message ID is the filename stem — no need to read the JSON.
        let msg_id = match msg_path.file_stem() {
            Some(s) => s.to_string_lossy().to_string(),
            None => continue,
        };

        // Check parts for this message
        let msg_part_dir = part_dir.join(&msg_id);
        if !msg_part_dir.exists() {
            continue;
        }

        let part_entries = match fs::read_dir(&msg_part_dir) {
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

            let raw = match fs::read_to_string(&part_path) {
                Ok(c) => c,
                Err(_) => continue,
            };

            // Fast pre-filter: skip files where needle can't appear.
            if !raw.contains(needle) {
                continue;
            }

            // Confirm match is in a searchable field of the part.
            let part: serde_json::Value = match serde_json::from_str(&raw) {
                Ok(v) => v,
                Err(_) => continue,
            };

            if part_contains_needle(&part, needle) {
                return true;
            }
        }
    }

    false
}

pub fn list_sessions() -> Result<Vec<SessionInfo>> {
    let file_sessions = list_sessions_from_files()?;
    let db_sessions = db::list_sessions_from_db().unwrap_or_default();
    Ok(merge_sessions(file_sessions, db_sessions))
}

/// File-based session listing (original logic).
fn list_sessions_from_files() -> Result<Vec<SessionInfo>> {
    let session_base = crate::opencode_data_dir().join("storage/session");
    if !session_base.exists() {
        return Ok(Vec::new());
    }

    let mut sessions = Vec::new();

    // Scan storage/session/<project_hash>/ses_*.json
    for project_entry in fs::read_dir(&session_base)? {
        let project_entry = project_entry?;
        if !project_entry.path().is_dir() {
            continue;
        }
        for file_entry in fs::read_dir(project_entry.path())? {
            let file_entry = file_entry?;
            let path = file_entry.path();
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }
            if let Ok(content) = fs::read_to_string(&path) {
                if let Ok(session) = serde_json::from_str::<FullSessionFile>(&content) {
                    let started_at = Utc
                        .timestamp_millis_opt(session.time.created)
                        .single()
                        .unwrap_or_else(Utc::now);
                    let updated_at = if session.time.updated > 0 {
                        Utc.timestamp_millis_opt(session.time.updated)
                            .single()
                            .unwrap_or(started_at)
                    } else {
                        started_at
                    };
                    sessions.push(SessionInfo {
                        session_id: session.id,
                        started_at,
                        updated_at,
                        project_dir: session.directory,
                        title: session.title,
                        parent_id: session.parent_id,
                    });
                }
            }
        }
    }

    sessions.sort_by_key(|s| s.started_at);
    Ok(sessions)
}

/// Merge file-based and DB-based sessions, deduplicating by session_id.
/// DB wins on conflict (it's the newer/canonical source).
fn merge_sessions(
    file_sessions: Vec<SessionInfo>,
    db_sessions: Vec<SessionInfo>,
) -> Vec<SessionInfo> {
    use std::collections::HashMap;

    let mut by_id: HashMap<String, SessionInfo> = HashMap::new();

    // Insert file-based first
    for s in file_sessions {
        by_id.insert(s.session_id.clone(), s);
    }
    // DB overwrites on conflict
    for s in db_sessions {
        by_id.insert(s.session_id.clone(), s);
    }

    let mut merged: Vec<SessionInfo> = by_id.into_values().collect();
    merged.sort_by_key(|s| s.started_at);
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper to create OpenCode message+part structure for a session.
    fn create_session_with_messages(
        base_dir: &std::path::Path,
        session_id: &str,
        messages: &[(&str, &[(&str, &str)])], // (msg_id, parts: [(part_id, text)])
    ) {
        let message_dir = base_dir.join("message").join(session_id);
        let part_dir = base_dir.join("part");
        fs::create_dir_all(&message_dir).unwrap();

        for (msg_id, parts) in messages {
            // Create message file
            let msg_json = format!(
                r#"{{"id":"{}","sessionID":"{}","role":"user","time":{{"created":1700000000000}}}}"#,
                msg_id, session_id
            );
            fs::write(message_dir.join(format!("{}.json", msg_id)), msg_json).unwrap();

            // Create parts
            let msg_part_dir = part_dir.join(msg_id);
            fs::create_dir_all(&msg_part_dir).unwrap();

            for (part_id, text) in *parts {
                let part_json = format!(
                    r#"{{"id":"{}","sessionID":"{}","messageID":"{}","type":"text","text":"{}"}}"#,
                    part_id, session_id, msg_id, text
                );
                fs::write(msg_part_dir.join(format!("{}.json", part_id)), part_json).unwrap();
            }
        }
    }

    #[test]
    fn test_session_contains_text_match() {
        let temp = tempfile::tempdir().unwrap();
        create_session_with_messages(
            temp.path(),
            "ses_abc",
            &[("msg_1", &[("prt_1", "Hello world")])],
        );

        let message_dir = temp.path().join("message/ses_abc");
        let part_dir = temp.path().join("part");

        assert!(session_contains_text_in_dirs(
            &message_dir,
            &part_dir,
            "Hello"
        ));
        assert!(session_contains_text_in_dirs(
            &message_dir,
            &part_dir,
            "world"
        ));
        assert!(!session_contains_text_in_dirs(
            &message_dir,
            &part_dir,
            "WORLD"
        ));
        assert!(!session_contains_text_in_dirs(
            &message_dir,
            &part_dir,
            "goodbye"
        ));
    }

    #[test]
    fn test_session_contains_text_multiple_messages() {
        let temp = tempfile::tempdir().unwrap();
        create_session_with_messages(
            temp.path(),
            "ses_abc",
            &[
                ("msg_1", &[("prt_1", "first message")]),
                ("msg_2", &[("prt_2", "second with target")]),
            ],
        );

        let message_dir = temp.path().join("message/ses_abc");
        let part_dir = temp.path().join("part");

        assert!(session_contains_text_in_dirs(
            &message_dir,
            &part_dir,
            "target"
        ));
        assert!(session_contains_text_in_dirs(
            &message_dir,
            &part_dir,
            "first"
        ));
    }

    #[test]
    fn test_session_contains_text_no_match() {
        let temp = tempfile::tempdir().unwrap();
        create_session_with_messages(
            temp.path(),
            "ses_abc",
            &[("msg_1", &[("prt_1", "some content")])],
        );

        let message_dir = temp.path().join("message/ses_abc");
        let part_dir = temp.path().join("part");

        assert!(!session_contains_text_in_dirs(
            &message_dir,
            &part_dir,
            "absent"
        ));
    }

    #[test]
    fn test_session_contains_text_empty_session() {
        let temp = tempfile::tempdir().unwrap();
        let message_dir = temp.path().join("message/ses_abc");
        let part_dir = temp.path().join("part");
        fs::create_dir_all(&message_dir).unwrap();

        assert!(!session_contains_text_in_dirs(
            &message_dir,
            &part_dir,
            "anything"
        ));
    }

    #[test]
    fn test_session_contains_text_nonexistent_dir() {
        let temp = tempfile::tempdir().unwrap();
        let message_dir = temp.path().join("message/ses_missing");
        let part_dir = temp.path().join("part");

        assert!(!session_contains_text_in_dirs(
            &message_dir,
            &part_dir,
            "anything"
        ));
    }

    /// Helper to create a raw part file with arbitrary JSON content.
    fn create_raw_part(
        base_dir: &std::path::Path,
        session_id: &str,
        msg_id: &str,
        part_id: &str,
        part_json: &str,
    ) {
        let message_dir = base_dir.join("message").join(session_id);
        let part_dir = base_dir.join("part").join(msg_id);
        fs::create_dir_all(&message_dir).unwrap();
        fs::create_dir_all(&part_dir).unwrap();

        // Create message file if it doesn't exist
        let msg_file = message_dir.join(format!("{}.json", msg_id));
        if !msg_file.exists() {
            fs::write(
                &msg_file,
                format!(
                    r#"{{"id":"{}","sessionID":"{}","role":"assistant","time":{{"created":1700000000000}}}}"#,
                    msg_id, session_id
                ),
            )
            .unwrap();
        }

        fs::write(part_dir.join(format!("{}.json", part_id)), part_json).unwrap();
    }

    #[test]
    fn test_session_tail_contains_text_matches_recent_only() {
        let temp = tempfile::tempdir().unwrap();
        // Create 5 messages, put the target in msg_3 (not in last 2)
        create_session_with_messages(
            temp.path(),
            "ses_tail",
            &[
                ("msg_1", &[("prt_1", "old message")]),
                ("msg_2", &[("prt_2", "another old")]),
                ("msg_3", &[("prt_3", "unique target phrase")]),
                ("msg_4", &[("prt_4", "recent message")]),
                ("msg_5", &[("prt_5", "latest message")]),
            ],
        );

        let message_dir = temp.path().join("message/ses_tail");
        let part_dir = temp.path().join("part");

        // Searching last 2 should NOT find "unique target phrase" (it's in msg_3)
        assert!(!session_contains_text_in_dirs_tail(
            &message_dir,
            &part_dir,
            "unique target phrase",
            Some(2)
        ));

        // Searching last 3 SHOULD find it (msg_3 is 3rd from end)
        assert!(session_contains_text_in_dirs_tail(
            &message_dir,
            &part_dir,
            "unique target phrase",
            Some(3)
        ));

        // Searching all messages should also find it
        assert!(session_contains_text_in_dirs_tail(
            &message_dir,
            &part_dir,
            "unique target phrase",
            None
        ));

        // Searching last 2 should find "latest message"
        assert!(session_contains_text_in_dirs_tail(
            &message_dir,
            &part_dir,
            "latest message",
            Some(2)
        ));
    }

    #[test]
    fn test_session_tail_contains_text_no_match() {
        let temp = tempfile::tempdir().unwrap();
        create_session_with_messages(
            temp.path(),
            "ses_tail2",
            &[
                ("msg_1", &[("prt_1", "first")]),
                ("msg_2", &[("prt_2", "second")]),
            ],
        );

        let message_dir = temp.path().join("message/ses_tail2");
        let part_dir = temp.path().join("part");

        assert!(!session_contains_text_in_dirs_tail(
            &message_dir,
            &part_dir,
            "absent",
            Some(5)
        ));
    }

    #[test]
    fn test_session_tail_contains_text_empty_session() {
        let temp = tempfile::tempdir().unwrap();
        let message_dir = temp.path().join("message/ses_empty");
        let part_dir = temp.path().join("part");
        fs::create_dir_all(&message_dir).unwrap();

        assert!(!session_contains_text_in_dirs_tail(
            &message_dir,
            &part_dir,
            "anything",
            Some(3)
        ));
    }

    #[test]
    fn test_session_search_finds_tool_name() {
        let temp = tempfile::tempdir().unwrap();
        create_raw_part(
            temp.path(),
            "ses_abc",
            "msg_1",
            "prt_1",
            r#"{"id":"prt_1","type":"tool","tool":"bash","state":{"status":"completed","input":{"command":"ls -la"}}}"#,
        );

        let message_dir = temp.path().join("message/ses_abc");
        let part_dir = temp.path().join("part");

        assert!(session_contains_text_in_dirs(
            &message_dir,
            &part_dir,
            "bash"
        ));
        assert!(!session_contains_text_in_dirs(
            &message_dir,
            &part_dir,
            "grep"
        ));
    }

    #[test]
    fn test_session_search_finds_tool_input() {
        let temp = tempfile::tempdir().unwrap();
        create_raw_part(
            temp.path(),
            "ses_abc",
            "msg_1",
            "prt_1",
            r#"{"id":"prt_1","type":"tool","tool":"bash","state":{"status":"completed","input":{"command":"cargo test --release"}}}"#,
        );

        let message_dir = temp.path().join("message/ses_abc");
        let part_dir = temp.path().join("part");

        assert!(session_contains_text_in_dirs(
            &message_dir,
            &part_dir,
            "cargo test"
        ));
        assert!(session_contains_text_in_dirs(
            &message_dir,
            &part_dir,
            "--release"
        ));
        assert!(!session_contains_text_in_dirs(
            &message_dir,
            &part_dir,
            "npm install"
        ));
    }

    #[test]
    fn test_session_search_finds_tool_output() {
        let temp = tempfile::tempdir().unwrap();
        create_raw_part(
            temp.path(),
            "ses_abc",
            "msg_1",
            "prt_1",
            r#"{"id":"prt_1","type":"tool","tool":"bash","state":{"status":"completed","input":{"command":"echo hello"},"output":"hello world output"}}"#,
        );

        let message_dir = temp.path().join("message/ses_abc");
        let part_dir = temp.path().join("part");

        assert!(session_contains_text_in_dirs(
            &message_dir,
            &part_dir,
            "hello world output"
        ));
    }

    // --- Merge sessions tests ---

    #[test]
    fn test_merge_sessions_dedup_db_wins() {
        let ts1 = Utc.timestamp_millis_opt(1705314600000).unwrap();
        let ts2 = Utc.timestamp_millis_opt(1705314700000).unwrap();

        let file_sessions = vec![SessionInfo {
            session_id: "ses_001".to_string(),
            started_at: ts1,
            updated_at: ts1,
            project_dir: "/old/path".to_string(),
            title: "Old title".to_string(),
            parent_id: None,
        }];

        let db_sessions = vec![SessionInfo {
            session_id: "ses_001".to_string(),
            started_at: ts1,
            updated_at: ts2,
            project_dir: "/new/path".to_string(),
            title: "New title".to_string(),
            parent_id: None,
        }];

        let merged = merge_sessions(file_sessions, db_sessions);
        assert_eq!(merged.len(), 1);
        // DB version wins
        assert_eq!(merged[0].title, "New title");
        assert_eq!(merged[0].project_dir, "/new/path");
    }

    #[test]
    fn test_merge_sessions_combines_unique() {
        let ts1 = Utc.timestamp_millis_opt(1705314600000).unwrap();
        let ts2 = Utc.timestamp_millis_opt(1705314700000).unwrap();

        let file_sessions = vec![SessionInfo {
            session_id: "ses_file_only".to_string(),
            started_at: ts1,
            updated_at: ts1,
            project_dir: "/proj".to_string(),
            title: "File session".to_string(),
            parent_id: None,
        }];

        let db_sessions = vec![SessionInfo {
            session_id: "ses_db_only".to_string(),
            started_at: ts2,
            updated_at: ts2,
            project_dir: "/proj".to_string(),
            title: "DB session".to_string(),
            parent_id: None,
        }];

        let merged = merge_sessions(file_sessions, db_sessions);
        assert_eq!(merged.len(), 2);
        // Sorted by started_at
        assert_eq!(merged[0].session_id, "ses_file_only");
        assert_eq!(merged[1].session_id, "ses_db_only");
    }

    #[test]
    fn test_merge_sessions_empty_sources() {
        let merged = merge_sessions(Vec::new(), Vec::new());
        assert!(merged.is_empty());

        let ts = Utc.timestamp_millis_opt(1705314600000).unwrap();
        let sessions = vec![SessionInfo {
            session_id: "ses_001".to_string(),
            started_at: ts,
            updated_at: ts,
            project_dir: "/proj".to_string(),
            title: "Only".to_string(),
            parent_id: None,
        }];

        let merged = merge_sessions(sessions.clone(), Vec::new());
        assert_eq!(merged.len(), 1);

        let merged = merge_sessions(Vec::new(), sessions);
        assert_eq!(merged.len(), 1);
    }

    // === session_edited_file tests ===

    #[test]
    fn test_session_edited_file_write_tool_absolute() {
        let temp = tempfile::tempdir().unwrap();
        create_raw_part(
            temp.path(),
            "ses_abc",
            "msg_1",
            "prt_1",
            r#"{"id":"prt_1","type":"tool","tool":"write","state":{"status":"completed","input":{"filePath":"/home/user/src/main.rs","content":"fn main() {}"}}}"#,
        );

        let message_dir = temp.path().join("message/ses_abc");
        let part_dir = temp.path().join("part");

        assert!(session_edited_file_in_dirs(
            &message_dir,
            &part_dir,
            "/home/user/src/main.rs"
        ));
        assert!(!session_edited_file_in_dirs(
            &message_dir,
            &part_dir,
            "/home/user/src/lib.rs"
        ));
    }

    #[test]
    fn test_session_edited_file_edit_tool() {
        let temp = tempfile::tempdir().unwrap();
        create_raw_part(
            temp.path(),
            "ses_abc",
            "msg_1",
            "prt_1",
            r#"{"id":"prt_1","type":"tool","tool":"edit","state":{"status":"completed","input":{"filePath":"/home/user/src/lib.rs","oldString":"old","newString":"new"}}}"#,
        );

        let message_dir = temp.path().join("message/ses_abc");
        let part_dir = temp.path().join("part");

        assert!(session_edited_file_in_dirs(
            &message_dir,
            &part_dir,
            "/home/user/src/lib.rs"
        ));
    }

    #[test]
    fn test_session_edited_file_multi_edit_tool() {
        let temp = tempfile::tempdir().unwrap();
        create_raw_part(
            temp.path(),
            "ses_abc",
            "msg_1",
            "prt_1",
            r#"{"id":"prt_1","type":"tool","tool":"multi_edit","state":{"status":"completed","input":{"filePath":"/home/user/src/mod.rs","edits":[]}}}"#,
        );

        let message_dir = temp.path().join("message/ses_abc");
        let part_dir = temp.path().join("part");

        assert!(session_edited_file_in_dirs(
            &message_dir,
            &part_dir,
            "/home/user/src/mod.rs"
        ));
    }

    #[test]
    fn test_session_edited_file_create_tool() {
        let temp = tempfile::tempdir().unwrap();
        create_raw_part(
            temp.path(),
            "ses_abc",
            "msg_1",
            "prt_1",
            r#"{"id":"prt_1","type":"tool","tool":"create","state":{"status":"completed","input":{"filePath":"/home/user/new.rs","content":"// new"}}}"#,
        );

        let message_dir = temp.path().join("message/ses_abc");
        let part_dir = temp.path().join("part");

        assert!(session_edited_file_in_dirs(
            &message_dir,
            &part_dir,
            "/home/user/new.rs"
        ));
    }

    #[test]
    fn test_session_edited_file_snake_case_field() {
        let temp = tempfile::tempdir().unwrap();
        create_raw_part(
            temp.path(),
            "ses_abc",
            "msg_1",
            "prt_1",
            r#"{"id":"prt_1","type":"tool","tool":"write","state":{"status":"completed","input":{"file_path":"/home/user/src/main.rs","content":"fn main() {}"}}}"#,
        );

        let message_dir = temp.path().join("message/ses_abc");
        let part_dir = temp.path().join("part");

        assert!(session_edited_file_in_dirs(
            &message_dir,
            &part_dir,
            "/home/user/src/main.rs"
        ));
    }

    #[test]
    fn test_session_edited_file_relative_path_in_tool() {
        let temp = tempfile::tempdir().unwrap();
        create_raw_part(
            temp.path(),
            "ses_abc",
            "msg_1",
            "prt_1",
            r#"{"id":"prt_1","type":"tool","tool":"write","state":{"status":"completed","input":{"filePath":"src/main.rs","content":"fn main() {}"}}}"#,
        );

        let message_dir = temp.path().join("message/ses_abc");
        let part_dir = temp.path().join("part");

        assert!(session_edited_file_in_dirs(
            &message_dir,
            &part_dir,
            "/home/user/project/src/main.rs"
        ));
        assert!(!session_edited_file_in_dirs(
            &message_dir,
            &part_dir,
            "/home/user/project/src/lib.rs"
        ));
    }

    #[test]
    fn test_session_edited_file_ignores_non_write_tools() {
        let temp = tempfile::tempdir().unwrap();
        create_raw_part(
            temp.path(),
            "ses_abc",
            "msg_1",
            "prt_1",
            r#"{"id":"prt_1","type":"tool","tool":"read","state":{"status":"completed","input":{"filePath":"/home/user/src/main.rs"}}}"#,
        );

        let message_dir = temp.path().join("message/ses_abc");
        let part_dir = temp.path().join("part");

        assert!(!session_edited_file_in_dirs(
            &message_dir,
            &part_dir,
            "/home/user/src/main.rs"
        ));
    }

    #[test]
    fn test_session_edited_file_ignores_text_parts() {
        let temp = tempfile::tempdir().unwrap();
        create_session_with_messages(
            temp.path(),
            "ses_abc",
            &[("msg_1", &[("prt_1", "/home/user/src/main.rs")])],
        );

        let message_dir = temp.path().join("message/ses_abc");
        let part_dir = temp.path().join("part");

        assert!(!session_edited_file_in_dirs(
            &message_dir,
            &part_dir,
            "/home/user/src/main.rs"
        ));
    }

    #[test]
    fn test_session_edited_file_empty_session() {
        let temp = tempfile::tempdir().unwrap();
        let message_dir = temp.path().join("message/ses_empty");
        let part_dir = temp.path().join("part");
        fs::create_dir_all(&message_dir).unwrap();

        assert!(!session_edited_file_in_dirs(
            &message_dir,
            &part_dir,
            "/home/user/src/main.rs"
        ));
    }

    #[test]
    fn test_session_edited_file_nonexistent_dir() {
        let temp = tempfile::tempdir().unwrap();
        let message_dir = temp.path().join("message/ses_missing");
        let part_dir = temp.path().join("part");

        assert!(!session_edited_file_in_dirs(
            &message_dir,
            &part_dir,
            "/home/user/src/main.rs"
        ));
    }
}

pub mod cache;
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
    time: SessionTime,
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
/// Walks message/<session_id>/ to find message IDs, then checks
/// part/<msg_id>/ for text parts containing the needle.
pub fn session_contains_text(session_id: &str, needle: &str) -> bool {
    let storage_dir = storage_dir();
    let part_dir = storage_dir.parent().unwrap_or(&storage_dir).join("part");
    let message_dir = storage_dir
        .parent()
        .unwrap_or(&storage_dir)
        .join("message")
        .join(session_id);

    session_contains_text_in_dirs(&message_dir, &part_dir, needle)
}

/// Check if an OpenCode part JSON contains the needle in searchable fields.
///
/// Searches:
/// - `text` parts: the `text` field
/// - `tool` parts: the tool name and `state.input` (serialized)
/// - `tool` parts with output: `state.output` (serialized)
fn part_contains_needle(part: &serde_json::Value, needle: &str) -> bool {
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
fn session_contains_text_in_dirs(
    message_dir: &std::path::Path,
    part_dir: &std::path::Path,
    needle: &str,
) -> bool {
    if !message_dir.exists() {
        return false;
    }

    let msg_entries = match fs::read_dir(message_dir) {
        Ok(entries) => entries,
        Err(_) => return false,
    };

    for msg_entry in msg_entries {
        let msg_entry = match msg_entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let msg_path = msg_entry.path();
        if msg_path.extension().is_none_or(|e| e != "json") {
            continue;
        }

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
                    });
                }
            }
        }
    }

    sessions.sort_by_key(|s| s.started_at);
    Ok(sessions)
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
}

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct ToolUse {
    pub timestamp: DateTime<Utc>,
    pub tool: String,
    pub input: HashMap<String, Value>,
}

#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub session_id: String,
    pub timestamp: DateTime<Utc>,
}

pub fn list_sessions() -> Result<Vec<SessionInfo>> {
    let projects_dir = crate::claudecode::projects_dir();
    if !projects_dir.exists() {
        return Ok(Vec::new());
    }

    let mut sessions = Vec::new();

    for project_entry in fs::read_dir(&projects_dir)? {
        let project_entry = project_entry?;
        let project_path = project_entry.path();
        if !project_path.is_dir() {
            continue;
        }

        for file_entry in fs::read_dir(&project_path)? {
            let file_entry = file_entry?;
            let file_path = file_entry.path();
            if file_path.extension().is_some_and(|e| e == "jsonl") {
                if let Some(stem) = file_path.file_stem() {
                    let session_id = stem.to_string_lossy().to_string();
                    if let Ok(ts) = get_session_first_timestamp(&file_path) {
                        sessions.push(SessionInfo {
                            session_id,
                            timestamp: ts,
                        });
                    }
                }
            }
        }
    }

    sessions.sort_by_key(|s| s.timestamp);
    Ok(sessions)
}

fn get_session_first_timestamp(path: &Path) -> Result<DateTime<Utc>> {
    let file = fs::File::open(path)?;
    let reader = std::io::BufReader::new(file);
    use std::io::BufRead;

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(entry) = serde_json::from_str::<serde_json::Value>(&line) {
            if let Some(ts_str) = entry.get("timestamp").and_then(|v| v.as_str()) {
                if let Ok(dt) = DateTime::parse_from_rfc3339(ts_str) {
                    return Ok(dt.with_timezone(&Utc));
                }
            }
        }
    }
    anyhow::bail!("No timestamp found in session file")
}

/// Check if a session's messages contain the given text (case-insensitive).
///
/// Scans user and assistant message content blocks in the JSONL file.
pub fn session_contains_text(session_id: &str, needle: &str) -> bool {
    let session_file = match find_session_file(session_id) {
        Some(f) => f,
        None => return false,
    };
    file_contains_text(&session_file, needle)
}

/// Check if a session JSONL file contains the given text in message content (case-insensitive).
///
/// Uses a two-pass strategy: first a raw string search to skip files that
/// cannot possibly match, then a proper JSON parse to confirm the match is
/// inside a message content field (not metadata).
fn file_contains_text(path: &Path, needle: &str) -> bool {
    use std::io::BufRead;

    // Fast path: read raw bytes and check if needle appears at all.
    // This skips JSON parsing for the vast majority of files.
    let raw = match fs::read_to_string(path) {
        Ok(r) => r,
        Err(_) => return false,
    };
    let needle_lower = needle.to_lowercase();
    if !raw.to_lowercase().contains(&needle_lower) {
        return false;
    }

    // Slow path: needle is somewhere in the file — confirm it's in message content.
    let reader = std::io::BufReader::new(raw.as_bytes());

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        if line.trim().is_empty() {
            continue;
        }

        // Quick per-line pre-filter: skip lines that can't contain the needle.
        if !line.to_lowercase().contains(&needle_lower) {
            continue;
        }

        let entry: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // Only check user and assistant messages
        let entry_type = entry.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if entry_type != "user" && entry_type != "assistant" {
            continue;
        }

        let content = match entry.get("message").and_then(|m| m.get("content")) {
            Some(c) => c,
            None => continue,
        };

        match content {
            serde_json::Value::String(s) => {
                if s.to_lowercase().contains(&needle_lower) {
                    return true;
                }
            }
            serde_json::Value::Array(arr) => {
                for block in arr {
                    if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                        if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                            if text.to_lowercase().contains(&needle_lower) {
                                return true;
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    false
}

pub fn find_session_file(session_uuid: &str) -> Option<PathBuf> {
    let projects_dir = crate::claudecode::projects_dir();
    if !projects_dir.exists() {
        return None;
    }

    // Search all project directories for the session file
    let filename = format!("{}.jsonl", session_uuid);

    for entry in fs::read_dir(&projects_dir).ok()? {
        let entry = entry.ok()?;
        let path = entry.path();
        if path.is_dir() {
            let session_file = path.join(&filename);
            if session_file.exists() {
                return Some(session_file);
            }
        }
    }
    None
}

#[derive(Deserialize)]
struct SessionEntry {
    timestamp: Option<String>,
    message: Option<MessageContent>,
}

#[derive(Deserialize)]
struct MessageContent {
    content: Option<Vec<ContentBlock>>,
}

#[derive(Deserialize)]
struct ContentBlock {
    #[serde(rename = "type")]
    block_type: Option<String>,
    name: Option<String>,
    input: Option<HashMap<String, Value>>,
}

/// Parse tool_use entries from a session JSONL file
pub fn parse_tool_uses(content: &str) -> Result<Vec<ToolUse>> {
    let mut tool_uses = Vec::new();

    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }

        let entry: SessionEntry = match serde_json::from_str(line) {
            Ok(e) => e,
            Err(_) => continue,
        };

        let timestamp_str = match entry.timestamp {
            Some(ts) => ts,
            None => continue,
        };

        let timestamp = match DateTime::parse_from_rfc3339(&timestamp_str) {
            Ok(dt) => dt.with_timezone(&Utc),
            Err(_) => continue,
        };

        let message = match entry.message {
            Some(m) => m,
            None => continue,
        };

        let content_blocks = match message.content {
            Some(c) => c,
            None => continue,
        };

        for block in content_blocks {
            if block.block_type.as_deref() == Some("tool_use") {
                if let (Some(name), Some(input)) = (block.name, block.input) {
                    tool_uses.push(ToolUse {
                        timestamp,
                        tool: name,
                        input,
                    });
                }
            }
        }
    }

    Ok(tool_uses)
}

/// Load and parse tool uses from a session file
pub fn load_tool_uses(session_uuid: &str) -> Result<Vec<ToolUse>> {
    let session_file = find_session_file(session_uuid).context("Session file not found")?;

    let content = fs::read_to_string(&session_file).context("Failed to read session file")?;

    parse_tool_uses(&content)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_file_contains_text_user_string_content() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"{{"type":"user","timestamp":"2024-01-15T10:30:00.000Z","message":{{"role":"user","content":"Hello world"}}}}"#
        )
        .unwrap();

        assert!(file_contains_text(file.path(), "hello"));
        assert!(file_contains_text(file.path(), "WORLD"));
        assert!(!file_contains_text(file.path(), "goodbye"));
    }

    #[test]
    fn test_file_contains_text_assistant_array_content() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"{{"type":"assistant","timestamp":"2024-01-15T10:30:00.000Z","message":{{"role":"assistant","content":[{{"type":"text","text":"some unique phrase"}}]}}}}"#
        )
        .unwrap();

        assert!(file_contains_text(file.path(), "unique phrase"));
        assert!(!file_contains_text(file.path(), "missing text"));
    }

    #[test]
    fn test_file_contains_text_skips_non_message_types() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"{{"type":"summary","summary":"needle in summary"}}"#
        )
        .unwrap();

        assert!(!file_contains_text(file.path(), "needle"));
    }

    #[test]
    fn test_file_contains_text_multiple_entries() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"{{"type":"user","timestamp":"2024-01-15T10:30:00.000Z","message":{{"role":"user","content":"first message"}}}}"#
        )
        .unwrap();
        writeln!(
            file,
            r#"{{"type":"assistant","timestamp":"2024-01-15T10:30:01.000Z","message":{{"role":"assistant","content":[{{"type":"text","text":"second message with target"}}]}}}}"#
        )
        .unwrap();

        assert!(file_contains_text(file.path(), "target"));
        assert!(file_contains_text(file.path(), "first"));
        assert!(!file_contains_text(file.path(), "absent"));
    }

    #[test]
    fn test_file_contains_text_empty_file() {
        let file = NamedTempFile::new().unwrap();
        assert!(!file_contains_text(file.path(), "anything"));
    }
}

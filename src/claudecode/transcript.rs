use anyhow::{Context, Result};
use chrono::DateTime;
use serde_json::Value;
use std::fs;
use std::io::{BufRead, BufReader};

use crate::transcript::{EntryType, Role, TranscriptEntry};

/// Parse a full conversation transcript from a Claude Code session JSONL file.
pub fn parse_transcript(session_id: &str) -> Result<Vec<TranscriptEntry>> {
    let session_file = super::session::find_session_file(session_id)
        .with_context(|| format!("Session file not found for: {}", session_id))?;

    parse_transcript_from_file(&session_file)
}

fn parse_transcript_from_file(path: &std::path::Path) -> Result<Vec<TranscriptEntry>> {
    let file = fs::File::open(path)
        .with_context(|| format!("Failed to open session file: {}", path.display()))?;
    let reader = BufReader::new(file);
    let mut entries = Vec::new();

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }

        let raw: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let entry_type = raw.get("type").and_then(|v| v.as_str()).unwrap_or("");

        // Skip non-conversation entries
        match entry_type {
            "user" | "assistant" => {}
            _ => continue,
        }

        let timestamp_str = match raw.get("timestamp").and_then(|v| v.as_str()) {
            Some(ts) => ts,
            None => continue,
        };

        let timestamp = match DateTime::parse_from_rfc3339(timestamp_str) {
            Ok(dt) => dt.to_utc(),
            Err(_) => continue,
        };

        let message = match raw.get("message") {
            Some(m) => m,
            None => continue,
        };

        let role_str = message.get("role").and_then(|v| v.as_str()).unwrap_or("");
        let role = match role_str {
            "user" => Role::User,
            "assistant" => Role::Assistant,
            _ => continue,
        };

        let content = match message.get("content") {
            Some(c) => c,
            None => continue,
        };

        match content {
            Value::String(s) => {
                // Simple string content → user text message
                if !s.trim().is_empty() {
                    entries.push(TranscriptEntry {
                        timestamp,
                        role,
                        entry_type: EntryType::Text,
                        content: s.clone(),
                        tool_name: None,
                        tool_input: None,
                    });
                }
            }
            Value::Array(blocks) => {
                for block in blocks {
                    let block_type = block.get("type").and_then(|v| v.as_str()).unwrap_or("");

                    match block_type {
                        "text" => {
                            if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                                if !text.trim().is_empty() {
                                    entries.push(TranscriptEntry {
                                        timestamp,
                                        role: role.clone(),
                                        entry_type: EntryType::Text,
                                        content: text.to_string(),
                                        tool_name: None,
                                        tool_input: None,
                                    });
                                }
                            }
                        }
                        "tool_use" => {
                            let name = block
                                .get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or("unknown")
                                .to_string();
                            let input = block.get("input").cloned();
                            entries.push(TranscriptEntry {
                                timestamp,
                                role: role.clone(),
                                entry_type: EntryType::ToolUse,
                                content: String::new(),
                                tool_name: Some(name),
                                tool_input: input,
                            });
                        }
                        "tool_result" => {
                            let result_content = extract_tool_result_content(block);
                            entries.push(TranscriptEntry {
                                timestamp,
                                role: role.clone(),
                                entry_type: EntryType::ToolResult,
                                content: result_content,
                                tool_name: None,
                                tool_input: None,
                            });
                        }
                        "thinking" => {
                            if let Some(text) = block.get("thinking").and_then(|v| v.as_str()) {
                                if !text.trim().is_empty() {
                                    entries.push(TranscriptEntry {
                                        timestamp,
                                        role: role.clone(),
                                        entry_type: EntryType::Thinking,
                                        content: text.to_string(),
                                        tool_name: None,
                                        tool_input: None,
                                    });
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    // Sort by timestamp for stable ordering
    entries.sort_by_key(|e| e.timestamp);
    Ok(entries)
}

/// Extract text content from a tool_result block.
///
/// The content can be a string or an array of text blocks.
fn extract_tool_result_content(block: &Value) -> String {
    match block.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|item| {
                if item.get("type").and_then(|t| t.as_str()) == Some("text") {
                    item.get("text").and_then(|t| t.as_str()).map(String::from)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn write_jsonl(lines: &[&str]) -> NamedTempFile {
        let mut file = NamedTempFile::new().unwrap();
        for line in lines {
            writeln!(file, "{}", line).unwrap();
        }
        file
    }

    #[test]
    fn test_parse_user_text_message() {
        let file = write_jsonl(&[
            r#"{"type":"user","timestamp":"2024-01-15T10:30:00.000Z","message":{"role":"user","content":"Hello AI"}}"#,
        ]);
        let entries = parse_transcript_from_file(file.path()).unwrap();
        assert_eq!(entries.len(), 1);
        assert!(matches!(entries[0].role, Role::User));
        assert!(matches!(entries[0].entry_type, EntryType::Text));
        assert_eq!(entries[0].content, "Hello AI");
    }

    #[test]
    fn test_parse_assistant_text() {
        let file = write_jsonl(&[
            r#"{"type":"assistant","timestamp":"2024-01-15T10:30:05.000Z","message":{"role":"assistant","content":[{"type":"text","text":"I can help with that."}]}}"#,
        ]);
        let entries = parse_transcript_from_file(file.path()).unwrap();
        assert_eq!(entries.len(), 1);
        assert!(matches!(entries[0].role, Role::Assistant));
        assert!(matches!(entries[0].entry_type, EntryType::Text));
        assert_eq!(entries[0].content, "I can help with that.");
    }

    #[test]
    fn test_parse_tool_use() {
        let file = write_jsonl(&[
            r#"{"type":"assistant","timestamp":"2024-01-15T10:30:10.000Z","message":{"role":"assistant","content":[{"type":"tool_use","name":"Bash","input":{"command":"ls -la"}}]}}"#,
        ]);
        let entries = parse_transcript_from_file(file.path()).unwrap();
        assert_eq!(entries.len(), 1);
        assert!(matches!(entries[0].entry_type, EntryType::ToolUse));
        assert_eq!(entries[0].tool_name.as_deref(), Some("Bash"));
        assert!(entries[0].tool_input.is_some());
    }

    #[test]
    fn test_parse_tool_result() {
        let file = write_jsonl(&[
            r#"{"type":"user","timestamp":"2024-01-15T10:30:15.000Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_123","content":"file1.txt\nfile2.txt"}]}}"#,
        ]);
        let entries = parse_transcript_from_file(file.path()).unwrap();
        assert_eq!(entries.len(), 1);
        assert!(matches!(entries[0].entry_type, EntryType::ToolResult));
        assert_eq!(entries[0].content, "file1.txt\nfile2.txt");
    }

    #[test]
    fn test_parse_tool_result_array_content() {
        let file = write_jsonl(&[
            r#"{"type":"user","timestamp":"2024-01-15T10:30:15.000Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_123","content":[{"type":"text","text":"result text"}]}]}}"#,
        ]);
        let entries = parse_transcript_from_file(file.path()).unwrap();
        assert_eq!(entries.len(), 1);
        assert!(matches!(entries[0].entry_type, EntryType::ToolResult));
        assert_eq!(entries[0].content, "result text");
    }

    #[test]
    fn test_parse_thinking() {
        let file = write_jsonl(&[
            r#"{"type":"assistant","timestamp":"2024-01-15T10:30:20.000Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"Let me analyze this..."}]}}"#,
        ]);
        let entries = parse_transcript_from_file(file.path()).unwrap();
        assert_eq!(entries.len(), 1);
        assert!(matches!(entries[0].entry_type, EntryType::Thinking));
        assert_eq!(entries[0].content, "Let me analyze this...");
    }

    #[test]
    fn test_skip_summary_and_snapshot() {
        let file = write_jsonl(&[
            r#"{"type":"summary","summary":"Session summary text"}"#,
            r#"{"type":"file-history-snapshot","messageId":"abc","snapshot":{}}"#,
            r#"{"type":"user","timestamp":"2024-01-15T10:30:00.000Z","message":{"role":"user","content":"Real message"}}"#,
        ]);
        let entries = parse_transcript_from_file(file.path()).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].content, "Real message");
    }

    #[test]
    fn test_ordering() {
        let file = write_jsonl(&[
            r#"{"type":"assistant","timestamp":"2024-01-15T10:30:05.000Z","message":{"role":"assistant","content":[{"type":"text","text":"Response"}]}}"#,
            r#"{"type":"user","timestamp":"2024-01-15T10:30:00.000Z","message":{"role":"user","content":"Question"}}"#,
        ]);
        let entries = parse_transcript_from_file(file.path()).unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries[0].timestamp < entries[1].timestamp);
        assert_eq!(entries[0].content, "Question");
        assert_eq!(entries[1].content, "Response");
    }
}

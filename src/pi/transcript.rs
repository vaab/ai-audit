//! Pi transcript parsing: JSONL → unified `TranscriptEntry`s.
//!
//! Block mapping (Pi → unified):
//! - `text` → `EntryType::Text`
//! - `thinking` → `EntryType::Thinking`
//! - `toolCall` → `EntryType::ToolUse` (carries `name` and `arguments`)
//! - role `toolResult` → `EntryType::ToolResult` (each text block becomes
//!   a single tool-result entry)

use anyhow::{Context, Result};
use chrono::DateTime;
use serde_json::Value;
use std::fs;
use std::io::{BufRead, BufReader};

use crate::transcript::{EntryType, Role, TranscriptEntry};

/// Parse a full conversation transcript from a Pi session JSONL file.
pub fn parse_transcript(session_id: &str) -> Result<Vec<TranscriptEntry>> {
    let path = super::session::find_session_file(session_id)
        .with_context(|| format!("Session file not found for: {}", session_id))?;
    parse_transcript_from_file(&path)
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

        // We only care about "message" entries for the transcript.
        if raw.get("type").and_then(|v| v.as_str()) != Some("message") {
            continue;
        }

        let timestamp = match raw
            .get("timestamp")
            .and_then(|v| v.as_str())
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        {
            Some(dt) => dt.to_utc(),
            None => continue,
        };

        let message = match raw.get("message") {
            Some(m) => m,
            None => continue,
        };

        let role_str = message.get("role").and_then(|v| v.as_str()).unwrap_or("");

        // Pi roles include `user`, `assistant`, and `toolResult`.
        // `toolResult` is mapped to a synthetic User-role tool-result
        // entry to mirror how Claude Code surfaces tool results inside
        // user-role content arrays.
        let role = match role_str {
            "user" => Role::User,
            "assistant" => Role::Assistant,
            "toolResult" => Role::User,
            _ => continue,
        };

        if role_str == "toolResult" {
            // Collect tool result text content.
            let tool_name = message
                .get("toolName")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let text = collect_tool_result_text(message.get("content").unwrap_or(&Value::Null));
            entries.push(TranscriptEntry {
                timestamp,
                role,
                entry_type: EntryType::ToolResult,
                content: text,
                tool_name,
                tool_input: None,
            });
            continue;
        }

        let content = match message.get("content") {
            Some(c) => c,
            None => continue,
        };

        match content {
            Value::String(s) => {
                if !s.trim().is_empty() {
                    entries.push(TranscriptEntry {
                        timestamp,
                        role: role.clone(),
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
                        "toolCall" => {
                            let name = block
                                .get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or("unknown")
                                .to_string();
                            let input = block.get("arguments").cloned();
                            entries.push(TranscriptEntry {
                                timestamp,
                                role: role.clone(),
                                entry_type: EntryType::ToolUse,
                                content: String::new(),
                                tool_name: Some(name),
                                tool_input: input,
                            });
                        }
                        _ => {} // image, file, etc. — ignored for now
                    }
                }
            }
            _ => {}
        }
    }
    Ok(entries)
}

fn collect_tool_result_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => {
            let mut parts = Vec::new();
            for block in blocks {
                if block.get("type").and_then(|v| v.as_str()) == Some("text") {
                    if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                        parts.push(text.to_string());
                    }
                }
            }
            parts.join("\n")
        }
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn write_jsonl(content: &str) -> NamedTempFile {
        let mut f = tempfile::Builder::new()
            .suffix(".jsonl")
            .tempfile()
            .unwrap();
        f.write_all(content.as_bytes()).unwrap();
        f
    }

    #[test]
    fn test_parse_transcript_text_thinking_toolcall() {
        let f = write_jsonl(indoc! {r#"
            {"type":"session","version":3,"id":"x","timestamp":"2026-04-30T09:36:43Z","cwd":"/tmp"}
            {"type":"message","id":"u","timestamp":"2026-04-30T09:37:00Z","message":{"role":"user","content":[{"type":"text","text":"please refactor"}]}}
            {"type":"message","id":"a","timestamp":"2026-04-30T09:37:01Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"I will read the file"},{"type":"text","text":"Reading now"},{"type":"toolCall","id":"tc1","name":"read","arguments":{"path":"/foo"}}]}}
            {"type":"message","id":"tr","timestamp":"2026-04-30T09:37:02Z","message":{"role":"toolResult","toolCallId":"tc1","toolName":"read","content":[{"type":"text","text":"file contents"}]}}
        "#});

        let entries = parse_transcript_from_file(f.path()).unwrap();
        assert_eq!(entries.len(), 5);

        // 1. user text
        assert!(matches!(entries[0].entry_type, EntryType::Text));
        assert!(matches!(entries[0].role, Role::User));
        assert_eq!(entries[0].content, "please refactor");

        // 2. assistant thinking
        assert!(matches!(entries[1].entry_type, EntryType::Thinking));
        assert!(matches!(entries[1].role, Role::Assistant));
        assert!(entries[1].content.contains("I will read"));

        // 3. assistant text
        assert!(matches!(entries[2].entry_type, EntryType::Text));
        assert!(matches!(entries[2].role, Role::Assistant));

        // 4. assistant toolCall
        assert!(matches!(entries[3].entry_type, EntryType::ToolUse));
        assert_eq!(entries[3].tool_name.as_deref(), Some("read"));
        assert!(entries[3].tool_input.is_some());

        // 5. tool result
        assert!(matches!(entries[4].entry_type, EntryType::ToolResult));
        assert!(matches!(entries[4].role, Role::User));
        assert_eq!(entries[4].tool_name.as_deref(), Some("read"));
        assert_eq!(entries[4].content, "file contents");
    }

    #[test]
    fn test_parse_transcript_skips_non_message() {
        let f = write_jsonl(indoc! {r#"
            {"type":"session","version":3,"id":"x","timestamp":"2026-04-30T09:36:43Z","cwd":"/tmp"}
            {"type":"model_change","id":"m","timestamp":"2026-04-30T09:36:44Z","provider":"x","modelId":"y"}
            {"type":"message","id":"u","timestamp":"2026-04-30T09:37:00Z","message":{"role":"user","content":"hi"}}
        "#});
        let entries = parse_transcript_from_file(f.path()).unwrap();
        assert_eq!(entries.len(), 1);
        assert!(matches!(entries[0].role, Role::User));
        assert_eq!(entries[0].content, "hi");
    }

    #[test]
    fn test_parse_transcript_string_content_user() {
        // Pi user messages can have content as a plain string (not an array).
        let f = write_jsonl(indoc! {r#"
            {"type":"session","version":3,"id":"x","timestamp":"2026-04-30T09:36:43Z","cwd":"/tmp"}
            {"type":"message","id":"u","timestamp":"2026-04-30T09:37:00Z","message":{"role":"user","content":"plain string"}}
        "#});
        let entries = parse_transcript_from_file(f.path()).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].content, "plain string");
    }
}

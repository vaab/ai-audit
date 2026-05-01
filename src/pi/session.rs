//! Pi session JSONL parsing.
//!
//! Pi session files have a header line of the form:
//! `{"type":"session","version":3,"id":"<uuidv7>","timestamp":"...","cwd":"..."}`
//! followed by one entry per line.  Entry shapes we care about:
//!
//! * `{"type":"message","id":"<short>","parentId":"<short|null>","timestamp":"...","message":{...}}`
//!   — the `message` object holds `role` (`user`, `assistant`, `toolResult`),
//!   `content` (array of `{type: text|thinking|toolCall|...}` blocks),
//!   `modelID` and `usage` for assistants.
//! * `{"type":"model_change", ...}` — informational, used for token attribution.
//!
//! Pi tool-call blocks use `{"type":"toolCall","name":"<tool>","arguments":{...}}`
//! and reference paths via the `path` argument key (not `file_path` /
//! `filePath` like Claude Code / OpenCode).

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use crate::provider::{Message, Provider, TokenUsage};

/// Pi session metadata derived from a JSONL file's header + last entry.
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub session_id: String,
    pub started_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Authoritative cwd from the JSONL header, NOT decoded from the
    /// directory name (which would be lossy).
    pub project_dir: String,
    pub title: String,
    /// Set when the session file lives under another session's directory
    /// (sub-agent invocation by e.g. `pi-subagents`); `None` otherwise.
    pub parent_id: Option<String>,
}

/// Pi tool names that write or edit files.  Argument key for the path
/// is `path` (not `file_path` / `filePath`).
const WRITE_TOOL_NAMES: &[&str] = &["write", "edit", "multi_edit"];

/// Maximum length for the auto-derived session title.
const MAX_TITLE_LEN: usize = 80;

/// List all Pi sessions (top-level + sub-agent) on disk.
pub fn list_sessions() -> Result<Vec<SessionInfo>> {
    let base = super::sessions_dir();
    if !base.exists() {
        return Ok(Vec::new());
    }

    let mut sessions = Vec::new();
    walk_for_jsonl(&base, &mut sessions);
    sessions.sort_by_key(|s| s.started_at);
    Ok(sessions)
}

/// Recursively walk the sessions tree, building `SessionInfo` for every
/// `*.jsonl` file whose first line is a Pi session header.
fn walk_for_jsonl(dir: &Path, out: &mut Vec<SessionInfo>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_for_jsonl(&path, out);
        } else if path.extension().is_some_and(|e| e == "jsonl") {
            if let Some(info) = build_session_info(&path) {
                out.push(info);
            }
        }
    }
}

/// Build a `SessionInfo` from a Pi JSONL file.  Returns `None` if the
/// file does not start with a valid session header.
fn build_session_info(path: &Path) -> Option<SessionInfo> {
    let raw = fs::read_to_string(path).ok()?;
    let mut lines = raw.lines();

    // Header (first non-empty line)
    let header_line = lines.find(|l| !l.trim().is_empty())?;
    let header: Value = serde_json::from_str(header_line).ok()?;
    if header.get("type")?.as_str()? != "session" {
        return None;
    }
    let session_id = header.get("id")?.as_str()?.to_string();
    let project_dir = header
        .get("cwd")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let started_at = header
        .get("timestamp")
        .and_then(|v| v.as_str())
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc))?;

    // Walk the rest for the last timestamp and the first user message
    // (used as the title).
    let mut last_ts = started_at;
    let mut title = String::new();
    for line in raw.lines().skip(1) {
        if line.trim().is_empty() {
            continue;
        }
        let entry: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if let Some(ts_str) = entry.get("timestamp").and_then(|v| v.as_str()) {
            if let Ok(dt) = DateTime::parse_from_rfc3339(ts_str) {
                last_ts = dt.with_timezone(&Utc);
            }
        }
        if title.is_empty() && entry.get("type").and_then(|v| v.as_str()) == Some("message") {
            if let Some(msg) = entry.get("message") {
                if msg.get("role").and_then(|v| v.as_str()) == Some("user") {
                    let extracted = extract_user_text(msg.get("content").unwrap_or(&Value::Null));
                    if !extracted.is_empty() {
                        title = truncate_title(&extracted);
                    }
                }
            }
        }
    }

    let parent_id = derive_parent_id(path);

    Some(SessionInfo {
        session_id,
        started_at,
        updated_at: last_ts,
        project_dir,
        title,
        parent_id,
    })
}

/// Derive a parent session UUID from the file path when the session
/// lives nested under another session's directory.
///
/// Layout for sub-agent files:
/// `<base>/--<encoded-cwd>--/<iso-ts>_<parent-uuid>/<entry-id>/run-N/session.jsonl`
///
/// We look for an ancestor directory whose name ends with a UUIDv7
/// after an underscore.  When found, that UUID is the parent session
/// ID.  Top-level sessions have no such ancestor and return `None`.
fn derive_parent_id(path: &Path) -> Option<String> {
    let mut current = path.parent()?;
    let base = super::sessions_dir();
    while current != base && current.parent().is_some() {
        if let Some(name) = current.file_name().and_then(|n| n.to_str()) {
            // Pattern: <ts>_<uuid>
            if let Some((_, uuid)) = name.rsplit_once('_') {
                if is_uuid_v7(uuid) {
                    return Some(uuid.to_string());
                }
            }
        }
        current = match current.parent() {
            Some(p) => p,
            None => break,
        };
    }
    None
}

/// Lightweight check that a string looks like a hyphenated UUIDv7.
fn is_uuid_v7(s: &str) -> bool {
    s.len() == 36
        && s.as_bytes()[14] == b'7'
        && s.as_bytes()[8] == b'-'
        && s.as_bytes()[13] == b'-'
        && s.as_bytes()[18] == b'-'
        && s.as_bytes()[23] == b'-'
}

/// Extract plain text from a Pi `message.content` field.
fn extract_user_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.replace('\n', " "),
        Value::Array(blocks) => {
            let mut parts = Vec::new();
            for block in blocks {
                if block.get("type").and_then(|v| v.as_str()) == Some("text") {
                    if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                        parts.push(t.replace('\n', " "));
                    }
                }
            }
            parts.join(" ")
        }
        _ => String::new(),
    }
}

/// Truncate a title to `MAX_TITLE_LEN` chars at a word boundary.
fn truncate_title(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.len() <= MAX_TITLE_LEN {
        return trimmed.to_string();
    }
    let truncated = &trimmed[..MAX_TITLE_LEN];
    let cut = truncated.rfind(' ').unwrap_or(MAX_TITLE_LEN);
    format!("{}...", &trimmed[..cut])
}

/// Locate a Pi session JSONL file by its UUIDv7.  Searches recursively
/// because sub-agent sessions live nested under their parent's dir.
pub fn find_session_file(session_uuid: &str) -> Option<PathBuf> {
    let base = super::sessions_dir();
    if !base.exists() {
        return None;
    }
    let mut found = None;
    walk_for_uuid(&base, session_uuid, &mut found);
    found
}

fn walk_for_uuid(dir: &Path, uuid: &str, out: &mut Option<PathBuf>) {
    if out.is_some() {
        return;
    }
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        if out.is_some() {
            return;
        }
        let path = entry.path();
        if path.is_dir() {
            walk_for_uuid(&path, uuid, out);
        } else if path.extension().is_some_and(|e| e == "jsonl") {
            // Cheap check: filename contains the uuid, OR
            // sub-agent path (`session.jsonl` whose header has the uuid).
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            if name.contains(uuid) {
                *out = Some(path.clone());
                return;
            }
            // Fallback: peek at the header
            if name == "session.jsonl" {
                if let Ok(raw) = fs::read_to_string(&path) {
                    if let Some(line) = raw.lines().find(|l| !l.trim().is_empty()) {
                        if let Ok(v) = serde_json::from_str::<Value>(line) {
                            if v.get("id").and_then(|x| x.as_str()) == Some(uuid) {
                                *out = Some(path.clone());
                                return;
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Whether a session's messages contain `needle` anywhere in their text
/// or tool-call blocks.
pub fn session_contains_text(session_id: &str, needle: &str) -> bool {
    match find_session_file(session_id) {
        Some(p) => file_contains_text(&p, needle),
        None => false,
    }
}

/// Whether the last `last_n` `message`-entries of a session contain `needle`.
pub fn session_tail_contains_text(session_id: &str, needle: &str, last_n: usize) -> bool {
    match find_session_file(session_id) {
        Some(p) => file_tail_contains_text(&p, needle, last_n),
        None => false,
    }
}

/// Whether a session contains a write/edit tool call targeting
/// `target_path`.
pub fn session_edited_file(session_id: &str, target_path: &str) -> bool {
    match find_session_file(session_id) {
        Some(p) => file_edited_file(&p, target_path),
        None => false,
    }
}

fn file_contains_text(path: &Path, needle: &str) -> bool {
    let raw = match fs::read_to_string(path) {
        Ok(r) => r,
        Err(_) => return false,
    };
    if !raw.contains(needle) {
        return false;
    }
    for line in raw.lines() {
        if !line.contains(needle) {
            continue;
        }
        let entry: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if entry.get("type").and_then(|v| v.as_str()) != Some("message") {
            continue;
        }
        let message = match entry.get("message") {
            Some(m) => m,
            None => continue,
        };
        if message_contains(message, needle) {
            return true;
        }
    }
    false
}

fn file_tail_contains_text(path: &Path, needle: &str, last_n: usize) -> bool {
    let raw = match fs::read_to_string(path) {
        Ok(r) => r,
        Err(_) => return false,
    };
    if !raw.contains(needle) {
        return false;
    }
    let messages: Vec<&str> = raw
        .lines()
        .filter(|l| {
            !l.trim().is_empty()
                && serde_json::from_str::<Value>(l)
                    .ok()
                    .and_then(|v| {
                        v.get("type")
                            .and_then(|t| t.as_str())
                            .map(|s| s.to_string())
                    })
                    .as_deref()
                    == Some("message")
        })
        .collect();
    let start = messages.len().saturating_sub(last_n);
    for line in &messages[start..] {
        if !line.contains(needle) {
            continue;
        }
        if let Ok(entry) = serde_json::from_str::<Value>(line) {
            if let Some(msg) = entry.get("message") {
                if message_contains(msg, needle) {
                    return true;
                }
            }
        }
    }
    false
}

fn message_contains(message: &Value, needle: &str) -> bool {
    let content = match message.get("content") {
        Some(c) => c,
        None => return false,
    };
    match content {
        Value::String(s) => s.contains(needle),
        Value::Array(blocks) => blocks.iter().any(|b| content_block_contains(b, needle)),
        _ => false,
    }
}

fn content_block_contains(block: &Value, needle: &str) -> bool {
    let block_type = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match block_type {
        "text" => block
            .get("text")
            .and_then(|v| v.as_str())
            .is_some_and(|t| t.contains(needle)),
        "thinking" => block
            .get("thinking")
            .and_then(|v| v.as_str())
            .is_some_and(|t| t.contains(needle)),
        "toolCall" => {
            if block
                .get("name")
                .and_then(|v| v.as_str())
                .is_some_and(|n| n.contains(needle))
            {
                return true;
            }
            block
                .get("arguments")
                .map(|a| a.to_string().contains(needle))
                .unwrap_or(false)
        }
        _ => false,
    }
}

fn file_edited_file(path: &Path, target_path: &str) -> bool {
    let filename = std::path::Path::new(target_path)
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or(target_path);
    let raw = match fs::read_to_string(path) {
        Ok(r) => r,
        Err(_) => return false,
    };
    if !raw.contains(filename) {
        return false;
    }
    for line in raw.lines() {
        if !line.contains(filename) {
            continue;
        }
        let entry: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if entry.get("type").and_then(|v| v.as_str()) != Some("message") {
            continue;
        }
        let blocks = match entry
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_array())
        {
            Some(b) => b,
            None => continue,
        };
        for block in blocks {
            if block.get("type").and_then(|v| v.as_str()) != Some("toolCall") {
                continue;
            }
            let name = match block.get("name").and_then(|v| v.as_str()) {
                Some(n) => n,
                None => continue,
            };
            if !WRITE_TOOL_NAMES.contains(&name) {
                continue;
            }
            // Pi argument key is `path`.
            let tool_path = block
                .get("arguments")
                .and_then(|a| a.get("path"))
                .or_else(|| block.get("arguments").and_then(|a| a.get("filePath")))
                .or_else(|| block.get("arguments").and_then(|a| a.get("file_path")))
                .and_then(|p| p.as_str());
            match tool_path {
                Some(p) if crate::file_path_matches(p, target_path) => return true,
                _ => continue,
            }
        }
    }
    false
}

/// Parse Pi `message.usage` into the unified `TokenUsage`.
///
/// Pi shape: `{ "input": N, "output": N, "cacheRead": N, "cacheWrite": N,
/// "totalTokens": N, "cost": { ... } }`.
fn parse_usage_from_value(usage: &Value) -> TokenUsage {
    TokenUsage {
        input: usage.get("input").and_then(|v| v.as_u64()).unwrap_or(0),
        output: usage.get("output").and_then(|v| v.as_u64()).unwrap_or(0),
        cache_read: usage.get("cacheRead").and_then(|v| v.as_u64()).unwrap_or(0),
        cache_write: usage
            .get("cacheWrite")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        cache_creation: 0,
        reasoning: usage.get("reasoning").and_then(|v| v.as_u64()).unwrap_or(0),
    }
}

/// List provider-agnostic `Message`s for a Pi session, in chronological
/// order.  User messages have `tokens: None`; assistant messages carry
/// the `usage` block parsed into `TokenUsage`.
pub fn list_messages(session_id: &str) -> Result<Vec<Message>> {
    let path = find_session_file(session_id).context("Session file not found")?;
    let raw = fs::read_to_string(&path).context("Failed to read session file")?;
    parse_messages(&raw, session_id)
}

pub fn parse_messages(content: &str, session_id: &str) -> Result<Vec<Message>> {
    let mut messages = Vec::new();
    let mut seen_ids: HashSet<String> = HashSet::new();

    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let entry: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if entry.get("type").and_then(|v| v.as_str()) != Some("message") {
            continue;
        }
        let entry_id = entry
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if !entry_id.is_empty() && !seen_ids.insert(entry_id.clone()) {
            continue;
        }

        let timestamp = match entry
            .get("timestamp")
            .and_then(|v| v.as_str())
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        {
            Some(dt) => dt.with_timezone(&Utc),
            None => continue,
        };

        let message = match entry.get("message") {
            Some(m) => m,
            None => continue,
        };
        let role = match message.get("role").and_then(|v| v.as_str()) {
            Some(r) => r,
            None => continue,
        };

        // Pi `toolResult` and other internal roles are not surfaced as
        // unified messages — they don't carry token usage and aren't
        // user-authored.  Map only `user` and `assistant`.
        let role_str = match role {
            "user" => "user",
            "assistant" => "assistant",
            _ => continue,
        };

        let model = message
            .get("modelID")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let tokens = if role_str == "assistant" {
            message.get("usage").map(parse_usage_from_value)
        } else {
            None
        };

        let msg_id = if entry_id.is_empty() {
            format!("{}-{}", role_str, timestamp.timestamp_millis())
        } else {
            entry_id
        };

        messages.push(Message {
            message_id: msg_id,
            session_id: session_id.to_string(),
            provider: Provider::Pi,
            role: role_str.to_string(),
            model,
            timestamp,
            tokens,
        });
    }
    Ok(messages)
}

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn write_jsonl(content: &str) -> NamedTempFile {
        let mut file = tempfile::Builder::new()
            .suffix(".jsonl")
            .tempfile()
            .unwrap();
        file.write_all(content.as_bytes()).unwrap();
        file
    }

    #[test]
    fn test_build_session_info_basic() {
        let file = write_jsonl(indoc! {r#"
            {"type":"session","version":3,"id":"019dddbf-6e66-7709-9a3b-b5a18736f890","timestamp":"2026-04-30T09:36:43.622Z","cwd":"/home/vaab/dev/doc"}
            {"type":"message","id":"abc","parentId":null,"timestamp":"2026-04-30T09:37:00.000Z","message":{"role":"user","content":[{"type":"text","text":"Hello world"}]}}
        "#});

        let info = build_session_info(file.path()).expect("header parses");
        assert_eq!(info.session_id, "019dddbf-6e66-7709-9a3b-b5a18736f890");
        assert_eq!(info.project_dir, "/home/vaab/dev/doc");
        assert_eq!(info.title, "Hello world");
        assert_eq!(info.parent_id, None);
        assert!(info.updated_at >= info.started_at);
    }

    #[test]
    fn test_build_session_info_rejects_non_pi_jsonl() {
        let file = write_jsonl(r#"{"type":"summary","id":"foo"}"#);
        assert!(build_session_info(file.path()).is_none());
    }

    #[test]
    fn test_is_uuid_v7() {
        assert!(is_uuid_v7("019dddbf-6e66-7709-9a3b-b5a18736f890"));
        assert!(!is_uuid_v7("550e8400-e29b-41d4-a716-446655440000")); // v4
        assert!(!is_uuid_v7("not-a-uuid"));
        assert!(!is_uuid_v7(""));
    }

    #[test]
    fn test_truncate_title() {
        assert_eq!(truncate_title("short"), "short");
        let long = "a".repeat(200);
        let out = truncate_title(&long);
        assert!(out.len() <= MAX_TITLE_LEN + 3);
        assert!(out.ends_with("..."));
    }

    #[test]
    fn test_extract_user_text_array() {
        let v = serde_json::json!([
            {"type": "text", "text": "first\nline"},
            {"type": "thinking", "thinking": "ignored"},
            {"type": "text", "text": "second"}
        ]);
        assert_eq!(extract_user_text(&v), "first line second");
    }

    #[test]
    fn test_file_contains_text_user_message() {
        let file = write_jsonl(indoc! {r#"
            {"type":"session","version":3,"id":"x","timestamp":"2026-04-30T09:36:43Z","cwd":"/tmp"}
            {"type":"message","id":"a","timestamp":"2026-04-30T09:37:00Z","message":{"role":"user","content":[{"type":"text","text":"please refactor auth"}]}}
        "#});
        assert!(file_contains_text(file.path(), "refactor auth"));
        assert!(!file_contains_text(file.path(), "missing"));
    }

    #[test]
    fn test_file_contains_text_tool_call_args() {
        let file = write_jsonl(indoc! {r#"
            {"type":"session","version":3,"id":"x","timestamp":"2026-04-30T09:36:43Z","cwd":"/tmp"}
            {"type":"message","id":"a","timestamp":"2026-04-30T09:37:00Z","message":{"role":"assistant","content":[{"type":"toolCall","name":"write","arguments":{"path":"/tmp/foo","content":"hi"}}]}}
        "#});
        assert!(file_contains_text(file.path(), "/tmp/foo"));
        assert!(file_contains_text(file.path(), "write"));
    }

    #[test]
    fn test_file_contains_text_thinking_block() {
        let file = write_jsonl(indoc! {r#"
            {"type":"session","version":3,"id":"x","timestamp":"2026-04-30T09:36:43Z","cwd":"/tmp"}
            {"type":"message","id":"a","timestamp":"2026-04-30T09:37:00Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"deeply hidden phrase"}]}}
        "#});
        assert!(file_contains_text(file.path(), "deeply hidden phrase"));
    }

    #[test]
    fn test_file_tail_contains_text_window() {
        let file = write_jsonl(indoc! {r#"
            {"type":"session","version":3,"id":"x","timestamp":"2026-04-30T09:36:43Z","cwd":"/tmp"}
            {"type":"message","id":"a","timestamp":"2026-04-30T09:37:00Z","message":{"role":"user","content":"old phrase here"}}
            {"type":"message","id":"b","timestamp":"2026-04-30T09:37:01Z","message":{"role":"user","content":"middle"}}
            {"type":"message","id":"c","timestamp":"2026-04-30T09:37:02Z","message":{"role":"user","content":"latest"}}
        "#});
        assert!(file_tail_contains_text(file.path(), "old phrase", 3));
        assert!(!file_tail_contains_text(file.path(), "old phrase", 1));
        assert!(file_tail_contains_text(file.path(), "latest", 1));
    }

    #[test]
    fn test_file_edited_file_write_tool() {
        let file = write_jsonl(indoc! {r#"
            {"type":"session","version":3,"id":"x","timestamp":"2026-04-30T09:36:43Z","cwd":"/home/u/proj"}
            {"type":"message","id":"a","timestamp":"2026-04-30T09:37:00Z","message":{"role":"assistant","content":[{"type":"toolCall","name":"write","arguments":{"path":"/home/u/proj/src/main.rs","content":"fn main(){}"}}]}}
        "#});
        assert!(file_edited_file(file.path(), "/home/u/proj/src/main.rs"));
        assert!(!file_edited_file(file.path(), "/home/u/proj/src/lib.rs"));
    }

    #[test]
    fn test_file_edited_file_relative_path() {
        let file = write_jsonl(indoc! {r#"
            {"type":"session","version":3,"id":"x","timestamp":"2026-04-30T09:36:43Z","cwd":"/home/u/proj"}
            {"type":"message","id":"a","timestamp":"2026-04-30T09:37:00Z","message":{"role":"assistant","content":[{"type":"toolCall","name":"edit","arguments":{"path":"src/main.rs"}}]}}
        "#});
        assert!(file_edited_file(file.path(), "/home/u/proj/src/main.rs"));
        assert!(!file_edited_file(file.path(), "/home/u/proj/src/lib.rs"));
    }

    #[test]
    fn test_file_edited_file_ignores_read_tool() {
        let file = write_jsonl(indoc! {r#"
            {"type":"session","version":3,"id":"x","timestamp":"2026-04-30T09:36:43Z","cwd":"/home/u"}
            {"type":"message","id":"a","timestamp":"2026-04-30T09:37:00Z","message":{"role":"assistant","content":[{"type":"toolCall","name":"read","arguments":{"path":"/home/u/foo.rs"}}]}}
        "#});
        assert!(!file_edited_file(file.path(), "/home/u/foo.rs"));
    }

    #[test]
    fn test_parse_messages_basic() {
        let jsonl = indoc! {r#"
            {"type":"session","version":3,"id":"x","timestamp":"2026-04-30T09:36:43Z","cwd":"/tmp"}
            {"type":"model_change","id":"m","timestamp":"2026-04-30T09:36:44Z","provider":"openai-codex","modelId":"gpt-5.5"}
            {"type":"message","id":"u1","timestamp":"2026-04-30T09:37:00Z","message":{"role":"user","content":"hello"}}
            {"type":"message","id":"a1","timestamp":"2026-04-30T09:37:01Z","message":{"role":"assistant","modelID":"gpt-5.5","usage":{"input":100,"output":50,"cacheRead":10,"cacheWrite":20,"totalTokens":180},"content":[{"type":"text","text":"hi"}]}}
        "#};
        let msgs = parse_messages(jsonl, "ses").unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, "user");
        assert!(msgs[0].tokens.is_none());
        assert_eq!(msgs[1].role, "assistant");
        assert_eq!(msgs[1].provider, Provider::Pi);
        assert_eq!(msgs[1].model.as_deref(), Some("gpt-5.5"));
        let toks = msgs[1].tokens.as_ref().unwrap();
        assert_eq!(toks.input, 100);
        assert_eq!(toks.output, 50);
        assert_eq!(toks.cache_read, 10);
        assert_eq!(toks.cache_write, 20);
    }

    #[test]
    fn test_parse_messages_skips_tool_result_role() {
        let jsonl = indoc! {r#"
            {"type":"session","version":3,"id":"x","timestamp":"2026-04-30T09:36:43Z","cwd":"/tmp"}
            {"type":"message","id":"tr","timestamp":"2026-04-30T09:37:00Z","message":{"role":"toolResult","toolCallId":"c","content":[{"type":"text","text":"output"}]}}
            {"type":"message","id":"u","timestamp":"2026-04-30T09:37:01Z","message":{"role":"user","content":"go"}}
        "#};
        let msgs = parse_messages(jsonl, "ses").unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, "user");
    }

    #[test]
    fn test_parse_messages_dedup_by_id() {
        // Same id appearing twice — second occurrence is dropped.
        let jsonl = indoc! {r#"
            {"type":"session","version":3,"id":"x","timestamp":"2026-04-30T09:36:43Z","cwd":"/tmp"}
            {"type":"message","id":"u1","timestamp":"2026-04-30T09:37:00Z","message":{"role":"user","content":"hi"}}
            {"type":"message","id":"u1","timestamp":"2026-04-30T09:37:01Z","message":{"role":"user","content":"hi again"}}
        "#};
        let msgs = parse_messages(jsonl, "ses").unwrap();
        assert_eq!(msgs.len(), 1);
    }
}

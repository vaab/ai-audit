use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::provider::{Message, Provider, TokenUsage};

#[derive(Debug, Clone)]
pub struct ToolUse {
    pub timestamp: DateTime<Utc>,
    pub tool: String,
    pub input: HashMap<String, Value>,
}

#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub session_id: String,
    /// Timestamp of the first entry in the session
    pub started_at: DateTime<Utc>,
    /// Timestamp of the last entry in the session
    pub updated_at: DateTime<Utc>,
    /// Project directory path (decoded from folder name)
    pub project_dir: String,
    /// Title derived from the first user message (truncated)
    pub title: String,
}

/// Maximum character length for session titles extracted from first user message.
const MAX_TITLE_LEN: usize = 80;

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

        let project_dir = decode_project_dir_name(
            &project_path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy(),
        );

        for file_entry in fs::read_dir(&project_path)? {
            let file_entry = file_entry?;
            let file_path = file_entry.path();
            if file_path.extension().is_some_and(|e| e == "jsonl") {
                if let Some(stem) = file_path.file_stem() {
                    let session_id = stem.to_string_lossy().to_string();
                    if let Ok(started_at) = get_session_first_timestamp(&file_path) {
                        let updated_at =
                            get_session_last_timestamp(&file_path).unwrap_or(started_at);
                        let title = get_first_user_message_text(&file_path).unwrap_or_default();
                        sessions.push(SessionInfo {
                            session_id,
                            started_at,
                            updated_at,
                            project_dir: project_dir.clone(),
                            title,
                        });
                    }
                }
            }
        }
    }

    sessions.sort_by_key(|s| s.started_at);
    Ok(sessions)
}

/// Decode a Claude Code project directory name back to a filesystem path.
///
/// Claude Code encodes paths by replacing `/` with `-`, e.g.
/// `-home-vaab-dev-rs-ai-audit` → `/home/vaab/dev/rs/ai-audit`.
///
/// Since `-` is ambiguous (could be path separator or literal `-` in
/// directory names), we resolve by checking if the decoded path exists
/// on the filesystem, trying the longest segments first.
/// Public wrapper for `decode_project_dir_name` (used by session detection).
pub fn decode_project_dir_name_pub(encoded: &str) -> String {
    decode_project_dir_name(encoded)
}

fn decode_project_dir_name(encoded: &str) -> String {
    // Strip leading '-' which represents the root '/'
    let stripped = encoded.strip_prefix('-').unwrap_or(encoded);
    let parts: Vec<&str> = stripped.split('-').collect();
    if parts.is_empty() {
        return encoded.to_string();
    }

    // Greedy left-to-right: try to merge parts into path components
    // by checking if the resulting path prefix exists
    let mut result = String::from("/");
    let mut i = 0;

    while i < parts.len() {
        // Try merging from current position: longest match first
        let mut best_end = i + 1;
        for end in (i + 1..=parts.len()).rev() {
            let candidate_component = parts[i..end].join("-");
            let candidate_path = if result == "/" {
                format!("/{}", candidate_component)
            } else {
                format!("{}/{}", result, candidate_component)
            };
            if std::path::Path::new(&candidate_path).exists() {
                best_end = end;
                break;
            }
        }

        let component = parts[i..best_end].join("-");
        if result == "/" {
            result = format!("/{}", component);
        } else {
            result = format!("{}/{}", result, component);
        }
        i = best_end;
    }

    result
}

/// Get the last timestamp from a session JSONL file.
///
/// Reads the last non-empty lines from the end of the file looking for a
/// timestamp. This is efficient because JSONL entries are appended, so the
/// last entry has the most recent timestamp.
fn get_session_last_timestamp(path: &Path) -> Result<DateTime<Utc>> {
    use std::io::{BufRead, Seek, SeekFrom};

    let file = fs::File::open(path)?;
    let file_len = file.metadata()?.len();

    // Read the last 8KB — enough to contain the last few entries
    let read_from = file_len.saturating_sub(8192);
    let mut reader = std::io::BufReader::new(file);
    reader.seek(SeekFrom::Start(read_from))?;

    let mut last_ts: Option<DateTime<Utc>> = None;
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(entry) = serde_json::from_str::<serde_json::Value>(&line) {
            if let Some(ts_str) = entry.get("timestamp").and_then(|v| v.as_str()) {
                if let Ok(dt) = DateTime::parse_from_rfc3339(ts_str) {
                    last_ts = Some(dt.with_timezone(&Utc));
                }
            }
        }
    }
    last_ts.ok_or_else(|| anyhow::anyhow!("No timestamp found in session file"))
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

/// Extract the text of the first user message from a session JSONL file.
///
/// Reads lines from the beginning until a `"type":"user"` entry is found,
/// then extracts its text content. The result is truncated to [`MAX_TITLE_LEN`]
/// characters at a word boundary and suffixed with "..." if truncated.
/// Newlines within the text are replaced with spaces.
fn get_first_user_message_text(path: &Path) -> Result<String> {
    let file = fs::File::open(path)?;
    let reader = std::io::BufReader::new(file);
    use std::io::BufRead;

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        // Quick pre-filter: only parse lines that look like user messages
        if !line.contains("\"type\":\"user\"") {
            continue;
        }
        let entry: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if entry.get("type").and_then(|v| v.as_str()) != Some("user") {
            continue;
        }
        let content = match entry.get("message").and_then(|m| m.get("content")) {
            Some(c) => c,
            None => continue,
        };
        let text = extract_user_text(content);
        if text.is_empty() {
            continue;
        }
        return Ok(truncate_title(&text));
    }
    anyhow::bail!("No user message found in session file")
}

/// Extract plain text from a Claude Code message content field.
///
/// Content may be a plain string or an array of content blocks.
fn extract_user_text(content: &serde_json::Value) -> String {
    match content {
        serde_json::Value::String(s) => s.replace('\n', " "),
        serde_json::Value::Array(blocks) => {
            let mut parts = Vec::new();
            for block in blocks {
                let block_type = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
                if block_type == "text" {
                    if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                        parts.push(text.replace('\n', " "));
                    }
                }
            }
            parts.join(" ")
        }
        _ => String::new(),
    }
}

/// Truncate a title to [`MAX_TITLE_LEN`] characters at a word boundary.
///
/// If the title exceeds the limit, it is cut at the last space before the
/// limit and "..." is appended.
fn truncate_title(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.len() <= MAX_TITLE_LEN {
        return trimmed.to_string();
    }
    // Find last space before the limit
    let truncated = &trimmed[..MAX_TITLE_LEN];
    let cut_at = truncated.rfind(' ').unwrap_or(MAX_TITLE_LEN);
    format!("{}...", &trimmed[..cut_at])
}

/// Check if a session's messages contain the given text.
///
/// Scans user and assistant message content blocks in the JSONL file.
pub fn session_contains_text(session_id: &str, needle: &str) -> bool {
    let session_file = match find_session_file(session_id) {
        Some(f) => f,
        None => return false,
    };
    file_contains_text(&session_file, needle)
}

/// Claude Code tool names that write or edit files.
const WRITE_TOOL_NAMES: &[&str] = &["Write", "Edit", "MultiEdit", "CreateFile"];

/// Check if a session contains any Write or Edit tool_use targeting the given file path.
///
/// The `target_path` should be an absolute, canonicalized path. It is matched against
/// the `file_path` input field of Write/Edit tool_use blocks. Both exact match and
/// suffix match (for relative paths stored in tool inputs) are tried.
pub fn session_edited_file(session_uuid: &str, target_path: &str) -> bool {
    let session_file = match find_session_file(session_uuid) {
        Some(f) => f,
        None => return false,
    };
    file_edited_file(&session_file, target_path)
}

/// Check if a session JSONL file contains a write/edit tool_use targeting the given path.
///
/// Uses a two-pass strategy: first a raw string check for the filename component
/// to skip files that cannot possibly match, then proper JSON parsing.
fn file_edited_file(path: &Path, target_path: &str) -> bool {
    use std::io::BufRead;

    // Fast pre-filter: check if the filename component appears anywhere in the raw file.
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

    // Slow path: parse JSONL and check tool_use blocks.
    let reader = std::io::BufReader::new(raw.as_bytes());

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        if line.trim().is_empty() {
            continue;
        }

        // Quick pre-filter: skip lines without the filename
        if !line.contains(filename) {
            continue;
        }

        // Only assistant messages have tool_use blocks
        if !line.contains("\"type\":\"assistant\"") {
            continue;
        }

        let entry: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        if entry.get("type").and_then(|v| v.as_str()) != Some("assistant") {
            continue;
        }

        let content = match entry.get("message").and_then(|m| m.get("content")) {
            Some(serde_json::Value::Array(arr)) => arr,
            _ => continue,
        };

        for block in content {
            let block_type = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
            if block_type != "tool_use" {
                continue;
            }
            let tool_name = match block.get("name").and_then(|n| n.as_str()) {
                Some(n) => n,
                None => continue,
            };
            if !WRITE_TOOL_NAMES.contains(&tool_name) {
                continue;
            }
            let tool_path = match block
                .get("input")
                .and_then(|i| i.get("file_path"))
                .and_then(|p| p.as_str())
            {
                Some(p) => p,
                None => continue,
            };
            if crate::file_path_matches(tool_path, target_path) {
                return true;
            }
        }
    }

    false
}

/// Check if the last `last_n` messages of a session contain the given text.
///
/// Like `session_contains_text` but only searches the most recent messages.
pub fn session_tail_contains_text(session_id: &str, needle: &str, last_n: usize) -> bool {
    let session_file = match find_session_file(session_id) {
        Some(f) => f,
        None => return false,
    };
    file_tail_contains_text(&session_file, needle, last_n)
}

/// Check if the last `last_n` message entries in a JSONL file contain the needle.
///
/// Reads lines from the end of the file, collects up to `last_n` user/assistant
/// entries, and searches their content.
fn file_tail_contains_text(path: &Path, needle: &str, last_n: usize) -> bool {
    use std::io::BufRead;

    // Fast path: read raw bytes and check if needle appears at all.
    let raw = match fs::read_to_string(path) {
        Ok(r) => r,
        Err(_) => return false,
    };
    if !raw.contains(needle) {
        return false;
    }

    // Collect all message lines, then take the last N
    let reader = std::io::BufReader::new(raw.as_bytes());
    let mut message_lines: Vec<String> = Vec::new();

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        if line.trim().is_empty() {
            continue;
        }
        // Quick check: is this a user or assistant message?
        if line.contains("\"type\":\"user\"") || line.contains("\"type\":\"assistant\"") {
            message_lines.push(line);
        }
    }

    // Take only the last N
    let start = message_lines.len().saturating_sub(last_n);
    for line in &message_lines[start..] {
        if !line.contains(needle) {
            continue;
        }

        let entry: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };

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
                if s.contains(needle) {
                    return true;
                }
            }
            serde_json::Value::Array(arr) => {
                for block in arr {
                    if content_block_contains(block, needle) {
                        return true;
                    }
                }
            }
            _ => {}
        }
    }

    false
}

/// Check if a session JSONL file contains the given text in message content.
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
    if !raw.contains(needle) {
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
        if !line.contains(needle) {
            continue;
        }

        let entry: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // Check user and assistant messages (text, tool_use, tool_result)
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
                if s.contains(needle) {
                    return true;
                }
            }
            serde_json::Value::Array(arr) => {
                for block in arr {
                    if content_block_contains(block, needle) {
                        return true;
                    }
                }
            }
            _ => {}
        }
    }

    false
}

/// Check if a content block (text, tool_use, or tool_result) contains the needle.
fn content_block_contains(block: &serde_json::Value, needle: &str) -> bool {
    let block_type = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match block_type {
        "text" => block
            .get("text")
            .and_then(|t| t.as_str())
            .is_some_and(|t| t.contains(needle)),
        "tool_use" => {
            // Check tool name
            if block
                .get("name")
                .and_then(|n| n.as_str())
                .is_some_and(|n| n.contains(needle))
            {
                return true;
            }
            // Check input (serialized as string for search)
            if let Some(input) = block.get("input") {
                let input_str = input.to_string();
                if input_str.contains(needle) {
                    return true;
                }
            }
            false
        }
        "tool_result" => {
            // tool_result content can be a string or array of blocks
            if let Some(content) = block.get("content") {
                match content {
                    serde_json::Value::String(s) => return s.contains(needle),
                    serde_json::Value::Array(arr) => {
                        for item in arr {
                            if item
                                .get("text")
                                .and_then(|t| t.as_str())
                                .is_some_and(|t| t.contains(needle))
                            {
                                return true;
                            }
                        }
                    }
                    _ => {}
                }
            }
            false
        }
        _ => false,
    }
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

/// Parse token usage from a Claude Code JSONL `message.usage` object.
fn parse_usage_from_value(usage: &Value) -> TokenUsage {
    TokenUsage {
        input: usage
            .get("input_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        output: usage
            .get("output_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        cache_read: usage
            .get("cache_read_input_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        cache_creation: usage
            .get("cache_creation_input_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        cache_write: 0,
        reasoning: 0,
    }
}

/// List messages with token usage data from a Claude Code session JSONL.
///
/// Parses `type: "assistant"` entries for direct messages and
/// `type: "progress"` entries for sub-agent token usage. Deduplicates
/// by `requestId` — multiple JSONL lines can share the same requestId
/// (streaming chunks); only the last occurrence (with final usage) is kept.
pub fn list_messages(session_uuid: &str) -> Result<Vec<Message>> {
    let session_file = find_session_file(session_uuid).context("Session file not found")?;
    let content = fs::read_to_string(&session_file).context("Failed to read session file")?;
    parse_messages(&content, session_uuid)
}

/// Parse messages with token data from raw JSONL content.
pub fn parse_messages(content: &str, session_id: &str) -> Result<Vec<Message>> {
    use std::io::BufRead;

    let reader = std::io::BufReader::new(content.as_bytes());

    // Collect entries keyed by (requestId or synthetic id) → (timestamp, message data).
    // For entries sharing a requestId, the last one wins (has final usage).
    let mut messages_by_id: Vec<(String, Message)> = Vec::new();
    let mut seen_request_ids: HashSet<String> = HashSet::new();
    // Collect in reverse order to let last occurrence win, then reverse at end.
    let lines: Vec<String> = reader
        .lines()
        .filter_map(|l| l.ok())
        .filter(|l| !l.trim().is_empty())
        .collect();

    // Process lines in reverse so we see the last occurrence of each requestId first.
    for line in lines.iter().rev() {
        let entry: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let entry_type = entry.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let timestamp_str = match entry.get("timestamp").and_then(|v| v.as_str()) {
            Some(ts) => ts,
            None => continue,
        };
        let timestamp = match DateTime::parse_from_rfc3339(timestamp_str) {
            Ok(dt) => dt.with_timezone(&Utc),
            Err(_) => continue,
        };

        match entry_type {
            "assistant" => {
                let message = match entry.get("message") {
                    Some(m) => m,
                    None => continue,
                };
                let usage = match message.get("usage") {
                    Some(u) => u,
                    None => continue,
                };

                // Deduplicate by requestId
                let request_id = entry
                    .get("requestId")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if !request_id.is_empty() {
                    if seen_request_ids.contains(&request_id) {
                        continue;
                    }
                    seen_request_ids.insert(request_id.clone());
                }

                let model = message
                    .get("model")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());

                let msg_id = if !request_id.is_empty() {
                    request_id.clone()
                } else {
                    format!("assistant-{}", timestamp.timestamp_millis())
                };

                let tokens = parse_usage_from_value(usage);

                messages_by_id.push((
                    msg_id.clone(),
                    Message {
                        message_id: msg_id,
                        session_id: session_id.to_string(),
                        provider: Provider::ClaudeCode,
                        role: "assistant".to_string(),
                        model,
                        timestamp,
                        tokens: Some(tokens),
                    },
                ));
            }
            "progress" => {
                // Sub-agent token usage at data.message.message.usage
                let usage = entry
                    .get("data")
                    .and_then(|d| d.get("message"))
                    .and_then(|m| m.get("message"))
                    .and_then(|m| m.get("usage"));
                let usage = match usage {
                    Some(u) => u,
                    None => continue,
                };

                // Deduplicate by nested requestId
                let request_id = entry
                    .get("data")
                    .and_then(|d| d.get("message"))
                    .and_then(|m| m.get("requestId"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if !request_id.is_empty() {
                    if seen_request_ids.contains(&request_id) {
                        continue;
                    }
                    seen_request_ids.insert(request_id.clone());
                }

                let model = entry
                    .get("data")
                    .and_then(|d| d.get("message"))
                    .and_then(|m| m.get("message"))
                    .and_then(|m| m.get("model"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());

                let msg_id = if !request_id.is_empty() {
                    format!("sub-{}", request_id)
                } else {
                    format!("progress-{}", timestamp.timestamp_millis())
                };

                let tokens = parse_usage_from_value(usage);

                messages_by_id.push((
                    msg_id.clone(),
                    Message {
                        message_id: msg_id,
                        session_id: session_id.to_string(),
                        provider: Provider::ClaudeCode,
                        role: "assistant".to_string(),
                        model,
                        timestamp,
                        tokens: Some(tokens),
                    },
                ));
            }
            "user" => {
                // User messages have no token data, but include them for completeness
                messages_by_id.push((
                    format!("user-{}", timestamp.timestamp_millis()),
                    Message {
                        message_id: format!("user-{}", timestamp.timestamp_millis()),
                        session_id: session_id.to_string(),
                        provider: Provider::ClaudeCode,
                        role: "user".to_string(),
                        model: None,
                        timestamp,
                        tokens: None,
                    },
                ));
            }
            _ => continue,
        }
    }

    // Reverse to restore chronological order
    messages_by_id.reverse();

    Ok(messages_by_id.into_iter().map(|(_, msg)| msg).collect())
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

        assert!(file_contains_text(file.path(), "Hello"));
        assert!(file_contains_text(file.path(), "world"));
        assert!(!file_contains_text(file.path(), "WORLD"));
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

    #[test]
    fn test_file_contains_text_tool_use_name() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"{{"type":"assistant","timestamp":"2024-01-15T10:30:00.000Z","message":{{"role":"assistant","content":[{{"type":"tool_use","name":"Bash","input":{{"command":"cargo build"}}}}]}}}}"#
        )
        .unwrap();

        assert!(file_contains_text(file.path(), "Bash"));
        assert!(file_contains_text(file.path(), "cargo build"));
        assert!(!file_contains_text(file.path(), "npm"));
    }

    #[test]
    fn test_file_contains_text_tool_result() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"{{"type":"user","timestamp":"2024-01-15T10:30:00.000Z","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"abc","content":"Compiling ai-audit v0.1.0"}}]}}}}"#
        )
        .unwrap();

        assert!(file_contains_text(file.path(), "Compiling ai-audit"));
        assert!(!file_contains_text(file.path(), "error"));
    }

    #[test]
    fn test_file_contains_text_tool_result_array_content() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"{{"type":"user","timestamp":"2024-01-15T10:30:00.000Z","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"abc","content":[{{"type":"text","text":"test result output"}}]}}]}}}}"#
        )
        .unwrap();

        assert!(file_contains_text(file.path(), "test result output"));
        assert!(!file_contains_text(file.path(), "missing"));
    }

    // === Tail search tests ===

    #[test]
    fn test_file_tail_contains_text_matches_recent_only() {
        let mut file = NamedTempFile::new().unwrap();
        // Write 5 messages; put the target in message 3 (not in last 2)
        writeln!(
            file,
            r#"{{"type":"user","timestamp":"2024-01-15T10:30:00.000Z","message":{{"role":"user","content":"first old message"}}}}"#
        ).unwrap();
        writeln!(
            file,
            r#"{{"type":"assistant","timestamp":"2024-01-15T10:30:01.000Z","message":{{"role":"assistant","content":[{{"type":"text","text":"second old message"}}]}}}}"#
        ).unwrap();
        writeln!(
            file,
            r#"{{"type":"user","timestamp":"2024-01-15T10:30:02.000Z","message":{{"role":"user","content":"unique target phrase"}}}}"#
        ).unwrap();
        writeln!(
            file,
            r#"{{"type":"assistant","timestamp":"2024-01-15T10:30:03.000Z","message":{{"role":"assistant","content":[{{"type":"text","text":"recent response"}}]}}}}"#
        ).unwrap();
        writeln!(
            file,
            r#"{{"type":"user","timestamp":"2024-01-15T10:30:04.000Z","message":{{"role":"user","content":"latest message"}}}}"#
        ).unwrap();

        // Last 2 should NOT find "unique target phrase"
        assert!(!file_tail_contains_text(
            file.path(),
            "unique target phrase",
            2
        ));
        // Last 3 SHOULD find it
        assert!(file_tail_contains_text(
            file.path(),
            "unique target phrase",
            3
        ));
        // Last 2 should find "latest message"
        assert!(file_tail_contains_text(file.path(), "latest message", 2));
    }

    #[test]
    fn test_file_tail_contains_text_skips_non_messages() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"{{"type":"summary","summary":"needle in summary"}}"#
        )
        .unwrap();
        writeln!(
            file,
            r#"{{"type":"user","timestamp":"2024-01-15T10:30:00.000Z","message":{{"role":"user","content":"clean message"}}}}"#
        ).unwrap();

        // "needle" is only in a summary entry, not in messages
        assert!(!file_tail_contains_text(file.path(), "needle", 5));
        assert!(file_tail_contains_text(file.path(), "clean message", 1));
    }

    #[test]
    fn test_file_tail_contains_text_empty_file() {
        let file = NamedTempFile::new().unwrap();
        assert!(!file_tail_contains_text(file.path(), "anything", 5));
    }

    // === Title extraction tests ===

    #[test]
    fn test_get_first_user_message_string_content() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"{{"type":"user","timestamp":"2024-01-15T10:30:00.000Z","message":{{"role":"user","content":"Hello, can you help me with Rust?"}}}}"#
        )
        .unwrap();

        let title = get_first_user_message_text(file.path()).unwrap();
        assert_eq!(title, "Hello, can you help me with Rust?");
    }

    #[test]
    fn test_get_first_user_message_array_content() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"{{"type":"user","timestamp":"2024-01-15T10:30:00.000Z","message":{{"role":"user","content":[{{"type":"text","text":"Add dark mode to the app"}}]}}}}"#
        )
        .unwrap();

        let title = get_first_user_message_text(file.path()).unwrap();
        assert_eq!(title, "Add dark mode to the app");
    }

    #[test]
    fn test_get_first_user_message_skips_non_user_entries() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, r#"{{"type":"summary","summary":"some summary"}}"#).unwrap();
        writeln!(
            file,
            r#"{{"type":"assistant","timestamp":"2024-01-15T10:30:00.000Z","message":{{"role":"assistant","content":[{{"type":"text","text":"assistant text"}}]}}}}"#
        )
        .unwrap();
        writeln!(
            file,
            r#"{{"type":"user","timestamp":"2024-01-15T10:30:01.000Z","message":{{"role":"user","content":"actual user question"}}}}"#
        )
        .unwrap();

        let title = get_first_user_message_text(file.path()).unwrap();
        assert_eq!(title, "actual user question");
    }

    #[test]
    fn test_get_first_user_message_returns_first_only() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"{{"type":"user","timestamp":"2024-01-15T10:30:00.000Z","message":{{"role":"user","content":"first question"}}}}"#
        )
        .unwrap();
        writeln!(
            file,
            r#"{{"type":"user","timestamp":"2024-01-15T10:30:01.000Z","message":{{"role":"user","content":"second question"}}}}"#
        )
        .unwrap();

        let title = get_first_user_message_text(file.path()).unwrap();
        assert_eq!(title, "first question");
    }

    #[test]
    fn test_get_first_user_message_empty_file() {
        let file = NamedTempFile::new().unwrap();
        assert!(get_first_user_message_text(file.path()).is_err());
    }

    #[test]
    fn test_get_first_user_message_replaces_newlines() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"{{"type":"user","timestamp":"2024-01-15T10:30:00.000Z","message":{{"role":"user","content":"line one\nline two\nline three"}}}}"#
        )
        .unwrap();

        let title = get_first_user_message_text(file.path()).unwrap();
        assert_eq!(title, "line one line two line three");
    }

    #[test]
    fn test_truncate_title_short_text() {
        assert_eq!(truncate_title("short title"), "short title");
    }

    #[test]
    fn test_truncate_title_exact_limit() {
        let text = "a".repeat(MAX_TITLE_LEN);
        assert_eq!(truncate_title(&text), text);
    }

    #[test]
    fn test_truncate_title_long_text_at_word_boundary() {
        let text = format!("{} {}", "word".repeat(15), "end".repeat(10));
        let result = truncate_title(&text);
        assert!(result.ends_with("..."));
        assert!(result.len() <= MAX_TITLE_LEN + 3); // +3 for "..."
    }

    #[test]
    fn test_truncate_title_trims_whitespace() {
        assert_eq!(truncate_title("  hello world  "), "hello world");
    }

    #[test]
    fn test_file_tail_contains_text_tool_use_in_recent() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"{{"type":"user","timestamp":"2024-01-15T10:30:00.000Z","message":{{"role":"user","content":"old message"}}}}"#
        ).unwrap();
        writeln!(
            file,
            r#"{{"type":"assistant","timestamp":"2024-01-15T10:30:01.000Z","message":{{"role":"assistant","content":[{{"type":"tool_use","name":"Bash","input":{{"command":"cargo test"}}}}]}}}}"#
        ).unwrap();

        // "cargo test" is in the last message (tool_use input)
        assert!(file_tail_contains_text(file.path(), "cargo test", 1));
        // "old message" is NOT in the last 1 message
        assert!(!file_tail_contains_text(file.path(), "old message", 1));
        // But IS in the last 2
        assert!(file_tail_contains_text(file.path(), "old message", 2));
    }

    // === file_edited_file tests ===

    #[test]
    fn test_file_edited_file_write_absolute_match() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"{{"type":"assistant","timestamp":"2024-01-15T10:30:00.000Z","message":{{"role":"assistant","content":[{{"type":"tool_use","name":"Write","input":{{"file_path":"/home/user/project/src/main.rs","content":"fn main() {{}}"}}}}]}}}}"#
        ).unwrap();

        assert!(file_edited_file(
            file.path(),
            "/home/user/project/src/main.rs"
        ));
        assert!(!file_edited_file(
            file.path(),
            "/home/user/project/src/lib.rs"
        ));
    }

    #[test]
    fn test_file_edited_file_edit_tool() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"{{"type":"assistant","timestamp":"2024-01-15T10:30:00.000Z","message":{{"role":"assistant","content":[{{"type":"tool_use","name":"Edit","input":{{"file_path":"/home/user/src/lib.rs","old_string":"old","new_string":"new"}}}}]}}}}"#
        ).unwrap();

        assert!(file_edited_file(file.path(), "/home/user/src/lib.rs"));
    }

    #[test]
    fn test_file_edited_file_multiedit_tool() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"{{"type":"assistant","timestamp":"2024-01-15T10:30:00.000Z","message":{{"role":"assistant","content":[{{"type":"tool_use","name":"MultiEdit","input":{{"file_path":"/home/user/src/mod.rs","edits":[]}}}}]}}}}"#
        ).unwrap();

        assert!(file_edited_file(file.path(), "/home/user/src/mod.rs"));
    }

    #[test]
    fn test_file_edited_file_createfile_tool() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"{{"type":"assistant","timestamp":"2024-01-15T10:30:00.000Z","message":{{"role":"assistant","content":[{{"type":"tool_use","name":"CreateFile","input":{{"file_path":"/home/user/new.rs","content":"// new"}}}}]}}}}"#
        ).unwrap();

        assert!(file_edited_file(file.path(), "/home/user/new.rs"));
    }

    #[test]
    fn test_file_edited_file_relative_path_in_tool() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"{{"type":"assistant","timestamp":"2024-01-15T10:30:00.000Z","message":{{"role":"assistant","content":[{{"type":"tool_use","name":"Write","input":{{"file_path":"src/main.rs","content":"fn main() {{}}"}}}}]}}}}"#
        ).unwrap();

        // Relative tool path should suffix-match against absolute target
        assert!(file_edited_file(
            file.path(),
            "/home/user/project/src/main.rs"
        ));
        assert!(!file_edited_file(
            file.path(),
            "/home/user/project/src/lib.rs"
        ));
    }

    #[test]
    fn test_file_edited_file_ignores_non_write_tools() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"{{"type":"assistant","timestamp":"2024-01-15T10:30:00.000Z","message":{{"role":"assistant","content":[{{"type":"tool_use","name":"Read","input":{{"file_path":"/home/user/src/main.rs"}}}}]}}}}"#
        ).unwrap();

        assert!(!file_edited_file(file.path(), "/home/user/src/main.rs"));
    }

    #[test]
    fn test_file_edited_file_ignores_user_messages() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"{{"type":"user","timestamp":"2024-01-15T10:30:00.000Z","message":{{"role":"user","content":"edit /home/user/src/main.rs"}}}}"#
        ).unwrap();

        assert!(!file_edited_file(file.path(), "/home/user/src/main.rs"));
    }

    #[test]
    fn test_file_edited_file_empty_file() {
        let file = NamedTempFile::new().unwrap();
        assert!(!file_edited_file(file.path(), "/home/user/src/main.rs"));
    }

    #[test]
    fn test_file_edited_file_multiple_tools_one_matches() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"{{"type":"assistant","timestamp":"2024-01-15T10:30:00.000Z","message":{{"role":"assistant","content":[{{"type":"tool_use","name":"Read","input":{{"file_path":"/home/user/src/main.rs"}}}},{{"type":"tool_use","name":"Write","input":{{"file_path":"/home/user/src/main.rs","content":"updated"}}}}]}}}}"#
        ).unwrap();

        assert!(file_edited_file(file.path(), "/home/user/src/main.rs"));
    }

    // === parse_messages tests ===

    #[test]
    fn test_parse_messages_assistant_with_usage() {
        let jsonl = r#"{"type":"assistant","timestamp":"2024-01-15T10:30:00.000Z","requestId":"req-001","message":{"role":"assistant","model":"claude-3-opus","usage":{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":200,"cache_creation_input_tokens":300}}}"#;
        let messages = parse_messages(jsonl, "test-session").unwrap();
        assert_eq!(messages.len(), 1);
        let msg = &messages[0];
        assert_eq!(msg.role, "assistant");
        assert_eq!(msg.provider, Provider::ClaudeCode);
        assert_eq!(msg.session_id, "test-session");
        assert_eq!(msg.model.as_deref(), Some("claude-3-opus"));
        assert_eq!(msg.message_id, "req-001");
        let tokens = msg.tokens.as_ref().unwrap();
        assert_eq!(tokens.input, 100);
        assert_eq!(tokens.output, 50);
        assert_eq!(tokens.cache_read, 200);
        assert_eq!(tokens.cache_creation, 300);
        assert_eq!(tokens.cache_write, 0);
        assert_eq!(tokens.reasoning, 0);
    }

    #[test]
    fn test_parse_messages_user_entry() {
        let jsonl = r#"{"type":"user","timestamp":"2024-01-15T10:30:00.000Z","message":{"role":"user","content":"Hello"}}"#;
        let messages = parse_messages(jsonl, "test-session").unwrap();
        assert_eq!(messages.len(), 1);
        let msg = &messages[0];
        assert_eq!(msg.role, "user");
        assert_eq!(msg.provider, Provider::ClaudeCode);
        assert!(msg.tokens.is_none());
        assert!(msg.model.is_none());
    }

    #[test]
    fn test_parse_messages_deduplication_by_request_id() {
        let jsonl = [
            r#"{"type":"assistant","timestamp":"2024-01-15T10:30:00.000Z","requestId":"req-dup","message":{"role":"assistant","model":"claude-3-opus","usage":{"input_tokens":10,"output_tokens":5}}}"#,
            r#"{"type":"assistant","timestamp":"2024-01-15T10:30:01.000Z","requestId":"req-dup","message":{"role":"assistant","model":"claude-3-opus","usage":{"input_tokens":20,"output_tokens":10}}}"#,
            r#"{"type":"assistant","timestamp":"2024-01-15T10:30:02.000Z","requestId":"req-dup","message":{"role":"assistant","model":"claude-3-opus","usage":{"input_tokens":30,"output_tokens":15}}}"#,
        ]
        .join("\n");
        let messages = parse_messages(&jsonl, "test-session").unwrap();
        // Deduplication: only the last occurrence (final usage) is kept
        assert_eq!(messages.len(), 1);
        let tokens = messages[0].tokens.as_ref().unwrap();
        assert_eq!(tokens.input, 30);
        assert_eq!(tokens.output, 15);
    }

    #[test]
    fn test_parse_messages_progress_subagent() {
        let jsonl = r#"{"type":"progress","timestamp":"2024-01-15T10:30:00.000Z","data":{"message":{"requestId":"sub-req-001","message":{"role":"assistant","model":"claude-3-haiku","usage":{"input_tokens":500,"output_tokens":250,"cache_read_input_tokens":100,"cache_creation_input_tokens":50}}}}}"#;
        let messages = parse_messages(jsonl, "test-session").unwrap();
        assert_eq!(messages.len(), 1);
        let msg = &messages[0];
        assert!(msg.message_id.starts_with("sub-"));
        assert_eq!(msg.role, "assistant");
        assert_eq!(msg.model.as_deref(), Some("claude-3-haiku"));
        let tokens = msg.tokens.as_ref().unwrap();
        assert_eq!(tokens.input, 500);
        assert_eq!(tokens.output, 250);
        assert_eq!(tokens.cache_read, 100);
        assert_eq!(tokens.cache_creation, 50);
    }

    #[test]
    fn test_parse_messages_chronological_order() {
        // JSONL lines in chronological file order — parse_messages preserves this order
        let jsonl = [
            r#"{"type":"assistant","timestamp":"2024-01-15T10:30:00.000Z","requestId":"req-first","message":{"role":"assistant","model":"claude-3-opus","usage":{"input_tokens":1,"output_tokens":1}}}"#,
            r#"{"type":"user","timestamp":"2024-01-15T10:31:00.000Z","message":{"role":"user","content":"ok"}}"#,
            r#"{"type":"assistant","timestamp":"2024-01-15T10:32:00.000Z","requestId":"req-third","message":{"role":"assistant","model":"claude-3-opus","usage":{"input_tokens":3,"output_tokens":3}}}"#,
        ]
        .join("\n");
        let messages = parse_messages(&jsonl, "test-session").unwrap();
        assert_eq!(messages.len(), 3);
        // Order preserved from JSONL file order (chronological)
        assert_eq!(messages[0].message_id, "req-first");
        assert_eq!(messages[1].role, "user");
        assert_eq!(messages[2].message_id, "req-third");
        assert!(messages[0].timestamp < messages[1].timestamp);
        assert!(messages[1].timestamp < messages[2].timestamp);
    }

    #[test]
    fn test_parse_messages_ignores_other_types() {
        let jsonl = [
            r#"{"type":"summary","timestamp":"2024-01-15T10:30:00.000Z","summary":"test"}"#,
            r#"{"type":"system","timestamp":"2024-01-15T10:30:01.000Z","data":"init"}"#,
            r#"{"type":"assistant","timestamp":"2024-01-15T10:30:02.000Z","requestId":"req-real","message":{"role":"assistant","model":"claude-3-opus","usage":{"input_tokens":10,"output_tokens":5}}}"#,
        ]
        .join("\n");
        let messages = parse_messages(&jsonl, "test-session").unwrap();
        // Only the assistant entry should be present
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].message_id, "req-real");
    }

    #[test]
    fn test_parse_messages_empty_content() {
        let messages = parse_messages("", "test-session").unwrap();
        assert!(messages.is_empty());
    }
}

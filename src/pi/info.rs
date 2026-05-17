//! Single-shot session metadata for `ai-audit session info` (pi).
//!
//! Pi sessions are JSONL files whose first line is always a
//! `{"type":"session", ...}` header carrying authoritative `cwd`,
//! `id`, and creation `timestamp`.  Subsequent lines are
//! `message`, `model_change`, and other events.
//!
//! Reads the file in a single forward pass.

use anyhow::{anyhow, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::fs;
use std::io::{BufRead, BufReader};

use crate::opencode::info::SessionDetailInfo;
use crate::provider::Provider;

use super::session;

/// Fetch session info from a pi JSONL transcript.
pub fn fetch_info(session_id: &str) -> Result<SessionDetailInfo> {
    let path = session::find_session_file(session_id)
        .ok_or_else(|| anyhow!("session not found: {}", session_id))?;
    let file = fs::File::open(&path)?;
    let reader = BufReader::new(file);

    let mut project_dir: Option<String> = None;
    let mut started_at: Option<DateTime<Utc>> = None;
    let mut last_ts: Option<DateTime<Utc>> = None;
    let mut message_count: usize = 0;
    let mut tool_call_count: usize = 0;
    let mut last_model: Option<String> = None;
    let mut last_provider: Option<String> = None;
    let mut last_entry_kind: Option<EntryKind> = None;
    let mut title: Option<String> = None;
    let mut header_seen = false;

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        if line.trim().is_empty() {
            continue;
        }
        let entry: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // Capture timestamp from every entry that has one.
        let entry_ts = entry
            .get("timestamp")
            .and_then(|v| v.as_str())
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&Utc));
        if let Some(ts) = entry_ts {
            if started_at.is_none() {
                started_at = Some(ts);
            }
            last_ts = Some(ts);
        }

        let entry_type = entry.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match entry_type {
            "session" => {
                header_seen = true;
                if let Some(cwd) = entry.get("cwd").and_then(|v| v.as_str()) {
                    if !cwd.is_empty() {
                        project_dir = Some(cwd.to_string());
                    }
                }
            }
            "message" => {
                let message = match entry.get("message") {
                    Some(m) => m,
                    None => continue,
                };
                let role = message.get("role").and_then(|v| v.as_str()).unwrap_or("");
                match role {
                    "user" => {
                        message_count += 1;
                        last_entry_kind = Some(EntryKind::User);
                        if title.is_none() {
                            if let Some(text) = extract_text_content(message) {
                                title = Some(truncate_title(&text));
                            }
                        }
                    }
                    "assistant" => {
                        message_count += 1;
                        last_entry_kind = Some(EntryKind::Assistant);
                        if let Some(model) = message
                            .get("model")
                            .and_then(|v| v.as_str())
                            .or_else(|| message.get("modelID").and_then(|v| v.as_str()))
                        {
                            last_model = Some(model.to_string());
                        }
                        if let Some(prov) = message.get("provider").and_then(|v| v.as_str()) {
                            last_provider = Some(prov.to_string());
                        }
                        if let Some(blocks) = message.get("content").and_then(|c| c.as_array()) {
                            for block in blocks {
                                if block.get("type").and_then(|t| t.as_str()) == Some("toolCall") {
                                    tool_call_count += 1;
                                }
                            }
                        }
                    }
                    "toolResult" => {
                        // Internal — does not count as a "message" in the
                        // user-facing sense.  But it can indicate an error.
                        if has_error_flag(message) {
                            last_entry_kind = Some(EntryKind::Error);
                        }
                    }
                    _ => {}
                }
            }
            "model_change" => {
                // Pi records provider/modelId on the change event itself.
                if let Some(prov) = entry.get("provider").and_then(|v| v.as_str()) {
                    last_provider = Some(prov.to_string());
                }
                if let Some(model) = entry
                    .get("modelId")
                    .and_then(|v| v.as_str())
                    .or_else(|| entry.get("modelID").and_then(|v| v.as_str()))
                {
                    last_model = Some(model.to_string());
                }
            }
            "error" => {
                last_entry_kind = Some(EntryKind::Error);
            }
            _ => {}
        }
    }

    if !header_seen {
        return Err(anyhow!(
            "pi session file missing header line: {}",
            session_id
        ));
    }

    let parent_session_id = pi_parent_id(&path);
    let aborted = matches!(last_entry_kind, Some(EntryKind::Error));

    let model = match (last_provider, last_model) {
        (Some(p), Some(m)) => Some(format!("{}/{}", p, m)),
        (Some(p), None) => Some(p),
        (None, Some(m)) => Some(m),
        (None, None) => None,
    };

    Ok(SessionDetailInfo {
        session_id: session_id.to_string(),
        provider: Provider::Pi,
        project_dir,
        title,
        started_at,
        last_updated_at: last_ts,
        message_count,
        tool_call_count,
        static_status: None,
        aborted,
        parent_session_id,
        agent: None,
        model,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    User,
    Assistant,
    Error,
}

const MAX_TITLE_LEN: usize = 80;

fn truncate_title(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.len() <= MAX_TITLE_LEN {
        return trimmed.to_string();
    }
    let truncated = &trimmed[..MAX_TITLE_LEN];
    let cut = truncated.rfind(' ').unwrap_or(MAX_TITLE_LEN);
    format!("{}...", &trimmed[..cut])
}

fn extract_text_content(message: &Value) -> Option<String> {
    let content = message.get("content")?;
    match content {
        Value::String(s) => {
            if s.is_empty() {
                None
            } else {
                Some(s.replace('\n', " "))
            }
        }
        Value::Array(blocks) => {
            let mut parts = Vec::new();
            for block in blocks {
                if block.get("type").and_then(|v| v.as_str()) == Some("text") {
                    if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                        parts.push(text.replace('\n', " "));
                    }
                }
            }
            if parts.is_empty() {
                None
            } else {
                Some(parts.join(" "))
            }
        }
        _ => None,
    }
}

fn has_error_flag(message: &Value) -> bool {
    if message.get("error").is_some_and(|v| !v.is_null()) {
        return true;
    }
    if let Some(blocks) = message.get("content").and_then(|c| c.as_array()) {
        for block in blocks {
            if block.get("isError").and_then(|v| v.as_bool()) == Some(true) {
                return true;
            }
        }
    }
    false
}

/// Derive a parent session UUID when the file lives nested under
/// another session's directory.  Mirrors the helper in `session.rs`
/// but operates over the path directly.
fn pi_parent_id(path: &std::path::Path) -> Option<String> {
    let mut current = path.parent()?;
    let base = super::sessions_dir();
    while current != base && current.parent().is_some() {
        if let Some(name) = current.file_name().and_then(|n| n.to_str()) {
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

fn is_uuid_v7(s: &str) -> bool {
    s.len() == 36
        && s.as_bytes()[14] == b'7'
        && s.as_bytes()[8] == b'-'
        && s.as_bytes()[13] == b'-'
        && s.as_bytes()[18] == b'-'
        && s.as_bytes()[23] == b'-'
}

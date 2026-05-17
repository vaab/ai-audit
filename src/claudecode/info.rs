//! Single-shot session metadata for `ai-audit session info` (Claude Code).
//!
//! Reads the JSONL transcript in a single forward pass: accumulates
//! message/tool counts, first/last timestamps, last-entry shape, and
//! tracks the last assistant model.
//!
//! The output struct is shared with the OpenCode path
//! ([`crate::opencode::info::SessionDetailInfo`]) so the CLI
//! dispatcher does not need per-provider type knowledge.

use anyhow::{anyhow, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::fs;
use std::io::{BufRead, BufReader};

use crate::opencode::info::SessionDetailInfo;
use crate::provider::Provider;

use super::session;

/// Fetch session info from a Claude Code JSONL transcript.
pub fn fetch_info(session_id: &str) -> Result<SessionDetailInfo> {
    let path = session::find_session_file(session_id)
        .ok_or_else(|| anyhow!("session not found: {}", session_id))?;
    let file = fs::File::open(&path)?;
    let reader = BufReader::new(file);

    let mut message_count: usize = 0;
    let mut tool_call_count: usize = 0;
    let mut first_ts: Option<DateTime<Utc>> = None;
    let mut last_ts: Option<DateTime<Utc>> = None;
    let mut last_model: Option<String> = None;
    let mut last_entry_kind: Option<EntryKind> = None;
    let mut project_dir: Option<String> = None;
    let mut title: Option<String> = None;

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

        // Timestamps live on every entry.  RFC3339.
        if let Some(ts) = entry
            .get("timestamp")
            .and_then(|v| v.as_str())
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&Utc))
        {
            if first_ts.is_none() {
                first_ts = Some(ts);
            }
            last_ts = Some(ts);
        }

        // Project directory: Claude Code stamps `cwd` on each line.
        if project_dir.is_none() {
            if let Some(cwd) = entry.get("cwd").and_then(|v| v.as_str()) {
                if !cwd.is_empty() {
                    project_dir = Some(cwd.to_string());
                }
            }
        }

        let entry_type = entry.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match entry_type {
            "user" => {
                message_count += 1;
                last_entry_kind = Some(EntryKind::User);
                if title.is_none() {
                    if let Some(text) = first_user_text(&entry) {
                        title = Some(truncate_title(&text));
                    }
                }
            }
            "assistant" => {
                message_count += 1;
                last_entry_kind = Some(EntryKind::Assistant);
                // Last-wins model capture.
                if let Some(model) = entry
                    .get("message")
                    .and_then(|m| m.get("model"))
                    .and_then(|v| v.as_str())
                {
                    last_model = Some(model.to_string());
                }
                // Count tool_use content blocks.
                if let Some(blocks) = entry
                    .get("message")
                    .and_then(|m| m.get("content"))
                    .and_then(|c| c.as_array())
                {
                    for block in blocks {
                        if block.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                            tool_call_count += 1;
                        }
                    }
                }
            }
            "error" => {
                last_entry_kind = Some(EntryKind::Error);
            }
            _ => {}
        }
    }

    // Project dir fallback: derive from encoded folder name if no `cwd`
    // was ever stamped (defensive — modern Claude Code always emits it).
    if project_dir.is_none() {
        if let Some(parent) = path.parent().and_then(|p| p.file_name()) {
            let encoded = parent.to_string_lossy();
            if !encoded.is_empty() {
                project_dir = Some(session::decode_project_dir_name_pub(&encoded));
            }
        }
    }

    let aborted = matches!(last_entry_kind, Some(EntryKind::Error));

    Ok(SessionDetailInfo {
        session_id: session_id.to_string(),
        provider: Provider::ClaudeCode,
        project_dir,
        title,
        started_at: first_ts,
        last_updated_at: last_ts,
        message_count,
        tool_call_count,
        static_status: None,
        aborted,
        parent_session_id: None,
        agent: None,
        model: last_model,
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

fn first_user_text(entry: &Value) -> Option<String> {
    let content = entry.get("message").and_then(|m| m.get("content"))?;
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

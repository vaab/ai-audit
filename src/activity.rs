use anyhow::{Context, Result};
use chrono::{DateTime, FixedOffset, Utc};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::Path;

use crate::config::Config;

/// Activity event types
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityType {
    Message,
    Permission,
}

impl ActivityType {
    pub fn as_str(&self) -> &'static str {
        match self {
            ActivityType::Message => "msg",
            ActivityType::Permission => "perm",
        }
    }
}

/// A single activity event
#[derive(Debug, Clone)]
pub struct ActivityEvent {
    /// Unix timestamp
    pub timestamp: i64,
    /// Identifier: CLIENT-TYPE@PROJECT_PATH (e.g., claude-msg@rs/ai-audit)
    pub ident: String,
    /// Session ID (UUID for Claude Code, ses_* for OpenCode)
    pub session_id: String,
    /// Activity type
    pub activity_type: ActivityType,
    /// The activity data (for JSON output)
    pub data: ActivityData,
}

/// Activity data payload
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum ActivityData {
    #[serde(rename = "msg")]
    Message { content: String },
    #[serde(rename = "perm")]
    Permission { rules: Vec<String> },
}

/// A timestamped activity for sorting
#[derive(Debug)]
pub struct TimestampedActivity {
    pub timestamp: i64,
    pub ident: String,
    pub event: ActivityEvent,
}

/// Client type (provider)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientType {
    Claude,
    Opencode,
}

impl ClientType {
    pub fn as_str(&self) -> &'static str {
        match self {
            ClientType::Claude => "claudecode",
            ClientType::Opencode => "opencode",
        }
    }
}

/// Parsed session entry from JSONL
#[derive(Debug, Deserialize)]
struct SessionEntry {
    #[serde(rename = "type")]
    entry_type: Option<String>,
    timestamp: Option<String>,
    message: Option<MessageContent>,
    cwd: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MessageContent {
    role: Option<String>,
    content: Option<serde_json::Value>,
}

/// Parse user messages from a Claude Code session JSONL file
pub fn parse_claudecode_messages(
    session_path: &Path,
    config: &Config,
) -> Result<Vec<ActivityEvent>> {
    let file = fs::File::open(session_path)
        .with_context(|| format!("Failed to open session file: {}", session_path.display()))?;
    let reader = BufReader::new(file);
    let mut events = Vec::new();

    let session_id = session_path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }

        let entry: SessionEntry = match serde_json::from_str(&line) {
            Ok(e) => e,
            Err(_) => continue,
        };

        // Only process user messages
        if entry.entry_type.as_deref() != Some("user") {
            continue;
        }

        let message = match entry.message {
            Some(m) if m.role.as_deref() == Some("user") => m,
            _ => continue,
        };

        let timestamp = match &entry.timestamp {
            Some(ts) => match DateTime::parse_from_rfc3339(ts) {
                Ok(dt) => dt.with_timezone(&Utc).timestamp(),
                Err(_) => continue,
            },
            None => continue,
        };

        // Extract message content
        let content = match message.content {
            Some(serde_json::Value::String(s)) => s,
            Some(serde_json::Value::Array(arr)) => {
                // Handle array of content blocks
                arr.iter()
                    .filter_map(|v| {
                        if let serde_json::Value::Object(obj) = v {
                            if obj.get("type").and_then(|t| t.as_str()) == Some("text") {
                                return obj.get("text").and_then(|t| t.as_str()).map(String::from);
                            }
                        }
                        None
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            }
            _ => continue,
        };

        // Skip empty messages (e.g., confirmation clicks, tool results)
        if content.trim().is_empty() {
            continue;
        }

        // Get project path from cwd
        let project_path = entry
            .cwd
            .as_deref()
            .map(|p| config.simplify_path(p))
            .unwrap_or_else(|| "unknown".to_string());

        let ident = format!(
            "{}-{}@{}",
            ClientType::Claude.as_str(),
            ActivityType::Message.as_str(),
            project_path
        );

        events.push(ActivityEvent {
            timestamp,
            ident,
            session_id: session_id.clone(),
            activity_type: ActivityType::Message,
            data: ActivityData::Message { content },
        });
    }

    Ok(events)
}

/// Parse permission grants from a Claude Code debug log
pub fn parse_claudecode_permissions(
    debug_path: &Path,
    session_path: Option<&Path>,
    config: &Config,
) -> Result<Vec<ActivityEvent>> {
    let content = fs::read_to_string(debug_path)
        .with_context(|| format!("Failed to read debug file: {}", debug_path.display()))?;

    let mut events = Vec::new();

    // Pattern: timestamp [DEBUG] Applying permission update: Adding N allow rule(s)...
    let re = Regex::new(
        r#"(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d+Z)\s+\[DEBUG\]\s+Applying permission update:\s+Adding\s+\d+\s+allow rule\(s\)[^:]*:\s*\[([^\]]+)\]"#,
    )?;

    // Try to get project path from session file
    let project_path = session_path
        .and_then(|p| get_project_path_from_session(p, config).ok())
        .or_else(|| get_project_path_from_debug_path(debug_path, config))
        .unwrap_or_else(|| "unknown".to_string());

    let session_id = debug_path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();

    for cap in re.captures_iter(&content) {
        let timestamp_str = &cap[1];
        let rules_str = &cap[2];

        let timestamp = match DateTime::parse_from_rfc3339(timestamp_str) {
            Ok(dt) => dt.with_timezone(&Utc).timestamp(),
            Err(_) => continue,
        };

        // Parse rules from the captured string
        let rules: Vec<String> = rules_str
            .split("\",\"")
            .map(|s| s.trim_matches(|c| c == '"' || c == ' ').to_string())
            .filter(|s| !s.is_empty())
            .collect();

        let ident = format!(
            "{}-{}@{}",
            ClientType::Claude.as_str(),
            ActivityType::Permission.as_str(),
            project_path
        );

        events.push(ActivityEvent {
            timestamp,
            ident,
            session_id: session_id.clone(),
            activity_type: ActivityType::Permission,
            data: ActivityData::Permission { rules },
        });
    }

    Ok(events)
}

/// Get project path from first entry in session file
fn get_project_path_from_session(session_path: &Path, config: &Config) -> Result<String> {
    let file = fs::File::open(session_path)?;
    let reader = BufReader::new(file);

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }

        let entry: SessionEntry = match serde_json::from_str(&line) {
            Ok(e) => e,
            Err(_) => continue,
        };

        if let Some(cwd) = entry.cwd {
            return Ok(config.simplify_path(&cwd));
        }
    }

    anyhow::bail!("No cwd found in session file")
}

/// Try to infer project path from debug file path
/// Debug files are in ~/.claude/debug/<session-id>.txt
/// We can try to find the corresponding session file
fn get_project_path_from_debug_path(debug_path: &Path, config: &Config) -> Option<String> {
    let session_id = debug_path.file_stem()?.to_string_lossy();
    let session_file = crate::claudecode::session::find_session_file(&session_id)?;

    get_project_path_from_session(&session_file, config).ok()
}

// ============================================================================
// OpenCode parsing
// ============================================================================

/// OpenCode session metadata
#[derive(Debug, Deserialize)]
struct OpenCodeSession {
    id: String,
    directory: Option<String>,
    #[allow(dead_code)]
    time: OpenCodeTime,
}

/// OpenCode message metadata  
#[derive(Debug, Deserialize)]
struct OpenCodeMessage {
    id: String,
    #[allow(dead_code)]
    #[serde(rename = "sessionID")]
    session_id: String,
    role: Option<String>,
    time: OpenCodeTime,
}

/// OpenCode part (message content)
#[derive(Debug, Deserialize)]
struct OpenCodePart {
    #[serde(rename = "type")]
    part_type: Option<String>,
    text: Option<String>,
    #[allow(dead_code)]
    #[serde(rename = "messageID")]
    message_id: String,
    #[allow(dead_code)]
    #[serde(rename = "sessionID")]
    session_id: String,
}

#[derive(Debug, Deserialize)]
struct OpenCodeTime {
    created: i64,
    #[allow(dead_code)]
    #[serde(default)]
    updated: Option<i64>,
}

/// Parse user messages from OpenCode storage
pub fn parse_opencode_messages(config: &Config) -> Result<Vec<ActivityEvent>> {
    let storage_dir = crate::opencode_data_dir().join("storage");
    parse_opencode_messages_from_dir(&storage_dir, config)
}

/// Parse user messages from OpenCode storage at a specific directory
/// (Internal function, also used for testing)
fn parse_opencode_messages_from_dir(
    storage_dir: &Path,
    config: &Config,
) -> Result<Vec<ActivityEvent>> {
    let message_dir = storage_dir.join("message");
    let part_dir = storage_dir.join("part");
    let session_dir = storage_dir.join("session");

    if !message_dir.exists() {
        return Ok(Vec::new());
    }

    let mut events = Vec::new();

    // Build session directory lookup (session_id -> directory path)
    let mut session_dirs: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    if session_dir.exists() {
        for project_entry in fs::read_dir(&session_dir)? {
            let project_entry = project_entry?;
            let project_path = project_entry.path();
            if !project_path.is_dir() {
                continue;
            }
            for session_file in fs::read_dir(&project_path)? {
                let session_file = session_file?;
                let path = session_file.path();
                if path.extension().is_some_and(|e| e == "json") {
                    if let Ok(content) = fs::read_to_string(&path) {
                        if let Ok(session) = serde_json::from_str::<OpenCodeSession>(&content) {
                            if let Some(dir) = session.directory {
                                session_dirs.insert(session.id, dir);
                            }
                        }
                    }
                }
            }
        }
    }

    // Process message directories (each is a session)
    for session_entry in fs::read_dir(&message_dir)? {
        let session_entry = session_entry?;
        let session_path = session_entry.path();
        if !session_path.is_dir() {
            continue;
        }

        let session_id = session_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();

        // Get project path for this session
        let project_path = session_dirs
            .get(&session_id)
            .map(|d| config.simplify_path(d))
            .unwrap_or_else(|| "unknown".to_string());

        // Process each message file
        for msg_entry in fs::read_dir(&session_path)? {
            let msg_entry = msg_entry?;
            let msg_path = msg_entry.path();
            if msg_path.extension().is_none_or(|e| e != "json") {
                continue;
            }

            let content = match fs::read_to_string(&msg_path) {
                Ok(c) => c,
                Err(_) => continue,
            };

            let message: OpenCodeMessage = match serde_json::from_str(&content) {
                Ok(m) => m,
                Err(_) => continue,
            };

            // Only process user messages
            if message.role.as_deref() != Some("user") {
                continue;
            }

            // Get message content from parts
            let msg_content = get_opencode_message_content(&part_dir, &message.id)?;
            if msg_content.trim().is_empty() {
                continue;
            }

            // Timestamp is in milliseconds
            let timestamp = message.time.created / 1000;

            let ident = format!(
                "{}-{}@{}",
                ClientType::Opencode.as_str(),
                ActivityType::Message.as_str(),
                project_path
            );

            events.push(ActivityEvent {
                timestamp,
                ident,
                session_id: session_id.clone(),
                activity_type: ActivityType::Message,
                data: ActivityData::Message {
                    content: msg_content,
                },
            });
        }
    }

    Ok(events)
}

/// Get message content from OpenCode parts
fn get_opencode_message_content(part_dir: &Path, message_id: &str) -> Result<String> {
    let msg_part_dir = part_dir.join(message_id);
    if !msg_part_dir.exists() {
        return Ok(String::new());
    }

    let mut text_parts = Vec::new();

    for part_entry in fs::read_dir(&msg_part_dir)? {
        let part_entry = part_entry?;
        let part_path = part_entry.path();
        if part_path.extension().is_none_or(|e| e != "json") {
            continue;
        }

        let content = match fs::read_to_string(&part_path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let part: OpenCodePart = match serde_json::from_str(&content) {
            Ok(p) => p,
            Err(_) => continue,
        };

        // Only include text parts
        if part.part_type.as_deref() == Some("text") {
            if let Some(text) = part.text {
                text_parts.push(text);
            }
        }
    }

    // Join all text parts
    Ok(text_parts.join("\n"))
}

/// List OpenCode sessions for identifiers
fn list_opencode_identifiers(config: &Config, identifiers: &mut Vec<String>) -> Result<()> {
    let storage_dir = crate::opencode_data_dir().join("storage");
    let session_dir = storage_dir.join("session");

    if !session_dir.exists() {
        return Ok(());
    }

    for project_entry in fs::read_dir(&session_dir)? {
        let project_entry = project_entry?;
        let project_path = project_entry.path();
        if !project_path.is_dir() {
            continue;
        }

        for session_file in fs::read_dir(&project_path)? {
            let session_file = session_file?;
            let path = session_file.path();
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }

            if let Ok(content) = fs::read_to_string(&path) {
                if let Ok(session) = serde_json::from_str::<OpenCodeSession>(&content) {
                    if let Some(dir) = session.directory {
                        let simplified = config.simplify_path(&dir);
                        let msg_ident = format!(
                            "{}-{}@{}",
                            ClientType::Opencode.as_str(),
                            ActivityType::Message.as_str(),
                            simplified
                        );

                        if !identifiers.contains(&msg_ident) {
                            identifiers.push(msg_ident);
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

/// List all available activity identifiers
pub fn list_identifiers(config: &Config) -> Result<Vec<String>> {
    let mut identifiers = Vec::new();

    // Claude Code sessions
    let projects_dir = crate::claudecode::projects_dir();
    if projects_dir.exists() {
        for project_entry in fs::read_dir(&projects_dir)? {
            let project_entry = project_entry?;
            let project_path = project_entry.path();
            if !project_path.is_dir() {
                continue;
            }

            // Get simplified project path from directory name
            // Directory names are URL-encoded paths like -home-vaab-dev-rs-ai-audit
            let dir_name = project_path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();

            let decoded_path = decode_project_dir_name(&dir_name);
            let simplified = config.simplify_path(&decoded_path);

            // Add both msg and perm identifiers
            let msg_ident = format!(
                "{}-{}@{}",
                ClientType::Claude.as_str(),
                ActivityType::Message.as_str(),
                simplified
            );
            let perm_ident = format!(
                "{}-{}@{}",
                ClientType::Claude.as_str(),
                ActivityType::Permission.as_str(),
                simplified
            );

            if !identifiers.contains(&msg_ident) {
                identifiers.push(msg_ident);
            }
            if !identifiers.contains(&perm_ident) {
                identifiers.push(perm_ident);
            }
        }
    }

    // OpenCode sessions
    list_opencode_identifiers(config, &mut identifiers)?;

    identifiers.sort();
    identifiers.dedup();
    Ok(identifiers)
}

/// Decode a Claude Code project directory name to a path
/// Example: -home-vaab-dev-rs-ai-audit -> /home/vaab/dev/rs/ai-audit
/// Example: -home-vaab--cfg-store-live-shared -> /home/vaab/.cfg-store/live-shared
fn decode_project_dir_name(name: &str) -> String {
    // The encoding:
    // - / becomes -
    // - /. (hidden dir) becomes -- (double dash represents /.)
    // - Absolute paths start with -

    // Handle double-dash (encoded /. for hidden directories)
    // Replace -- with /. placeholder
    let with_hidden = name.replace("--", "\x00HIDDEN\x00");

    // Replace leading - with /
    let path = if let Some(stripped) = with_hidden.strip_prefix('-') {
        format!("/{}", stripped)
    } else {
        with_hidden
    };

    // Replace remaining - with /
    let path = path.replace('-', "/");

    // Restore hidden directory markers (/.)
    path.replace("\x00HIDDEN\x00", "/.")
}

/// Fetch all activity events within a time range
pub fn fetch_activities(
    config: &Config,
    start: DateTime<FixedOffset>,
    end: DateTime<FixedOffset>,
    identifiers: &[String],
    session_ids: &[String],
) -> Result<Vec<ActivityEvent>> {
    let start_ts = start.timestamp();
    let end_ts = end.timestamp();

    let mut all_events = Vec::new();

    // Parse which clients and types are requested
    let filter = parse_identifier_filter(identifiers);

    // Claude Code sessions
    if filter.include_claude {
        let projects_dir = crate::claudecode::projects_dir();
        if projects_dir.exists() {
            for project_entry in fs::read_dir(&projects_dir)? {
                let project_entry = project_entry?;
                let project_path = project_entry.path();
                if !project_path.is_dir() {
                    continue;
                }

                // Process session files
                for file_entry in fs::read_dir(&project_path)? {
                    let file_entry = file_entry?;
                    let file_path = file_entry.path();
                    if file_path.extension().is_some_and(|e| e == "jsonl") {
                        // Skip subagent directories
                        if file_path
                            .parent()
                            .is_some_and(|p| p.file_name().is_some_and(|n| n == "subagents"))
                        {
                            continue;
                        }

                        // Parse messages
                        if filter.include_messages {
                            if let Ok(events) = parse_claudecode_messages(&file_path, config) {
                                for event in events {
                                    if event.timestamp >= start_ts
                                        && event.timestamp <= end_ts
                                        && filter.matches_ident(&event.ident)
                                    {
                                        all_events.push(event);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        // Parse permission events from debug logs
        if filter.include_permissions {
            let debug_dir = crate::claudecode::debug_dir();
            if debug_dir.exists() {
                for entry in fs::read_dir(&debug_dir)? {
                    let entry = entry?;
                    let path = entry.path();
                    if path.extension().is_some_and(|e| e == "txt") {
                        let session_id = path.file_stem().map(|s| s.to_string_lossy().to_string());
                        let session_file = session_id
                            .as_ref()
                            .and_then(|id| crate::claudecode::session::find_session_file(id));

                        if let Ok(events) =
                            parse_claudecode_permissions(&path, session_file.as_deref(), config)
                        {
                            for event in events {
                                if event.timestamp >= start_ts
                                    && event.timestamp <= end_ts
                                    && filter.matches_ident(&event.ident)
                                {
                                    all_events.push(event);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // OpenCode sessions
    if filter.include_opencode && filter.include_messages {
        if let Ok(events) = parse_opencode_messages(config) {
            for event in events {
                if event.timestamp >= start_ts
                    && event.timestamp <= end_ts
                    && filter.matches_ident(&event.ident)
                {
                    all_events.push(event);
                }
            }
        }
    }

    // Apply session ID filter
    if !session_ids.is_empty() {
        all_events.retain(|e| session_ids.iter().any(|sid| e.session_id == *sid));
    }

    // Sort by timestamp
    all_events.sort_by_key(|e| e.timestamp);

    Ok(all_events)
}

/// Filter for which activities to include
struct IdentifierFilter {
    include_claude: bool,
    include_opencode: bool,
    include_messages: bool,
    include_permissions: bool,
    /// Specific project path filters (empty = all)
    project_filters: Vec<String>,
}

impl IdentifierFilter {
    fn matches_ident(&self, ident: &str) -> bool {
        if self.project_filters.is_empty() {
            return true;
        }

        // Check if any filter matches
        for filter in &self.project_filters {
            if ident.contains(filter) {
                return true;
            }
        }

        false
    }
}

/// Parse identifier arguments into a filter
fn parse_identifier_filter(identifiers: &[String]) -> IdentifierFilter {
    if identifiers.is_empty() {
        return IdentifierFilter {
            include_claude: true,
            include_opencode: true,
            include_messages: true,
            include_permissions: true,
            project_filters: Vec::new(),
        };
    }

    let mut include_claude = false;
    let mut include_opencode = false;
    let mut include_messages = false;
    let mut include_permissions = false;
    let mut project_filters = Vec::new();

    for ident in identifiers {
        // Parse format: CLIENT-TYPE@PROJECT_PATH
        if let Some((prefix, project)) = ident.split_once('@') {
            project_filters.push(project.to_string());

            if prefix.starts_with("claudecode") {
                include_claude = true;
            }
            if prefix.starts_with("opencode") {
                include_opencode = true;
            }
            if prefix.ends_with("-msg") {
                include_messages = true;
            }
            if prefix.ends_with("-perm") {
                include_permissions = true;
            }
        } else {
            // Just a project path, include all types
            project_filters.push(ident.clone());
            include_claude = true;
            include_opencode = true;
            include_messages = true;
            include_permissions = true;
        }
    }

    // If no specific types selected, include all
    if !include_messages && !include_permissions {
        include_messages = true;
        include_permissions = true;
    }
    if !include_claude && !include_opencode {
        include_claude = true;
        include_opencode = true;
    }

    IdentifierFilter {
        include_claude,
        include_opencode,
        include_messages,
        include_permissions,
        project_filters,
    }
}

/// Format timestamp for human display (local timezone, ISO-8601)
pub fn format_timestamp_display(ts: i64) -> String {
    let dt = DateTime::from_timestamp(ts, 0)
        .map(|dt| dt.with_timezone(&chrono::Local))
        .unwrap_or_else(chrono::Local::now);

    dt.format("%Y-%m-%dT%H:%M:%S%z").to_string()
}

/// Get a summary of the activity for human display
pub fn activity_summary(event: &ActivityEvent) -> String {
    match &event.data {
        ActivityData::Message { content } => {
            // Truncate long messages for display
            let preview: String = content
                .chars()
                .take(80)
                .collect::<String>()
                .replace('\n', " ");
            if content.len() > 80 {
                format!("{}...", preview)
            } else {
                preview
            }
        }
        ActivityData::Permission { rules } => {
            format!("{} permission rules granted", rules.len())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::{tempdir, NamedTempFile};

    fn default_config() -> Config {
        Config::default()
    }

    /// Helper to create OpenCode storage structure for testing
    fn create_opencode_storage(
        base_dir: &std::path::Path,
        session_id: &str,
        session_dir_path: &str,
        messages: &[(&str, &str, i64, Vec<(&str, &str)>)], // (msg_id, role, timestamp_ms, parts: [(part_id, text)])
    ) -> std::io::Result<()> {
        let storage = base_dir.join("storage");
        let session_storage = storage.join("session").join("project_hash");
        let message_storage = storage.join("message").join(session_id);
        let part_storage = storage.join("part");

        fs::create_dir_all(&session_storage)?;
        fs::create_dir_all(&message_storage)?;
        fs::create_dir_all(&part_storage)?;

        // Create session file
        let session_json = format!(
            r#"{{"id":"{}","directory":"{}","time":{{"created":1700000000000}}}}"#,
            session_id, session_dir_path
        );
        fs::write(
            session_storage.join(format!("{}.json", session_id)),
            session_json,
        )?;

        // Create messages and parts
        for (msg_id, role, timestamp_ms, parts) in messages {
            let msg_json = format!(
                r#"{{"id":"{}","sessionID":"{}","role":"{}","time":{{"created":{}}}}}"#,
                msg_id, session_id, role, timestamp_ms
            );
            fs::write(message_storage.join(format!("{}.json", msg_id)), msg_json)?;

            // Create parts for this message
            let msg_part_dir = part_storage.join(msg_id);
            fs::create_dir_all(&msg_part_dir)?;

            for (part_id, text) in parts {
                let part_json = format!(
                    r#"{{"id":"{}","sessionID":"{}","messageID":"{}","type":"text","text":"{}"}}"#,
                    part_id, session_id, msg_id, text
                );
                fs::write(msg_part_dir.join(format!("{}.json", part_id)), part_json)?;
            }
        }

        Ok(())
    }

    #[test]
    fn test_activity_type_as_str() {
        assert_eq!(ActivityType::Message.as_str(), "msg");
        assert_eq!(ActivityType::Permission.as_str(), "perm");
    }

    #[test]
    fn test_client_type_as_str() {
        assert_eq!(ClientType::Claude.as_str(), "claudecode");
        assert_eq!(ClientType::Opencode.as_str(), "opencode");
    }

    #[test]
    fn test_decode_project_dir_name_simple() {
        assert_eq!(
            decode_project_dir_name("-home-user-dev-project"),
            "/home/user/dev/project"
        );
    }

    #[test]
    fn test_decode_project_dir_name_hidden() {
        // Double dash encodes hidden directories (/.)
        assert_eq!(
            decode_project_dir_name("-home-user--config"),
            "/home/user/.config"
        );
    }

    #[test]
    fn test_decode_project_dir_name_multiple_hidden() {
        // Double dash encodes /. (hidden dir prefix)
        // So --cfg becomes /.cfg, and -store becomes /store
        assert_eq!(
            decode_project_dir_name("-home-user--cfg-store--local"),
            "/home/user/.cfg/store/.local"
        );
    }

    #[test]
    fn test_activity_summary_short_message() {
        let event = ActivityEvent {
            timestamp: 0,
            ident: "test".to_string(),
            session_id: "test-session".to_string(),
            activity_type: ActivityType::Message,
            data: ActivityData::Message {
                content: "Hello world".to_string(),
            },
        };
        assert_eq!(activity_summary(&event), "Hello world");
    }

    #[test]
    fn test_activity_summary_long_message() {
        let long_content = "a".repeat(100);
        let event = ActivityEvent {
            timestamp: 0,
            ident: "test".to_string(),
            session_id: "test-session".to_string(),
            activity_type: ActivityType::Message,
            data: ActivityData::Message {
                content: long_content,
            },
        };
        let summary = activity_summary(&event);
        assert!(summary.ends_with("..."));
        assert_eq!(summary.len(), 83); // 80 chars + "..."
    }

    #[test]
    fn test_activity_summary_multiline() {
        let event = ActivityEvent {
            timestamp: 0,
            ident: "test".to_string(),
            session_id: "test-session".to_string(),
            activity_type: ActivityType::Message,
            data: ActivityData::Message {
                content: "Line 1\nLine 2\nLine 3".to_string(),
            },
        };
        // Newlines should be replaced with spaces
        assert_eq!(activity_summary(&event), "Line 1 Line 2 Line 3");
    }

    #[test]
    fn test_activity_summary_permission() {
        let event = ActivityEvent {
            timestamp: 0,
            ident: "test".to_string(),
            session_id: "test-session".to_string(),
            activity_type: ActivityType::Permission,
            data: ActivityData::Permission {
                rules: vec!["rule1".to_string(), "rule2".to_string()],
            },
        };
        assert_eq!(activity_summary(&event), "2 permission rules granted");
    }

    #[test]
    fn test_parse_claudecode_messages_user_message() {
        let config = default_config();

        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"{{"type":"user","timestamp":"2024-01-15T10:30:00.000Z","message":{{"role":"user","content":"Hello AI"}},"cwd":"/home/user/project"}}"#
        )
        .unwrap();

        let events = parse_claudecode_messages(&file.path().to_path_buf(), &config).unwrap();

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].activity_type, ActivityType::Message);
        assert!(events[0].ident.starts_with("claudecode-msg@"));
        if let ActivityData::Message { content } = &events[0].data {
            assert_eq!(content, "Hello AI");
        } else {
            panic!("Expected Message data");
        }
    }

    #[test]
    fn test_parse_claudecode_messages_skips_assistant() {
        let config = default_config();

        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"{{"type":"assistant","timestamp":"2024-01-15T10:30:00.000Z","message":{{"role":"assistant","content":"Hello human"}},"cwd":"/home/user/project"}}"#
        )
        .unwrap();

        let events = parse_claudecode_messages(&file.path().to_path_buf(), &config).unwrap();

        assert_eq!(events.len(), 0);
    }

    #[test]
    fn test_parse_claudecode_messages_array_content() {
        let config = default_config();

        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"{{"type":"user","timestamp":"2024-01-15T10:30:00.000Z","message":{{"role":"user","content":[{{"type":"text","text":"Part 1"}},{{"type":"text","text":"Part 2"}}]}},"cwd":"/home/user/project"}}"#
        )
        .unwrap();

        let events = parse_claudecode_messages(&file.path().to_path_buf(), &config).unwrap();

        assert_eq!(events.len(), 1);
        if let ActivityData::Message { content } = &events[0].data {
            assert_eq!(content, "Part 1\nPart 2");
        } else {
            panic!("Expected Message data");
        }
    }

    #[test]
    fn test_parse_claudecode_messages_skips_empty() {
        let config = default_config();

        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"{{"type":"user","timestamp":"2024-01-15T10:30:00.000Z","message":{{"role":"user","content":"   "}},"cwd":"/home/user/project"}}"#
        )
        .unwrap();

        let events = parse_claudecode_messages(&file.path().to_path_buf(), &config).unwrap();

        assert_eq!(events.len(), 0);
    }

    #[test]
    fn test_identifier_format() {
        let ident = format!(
            "{}-{}@{}",
            ClientType::Claude.as_str(),
            ActivityType::Message.as_str(),
            "DEV>rs/project"
        );
        assert_eq!(ident, "claudecode-msg@DEV>rs/project");

        let ident = format!(
            "{}-{}@{}",
            ClientType::Opencode.as_str(),
            ActivityType::Permission.as_str(),
            "WORK>app"
        );
        assert_eq!(ident, "opencode-perm@WORK>app");
    }

    #[test]
    fn test_activity_data_json_serialization() {
        let msg_data = ActivityData::Message {
            content: "test message".to_string(),
        };
        let json = serde_json::to_string(&msg_data).unwrap();
        assert!(json.contains(r#""type":"msg""#));
        assert!(json.contains(r#""content":"test message""#));

        let perm_data = ActivityData::Permission {
            rules: vec!["rule1".to_string()],
        };
        let json = serde_json::to_string(&perm_data).unwrap();
        assert!(json.contains(r#""type":"perm""#));
        assert!(json.contains(r#""rules":["rule1"]"#));
    }

    // =========================================================================
    // Claude Code Permission Tests
    // =========================================================================

    #[test]
    fn test_parse_claudecode_permissions_single_grant() {
        let config = default_config();

        let mut file = NamedTempFile::new().unwrap();
        // Note: Real format has no space after comma between rules
        writeln!(
            file,
            r#"2024-01-15T10:30:00.123Z [DEBUG] Applying permission update: Adding 2 allow rule(s) for this session: ["Bash(npm:*)","Read(~/project/**)"]"#
        )
        .unwrap();

        let events =
            parse_claudecode_permissions(&file.path().to_path_buf(), None, &config).unwrap();

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].activity_type, ActivityType::Permission);
        assert!(events[0].ident.starts_with("claudecode-perm@"));
        if let ActivityData::Permission { rules } = &events[0].data {
            assert_eq!(rules.len(), 2);
            assert!(rules.contains(&"Bash(npm:*)".to_string()));
            assert!(rules.contains(&"Read(~/project/**)".to_string()));
        } else {
            panic!("Expected Permission data");
        }
    }

    #[test]
    fn test_parse_claudecode_permissions_multiple_grants() {
        let config = default_config();

        let mut file = NamedTempFile::new().unwrap();
        // Note: Real format uses "to destination 'X':" not "for this session:"
        writeln!(
            file,
            r#"2024-01-15T10:30:00.123Z [DEBUG] Applying permission update: Adding 1 allow rule(s) to destination 'userSettings': ["Bash(git:*)"]
2024-01-15T10:31:00.456Z [DEBUG] Applying permission update: Adding 1 allow rule(s) to destination 'localSettings': ["Write(src/**)"]"#
        )
        .unwrap();

        let events =
            parse_claudecode_permissions(&file.path().to_path_buf(), None, &config).unwrap();

        assert_eq!(events.len(), 2);

        // First grant
        if let ActivityData::Permission { rules } = &events[0].data {
            assert_eq!(rules, &vec!["Bash(git:*)".to_string()]);
        } else {
            panic!("Expected Permission data");
        }

        // Second grant
        if let ActivityData::Permission { rules } = &events[1].data {
            assert_eq!(rules, &vec!["Write(src/**)".to_string()]);
        } else {
            panic!("Expected Permission data");
        }
    }

    #[test]
    fn test_parse_claudecode_permissions_no_grants() {
        let config = default_config();

        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"2024-01-15T10:30:00.123Z [DEBUG] Some other log message"#
        )
        .unwrap();

        let events =
            parse_claudecode_permissions(&file.path().to_path_buf(), None, &config).unwrap();

        assert_eq!(events.len(), 0);
    }

    #[test]
    fn test_parse_claudecode_permissions_with_project_path() {
        let config = default_config();

        // Create a session file with cwd
        let mut session_file = NamedTempFile::new().unwrap();
        writeln!(
            session_file,
            r#"{{"type":"user","timestamp":"2024-01-15T10:30:00.000Z","message":{{"role":"user","content":"test"}},"cwd":"/home/user/myproject"}}"#
        )
        .unwrap();

        let mut debug_file = NamedTempFile::new().unwrap();
        writeln!(
            debug_file,
            r#"2024-01-15T10:30:00.123Z [DEBUG] Applying permission update: Adding 1 allow rule(s) to destination 'localSettings': ["Bash(*)"]"#
        )
        .unwrap();

        let events = parse_claudecode_permissions(
            &debug_file.path().to_path_buf(),
            Some(&session_file.path().to_path_buf()),
            &config,
        )
        .unwrap();

        assert_eq!(events.len(), 1);
        assert!(events[0].ident.contains("/home/user/myproject"));
    }

    // =========================================================================
    // Claude Code Message Format Tests
    // =========================================================================

    #[test]
    fn test_parse_claudecode_messages_multiple_messages() {
        let config = default_config();

        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"{{"type":"user","timestamp":"2024-01-15T10:30:00.000Z","message":{{"role":"user","content":"First message"}},"cwd":"/project"}}
{{"type":"user","timestamp":"2024-01-15T10:31:00.000Z","message":{{"role":"user","content":"Second message"}},"cwd":"/project"}}"#
        )
        .unwrap();

        let events = parse_claudecode_messages(&file.path().to_path_buf(), &config).unwrap();

        assert_eq!(events.len(), 2);
        if let ActivityData::Message { content } = &events[0].data {
            assert_eq!(content, "First message");
        }
        if let ActivityData::Message { content } = &events[1].data {
            assert_eq!(content, "Second message");
        }
    }

    #[test]
    fn test_parse_claudecode_messages_mixed_types() {
        let config = default_config();

        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"{{"type":"user","timestamp":"2024-01-15T10:30:00.000Z","message":{{"role":"user","content":"User msg"}},"cwd":"/project"}}
{{"type":"assistant","timestamp":"2024-01-15T10:30:30.000Z","message":{{"role":"assistant","content":"Assistant response"}}}}
{{"type":"user","timestamp":"2024-01-15T10:31:00.000Z","message":{{"role":"user","content":"Follow up"}},"cwd":"/project"}}"#
        )
        .unwrap();

        let events = parse_claudecode_messages(&file.path().to_path_buf(), &config).unwrap();

        // Should only have user messages
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn test_parse_claudecode_messages_timestamp_parsing() {
        let config = default_config();

        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"{{"type":"user","timestamp":"2024-01-15T10:30:00.000Z","message":{{"role":"user","content":"Test"}},"cwd":"/project"}}"#
        )
        .unwrap();

        let events = parse_claudecode_messages(&file.path().to_path_buf(), &config).unwrap();

        assert_eq!(events.len(), 1);
        // 2024-01-15T10:30:00Z = 1705314600 unix timestamp
        assert_eq!(events[0].timestamp, 1705314600);
    }

    // =========================================================================
    // OpenCode Format Tests
    // =========================================================================

    #[test]
    fn test_parse_opencode_messages_basic() {
        let config = default_config();
        let temp = tempdir().unwrap();

        create_opencode_storage(
            temp.path(),
            "ses_123",
            "/home/user/project",
            &[(
                "msg_001",
                "user",
                1705314600000, // 2024-01-15T10:30:00Z in ms
                vec![("prt_001", "Hello from OpenCode")],
            )],
        )
        .unwrap();

        let events =
            parse_opencode_messages_from_dir(&temp.path().join("storage"), &config).unwrap();

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].activity_type, ActivityType::Message);
        assert!(events[0].ident.starts_with("opencode-msg@"));
        assert!(events[0].ident.contains("/home/user/project"));
        if let ActivityData::Message { content } = &events[0].data {
            assert_eq!(content, "Hello from OpenCode");
        } else {
            panic!("Expected Message data");
        }
    }

    #[test]
    fn test_parse_opencode_messages_skips_assistant() {
        let config = default_config();
        let temp = tempdir().unwrap();

        create_opencode_storage(
            temp.path(),
            "ses_123",
            "/home/user/project",
            &[
                (
                    "msg_001",
                    "user",
                    1705314600000,
                    vec![("prt_001", "User message")],
                ),
                (
                    "msg_002",
                    "assistant",
                    1705314601000,
                    vec![("prt_002", "Assistant response")],
                ),
            ],
        )
        .unwrap();

        let events =
            parse_opencode_messages_from_dir(&temp.path().join("storage"), &config).unwrap();

        // Should only have user message
        assert_eq!(events.len(), 1);
        if let ActivityData::Message { content } = &events[0].data {
            assert_eq!(content, "User message");
        }
    }

    #[test]
    fn test_parse_opencode_messages_multiple_parts() {
        let config = default_config();
        let temp = tempdir().unwrap();

        create_opencode_storage(
            temp.path(),
            "ses_123",
            "/home/user/project",
            &[(
                "msg_001",
                "user",
                1705314600000,
                vec![("prt_001", "Part 1"), ("prt_002", "Part 2")],
            )],
        )
        .unwrap();

        let events =
            parse_opencode_messages_from_dir(&temp.path().join("storage"), &config).unwrap();

        assert_eq!(events.len(), 1);
        if let ActivityData::Message { content } = &events[0].data {
            // Parts should be joined
            assert!(content.contains("Part 1"));
            assert!(content.contains("Part 2"));
        } else {
            panic!("Expected Message data");
        }
    }

    #[test]
    fn test_parse_opencode_messages_no_parts() {
        let config = default_config();
        let temp = tempdir().unwrap();

        // Create message without parts
        create_opencode_storage(
            temp.path(),
            "ses_123",
            "/home/user/project",
            &[(
                "msg_001",
                "user",
                1705314600000,
                vec![], // No parts
            )],
        )
        .unwrap();

        let events =
            parse_opencode_messages_from_dir(&temp.path().join("storage"), &config).unwrap();

        // Message with no content should be skipped
        assert_eq!(events.len(), 0);
    }

    #[test]
    fn test_parse_opencode_messages_empty_storage() {
        let config = default_config();
        let temp = tempdir().unwrap();

        // Just create empty storage structure
        fs::create_dir_all(temp.path().join("storage/message")).unwrap();

        let events =
            parse_opencode_messages_from_dir(&temp.path().join("storage"), &config).unwrap();

        assert_eq!(events.len(), 0);
    }

    #[test]
    fn test_parse_opencode_messages_nonexistent_storage() {
        let config = default_config();
        let temp = tempdir().unwrap();

        // Don't create any storage
        let events =
            parse_opencode_messages_from_dir(&temp.path().join("storage"), &config).unwrap();

        assert_eq!(events.len(), 0);
    }

    #[test]
    fn test_parse_opencode_messages_timestamp_conversion() {
        let config = default_config();
        let temp = tempdir().unwrap();

        create_opencode_storage(
            temp.path(),
            "ses_123",
            "/project",
            &[(
                "msg_001",
                "user",
                1705314600123, // milliseconds
                vec![("prt_001", "Test")],
            )],
        )
        .unwrap();

        let events =
            parse_opencode_messages_from_dir(&temp.path().join("storage"), &config).unwrap();

        assert_eq!(events.len(), 1);
        // Should be converted to seconds
        assert_eq!(events[0].timestamp, 1705314600);
    }

    #[test]
    fn test_parse_opencode_messages_unknown_session() {
        let config = default_config();
        let temp = tempdir().unwrap();

        // Create message storage without corresponding session
        let storage = temp.path().join("storage");
        let message_dir = storage.join("message").join("ses_unknown");
        let part_dir = storage.join("part").join("msg_001");
        fs::create_dir_all(&message_dir).unwrap();
        fs::create_dir_all(&part_dir).unwrap();

        // Create message
        fs::write(
            message_dir.join("msg_001.json"),
            r#"{"id":"msg_001","sessionID":"ses_unknown","role":"user","time":{"created":1705314600000}}"#,
        )
        .unwrap();

        // Create part
        fs::write(
            part_dir.join("prt_001.json"),
            r#"{"id":"prt_001","sessionID":"ses_unknown","messageID":"msg_001","type":"text","text":"Orphan message"}"#,
        )
        .unwrap();

        let events = parse_opencode_messages_from_dir(&storage, &config).unwrap();

        assert_eq!(events.len(), 1);
        // Should use "unknown" as project path
        assert!(events[0].ident.contains("unknown"));
    }

    // =========================================================================
    // Session ID population tests
    // =========================================================================

    #[test]
    fn test_claudecode_messages_session_id_from_filename() {
        let config = default_config();
        let dir = tempdir().unwrap();
        let session_file = dir.path().join("abc-def-1234.jsonl");

        fs::write(
            &session_file,
            r#"{"type":"user","timestamp":"2024-01-15T10:30:00.000Z","message":{"role":"user","content":"Hello"},"cwd":"/project"}"#,
        )
        .unwrap();

        let events = parse_claudecode_messages(&session_file, &config).unwrap();

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].session_id, "abc-def-1234");
    }

    #[test]
    fn test_claudecode_permissions_session_id_from_filename() {
        let config = default_config();
        let dir = tempdir().unwrap();
        let debug_file = dir.path().join("my-session-uuid.txt");

        fs::write(
            &debug_file,
            r#"2024-01-15T10:30:00.123Z [DEBUG] Applying permission update: Adding 1 allow rule(s) for this session: ["Bash(git:*)"]"#,
        )
        .unwrap();

        let events = parse_claudecode_permissions(&debug_file, None, &config).unwrap();

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].session_id, "my-session-uuid");
    }

    #[test]
    fn test_opencode_messages_session_id() {
        let config = default_config();
        let temp = tempdir().unwrap();

        create_opencode_storage(
            temp.path(),
            "ses_abc123",
            "/home/user/project",
            &[("msg_001", "user", 1705314600000, vec![("prt_001", "Hello")])],
        )
        .unwrap();

        let events =
            parse_opencode_messages_from_dir(&temp.path().join("storage"), &config).unwrap();

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].session_id, "ses_abc123");
    }

    #[test]
    fn test_session_filter_retains_matching() {
        let events = vec![
            ActivityEvent {
                timestamp: 100,
                ident: "claudecode-msg@project".to_string(),
                session_id: "session-A".to_string(),
                activity_type: ActivityType::Message,
                data: ActivityData::Message {
                    content: "msg A".to_string(),
                },
            },
            ActivityEvent {
                timestamp: 200,
                ident: "claudecode-msg@project".to_string(),
                session_id: "session-B".to_string(),
                activity_type: ActivityType::Message,
                data: ActivityData::Message {
                    content: "msg B".to_string(),
                },
            },
            ActivityEvent {
                timestamp: 300,
                ident: "claudecode-msg@project".to_string(),
                session_id: "session-A".to_string(),
                activity_type: ActivityType::Message,
                data: ActivityData::Message {
                    content: "msg A2".to_string(),
                },
            },
        ];

        // Filter to session-A only
        let session_ids = vec!["session-A".to_string()];
        let mut filtered = events.clone();
        filtered.retain(|e| session_ids.iter().any(|sid| e.session_id == *sid));

        assert_eq!(filtered.len(), 2);
        assert_eq!(filtered[0].session_id, "session-A");
        assert_eq!(filtered[1].session_id, "session-A");
    }

    #[test]
    fn test_session_filter_empty_keeps_all() {
        let events = vec![
            ActivityEvent {
                timestamp: 100,
                ident: "test".to_string(),
                session_id: "session-A".to_string(),
                activity_type: ActivityType::Message,
                data: ActivityData::Message {
                    content: "msg".to_string(),
                },
            },
            ActivityEvent {
                timestamp: 200,
                ident: "test".to_string(),
                session_id: "session-B".to_string(),
                activity_type: ActivityType::Message,
                data: ActivityData::Message {
                    content: "msg".to_string(),
                },
            },
        ];

        // Empty filter keeps all
        let session_ids: Vec<String> = vec![];
        let mut filtered = events.clone();
        if !session_ids.is_empty() {
            filtered.retain(|e| session_ids.iter().any(|sid| e.session_id == *sid));
        }

        assert_eq!(filtered.len(), 2);
    }

    #[test]
    fn test_session_filter_multiple_session_ids() {
        let events = vec![
            ActivityEvent {
                timestamp: 100,
                ident: "test".to_string(),
                session_id: "session-A".to_string(),
                activity_type: ActivityType::Message,
                data: ActivityData::Message {
                    content: "a".to_string(),
                },
            },
            ActivityEvent {
                timestamp: 200,
                ident: "test".to_string(),
                session_id: "session-B".to_string(),
                activity_type: ActivityType::Message,
                data: ActivityData::Message {
                    content: "b".to_string(),
                },
            },
            ActivityEvent {
                timestamp: 300,
                ident: "test".to_string(),
                session_id: "session-C".to_string(),
                activity_type: ActivityType::Message,
                data: ActivityData::Message {
                    content: "c".to_string(),
                },
            },
        ];

        // Filter to A and C
        let session_ids = vec!["session-A".to_string(), "session-C".to_string()];
        let mut filtered = events.clone();
        filtered.retain(|e| session_ids.iter().any(|sid| e.session_id == *sid));

        assert_eq!(filtered.len(), 2);
        assert_eq!(filtered[0].session_id, "session-A");
        assert_eq!(filtered[1].session_id, "session-C");
    }
}

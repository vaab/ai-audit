use anyhow::{Context, Result};
use chrono::{DateTime, FixedOffset, Utc};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::provider::Provider;

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
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
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

/// Per-session metadata — the single source of truth for child-session
/// detection and project-directory resolution.
#[derive(Debug)]
struct SessionMeta {
    /// Session identifier (UUID for Claude Code, `ses_*` for OpenCode).
    id: String,
    /// Simplified project directory (e.g. `DEV>rs/ai-audit`).
    project_dir: String,
    /// `true` when this session belongs to a subagent.
    is_child: bool,
    /// Provider that owns this session.
    provider: Provider,
    /// Path to the JSONL file (Claude Code only; `None` for OpenCode).
    session_file: Option<PathBuf>,
}

/// Aggregated session index built from all providers.
pub struct SessionIndex {
    sessions: Vec<SessionMeta>,
}

impl SessionIndex {
    /// Iterate over non-child (top-level) sessions.
    fn non_child(&self) -> impl Iterator<Item = &SessionMeta> {
        self.sessions.iter().filter(|s| !s.is_child)
    }

    /// Collect the IDs of all child sessions into a `HashSet`.
    fn child_ids(&self) -> HashSet<String> {
        self.sessions
            .iter()
            .filter(|s| s.is_child)
            .map(|s| s.id.clone())
            .collect()
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

/// Check whether a Claude Code JSONL session file belongs to a subagent.
///
/// Subagent entries carry a ``sessionId`` field (pointing to the parent
/// session).  We only need to inspect the first parseable line.
fn is_claudecode_child_session(path: &Path) -> bool {
    /// Minimal struct to probe for the parent-session indicator.
    #[derive(Deserialize)]
    struct Probe {
        #[serde(rename = "sessionId")]
        parent_session_id: Option<String>,
    }

    let file = match fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return false,
    };
    let reader = BufReader::new(file);
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => return false,
        };
        if line.trim().is_empty() {
            continue;
        }
        return match serde_json::from_str::<Probe>(&line) {
            Ok(p) => p.parent_session_id.is_some(),
            Err(_) => false,
        };
    }
    false
}

/// Scan all Claude Code session files and build `SessionMeta` entries.
///
/// For each `.jsonl` file under `~/.claude/projects/<encoded-path>/`:
/// - `is_child` is determined via [`is_claudecode_child_session`].
/// - `project_dir` is read from the first `cwd` entry in the JSONL,
///   falling back to decoding the parent directory name.
fn scan_claudecode_sessions(config: &Config) -> Vec<SessionMeta> {
    let projects_dir = crate::claudecode::projects_dir();
    let mut metas = Vec::new();

    if !projects_dir.exists() {
        return metas;
    }

    let project_entries = match fs::read_dir(&projects_dir) {
        Ok(entries) => entries,
        Err(_) => return metas,
    };

    for project_entry in project_entries {
        let project_entry = match project_entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let project_path = project_entry.path();
        if !project_path.is_dir() {
            continue;
        }

        // Resolve project_dir once per project directory: try reading cwd
        // from the first session file, fall back to decoding the dir name.
        let mut project_dir_cache: Option<String> = None;

        let file_entries = match fs::read_dir(&project_path) {
            Ok(entries) => entries,
            Err(_) => continue,
        };

        for file_entry in file_entries {
            let file_entry = match file_entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            let file_path = file_entry.path();
            if file_path.extension().is_none_or(|e| e != "jsonl") {
                continue;
            }

            let session_id = file_path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();

            let is_child = is_claudecode_child_session(&file_path);

            // Populate project_dir_cache lazily from the first non-child
            // session that yields a cwd.  Child sessions may have a
            // different cwd, but the directory-level fallback is fine.
            let project_dir = if let Some(ref cached) = project_dir_cache {
                cached.clone()
            } else {
                let dir = get_project_path_from_session(&file_path, config)
                    .ok()
                    .unwrap_or_else(|| {
                        let dir_name = project_path
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_default();
                        config.simplify_path(&decode_project_dir_name(&dir_name))
                    });
                project_dir_cache = Some(dir.clone());
                dir
            };

            metas.push(SessionMeta {
                id: session_id,
                project_dir,
                is_child,
                provider: Provider::ClaudeCode,
                session_file: Some(file_path),
            });
        }
    }

    metas
}

/// Scan all OpenCode session files and build `SessionMeta` entries.
///
/// Replaces the former `scan_opencode_sessions` + `OpenCodeSessionIndex`.
fn scan_opencode_sessions_to_meta(session_dir: &Path, config: &Config) -> Vec<SessionMeta> {
    let mut metas = Vec::new();

    if !session_dir.exists() {
        return metas;
    }

    let project_entries = match fs::read_dir(session_dir) {
        Ok(entries) => entries,
        Err(_) => return metas,
    };

    for project_entry in project_entries {
        let project_entry = match project_entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let project_path = project_entry.path();
        if !project_path.is_dir() {
            continue;
        }

        let session_files = match fs::read_dir(&project_path) {
            Ok(entries) => entries,
            Err(_) => continue,
        };

        for session_file in session_files {
            let session_file = match session_file {
                Ok(e) => e,
                Err(_) => continue,
            };
            let path = session_file.path();
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }
            let content = match fs::read_to_string(&path) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let session: OpenCodeSession = match serde_json::from_str(&content) {
                Ok(s) => s,
                Err(_) => continue,
            };

            let project_dir = session
                .directory
                .as_deref()
                .map(|d| config.simplify_path(d))
                .unwrap_or_else(|| "unknown".to_string());

            metas.push(SessionMeta {
                id: session.id,
                project_dir,
                is_child: session.parent_id.is_some(),
                provider: Provider::OpenCode,
                session_file: None,
            });
        }
    }

    metas
}

/// Scan OpenCode sessions from the SQLite database and build `SessionMeta` entries.
fn scan_opencode_sessions_to_meta_from_db(config: &Config) -> Vec<SessionMeta> {
    if !crate::opencode::db::db_exists() {
        return Vec::new();
    }
    let conn = match crate::opencode::db::open_db() {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    scan_opencode_sessions_to_meta_from_conn(&conn, config)
}

/// Scan OpenCode sessions from a DB connection (testable).
fn scan_opencode_sessions_to_meta_from_conn(
    conn: &rusqlite::Connection,
    config: &Config,
) -> Vec<SessionMeta> {
    let sessions = match crate::opencode::db::list_sessions_from_conn(conn) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };

    sessions
        .into_iter()
        .map(|s| {
            let project_dir = if s.project_dir.is_empty() {
                "unknown".to_string()
            } else {
                config.simplify_path(&s.project_dir)
            };
            SessionMeta {
                id: s.session_id,
                project_dir,
                is_child: s.parent_id.is_some(),
                provider: Provider::OpenCode,
                session_file: None,
            }
        })
        .collect()
}

/// Scan all Pi session files and build `SessionMeta` entries.
///
/// Pi stores sessions under `~/.pi/agent/sessions/--<encoded-cwd>--/`,
/// with sub-agent sessions nested deeper.  We iterate via
/// [`crate::pi::session::list_sessions`], which already handles the
/// recursive walk and reads cwd from each JSONL header.
fn scan_pi_sessions(config: &Config) -> Vec<SessionMeta> {
    let pi_sessions = match crate::pi::session::list_sessions() {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    pi_sessions
        .into_iter()
        .map(|s| {
            let project_dir = if s.project_dir.is_empty() {
                "unknown".to_string()
            } else {
                config.simplify_path(&s.project_dir)
            };
            // Re-derive the on-disk path so downstream message parsing
            // does not have to walk the tree again.
            let session_file = crate::pi::session::find_session_file(&s.session_id);
            SessionMeta {
                id: s.session_id,
                project_dir,
                is_child: s.parent_id.is_some(),
                provider: Provider::Pi,
                session_file,
            }
        })
        .collect()
}

/// Parse user messages from a Pi session JSONL file.
pub fn parse_pi_messages(
    session_path: &Path,
    config: &Config,
    project_dir: Option<&str>,
) -> Result<Vec<ActivityEvent>> {
    let file = fs::File::open(session_path)
        .with_context(|| format!("Failed to open session file: {}", session_path.display()))?;
    let reader = BufReader::new(file);
    let mut events = Vec::new();

    let mut session_id: Option<String> = None;
    let mut header_cwd: Option<String> = None;

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }

        let entry: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let entry_type = entry.get("type").and_then(|v| v.as_str()).unwrap_or("");

        // Capture session id and cwd from the header line.
        if entry_type == "session" {
            session_id = entry
                .get("id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            header_cwd = entry
                .get("cwd")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            continue;
        }

        if entry_type != "message" {
            continue;
        }

        let message = match entry.get("message") {
            Some(m) => m,
            None => continue,
        };

        if message.get("role").and_then(|v| v.as_str()) != Some("user") {
            continue;
        }

        let timestamp = match entry.get("timestamp").and_then(|v| v.as_str()) {
            Some(ts) => match DateTime::parse_from_rfc3339(ts) {
                Ok(dt) => dt.with_timezone(&Utc).timestamp(),
                Err(_) => continue,
            },
            None => continue,
        };

        // Extract user text content.  Pi user messages can be either a
        // plain string or an array of content blocks (text/image/...).
        let content = match message.get("content") {
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(serde_json::Value::Array(arr)) => arr
                .iter()
                .filter_map(|v| {
                    if let serde_json::Value::Object(obj) = v {
                        if obj.get("type").and_then(|t| t.as_str()) == Some("text") {
                            return obj.get("text").and_then(|t| t.as_str()).map(String::from);
                        }
                    }
                    None
                })
                .collect::<Vec<_>>()
                .join("\n"),
            _ => continue,
        };

        if content.trim().is_empty() {
            continue;
        }

        let project_path = match project_dir {
            Some(dir) => dir.to_string(),
            None => header_cwd
                .as_deref()
                .map(|p| config.simplify_path(p))
                .unwrap_or_else(|| "unknown".to_string()),
        };

        let sid = session_id.clone().unwrap_or_default();
        let ident = format!(
            "{}-{}@{}",
            Provider::Pi.as_str(),
            ActivityType::Message.as_str(),
            project_path,
        );

        events.push(ActivityEvent {
            timestamp,
            ident,
            session_id: sid,
            activity_type: ActivityType::Message,
            data: ActivityData::Message { content },
        });
    }

    events.sort_by_key(|e| e.timestamp);
    events.dedup_by(|a, b| a.timestamp == b.timestamp && a.data == b.data);
    Ok(events)
}

/// Merge two lists of `SessionMeta`, deduplicating by session ID.
/// The second list (DB) wins on conflict.
fn merge_session_metas(
    file_metas: Vec<SessionMeta>,
    db_metas: Vec<SessionMeta>,
) -> Vec<SessionMeta> {
    use std::collections::HashMap;

    let mut by_id: HashMap<String, SessionMeta> = HashMap::new();

    for m in file_metas {
        by_id.insert(m.id.clone(), m);
    }
    for m in db_metas {
        by_id.insert(m.id.clone(), m);
    }

    by_id.into_values().collect()
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
    project_dir: Option<&str>,
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

        // Get project path from project_dir if provided, otherwise from cwd
        let project_path = match project_dir {
            Some(dir) => dir.to_string(),
            None => entry
                .cwd
                .as_deref()
                .map(|p| config.simplify_path(p))
                .unwrap_or_else(|| "unknown".to_string()),
        };

        let ident = format!(
            "{}-{}@{}",
            Provider::ClaudeCode.as_str(),
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

    // Claude Code JSONL files occasionally contain repeated entry blocks;
    // deduplicate on (timestamp, data) within this single session.
    events.sort_by_key(|e| e.timestamp);
    events.dedup_by(|a, b| a.timestamp == b.timestamp && a.data == b.data);

    Ok(events)
}

/// Parse permission grants from a Claude Code debug log
pub fn parse_claudecode_permissions(
    debug_path: &Path,
    session_path: Option<&Path>,
    config: &Config,
    project_dir: Option<&str>,
) -> Result<Vec<ActivityEvent>> {
    let content = fs::read_to_string(debug_path)
        .with_context(|| format!("Failed to read debug file: {}", debug_path.display()))?;

    let mut events = Vec::new();

    // Pattern: timestamp [DEBUG] Applying permission update: Adding N allow rule(s)...
    let re = Regex::new(
        r#"(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d+Z)\s+\[DEBUG\]\s+Applying permission update:\s+Adding\s+\d+\s+allow rule\(s\)[^:]*:\s*\[([^\]]+)\]"#,
    )?;

    // Use canonical project_dir if provided, otherwise derive from session file
    let project_path = match project_dir {
        Some(dir) => dir.to_string(),
        None => session_path
            .and_then(|p| get_project_path_from_session(p, config).ok())
            .or_else(|| get_project_path_from_debug_path(debug_path, config))
            .unwrap_or_else(|| "unknown".to_string()),
    };

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
            Provider::ClaudeCode.as_str(),
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

/// Get the raw (un-simplified) project working directory from the
/// first JSONL entry that carries a `cwd` field.
///
/// Used by callers that need the real filesystem path (for example
/// the `token-usage` action, which walks `.git` ancestors).  Most
/// other callers want the display-simplified form — they should
/// call [`get_project_path_from_session`] instead.
pub fn get_project_cwd_raw(session_path: &Path) -> Result<String> {
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
            return Ok(cwd);
        }
    }

    anyhow::bail!("No cwd found in session file")
}

/// Get project path from first entry in session file (display-simplified).
fn get_project_path_from_session(session_path: &Path, config: &Config) -> Result<String> {
    let raw = get_project_cwd_raw(session_path)?;
    Ok(config.simplify_path(&raw))
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
    /// Present in subagent sessions; points to the parent session.
    #[serde(rename = "parentID")]
    parent_id: Option<String>,
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

/// Parse user messages from OpenCode storage at a specific directory.
///
/// Builds a local session index to determine child sessions and project
/// directories.  Used by tests and the public [`parse_opencode_messages`].
fn parse_opencode_messages_from_dir(
    storage_dir: &Path,
    config: &Config,
) -> Result<Vec<ActivityEvent>> {
    let session_dir = storage_dir.join("session");
    let metas = scan_opencode_sessions_to_meta(&session_dir, config);
    let index = SessionIndex { sessions: metas };
    parse_opencode_messages_with_index(storage_dir, config, &index)
}

/// Parse user messages from OpenCode storage using a pre-built session index.
///
/// This is the "dumb" parser: it skips child sessions based on the
/// provided index and resolves project directories from it.
fn parse_opencode_messages_with_index(
    storage_dir: &Path,
    _config: &Config,
    index: &SessionIndex,
) -> Result<Vec<ActivityEvent>> {
    let message_dir = storage_dir.join("message");
    let part_dir = storage_dir.join("part");

    if !message_dir.exists() {
        return Ok(Vec::new());
    }

    let mut events = Vec::new();
    let children = index.child_ids();

    // Build a session_id → project_dir lookup from the index.
    let dir_map: std::collections::HashMap<&str, &str> = index
        .sessions
        .iter()
        .map(|s| (s.id.as_str(), s.project_dir.as_str()))
        .collect();

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

        // Skip subagent sessions
        if children.contains(&session_id) {
            continue;
        }

        // Get project path for this session
        let project_path = dir_map
            .get(session_id.as_str())
            .map(|d| d.to_string())
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
                Provider::OpenCode.as_str(),
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

/// Get message content from OpenCode parts in the SQLite database.
fn get_opencode_message_content_from_db(
    conn: &rusqlite::Connection,
    message_id: &str,
) -> Result<String> {
    let parts = crate::opencode::db::get_parts_for_message(conn, message_id)?;

    let mut text_parts = Vec::new();
    for part in &parts {
        if part.get("type").and_then(|v| v.as_str()) == Some("text") {
            if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                text_parts.push(text.to_string());
            }
        }
    }

    Ok(text_parts.join("\n"))
}

/// Parse user messages from OpenCode SQLite database using a pre-built session index.
fn parse_opencode_messages_from_db(
    conn: &rusqlite::Connection,
    _config: &Config,
    index: &SessionIndex,
) -> Result<Vec<ActivityEvent>> {
    let mut events = Vec::new();
    let children = index.child_ids();

    // Build a session_id → project_dir lookup from the index.
    let dir_map: std::collections::HashMap<&str, &str> = index
        .sessions
        .iter()
        .map(|s| (s.id.as_str(), s.project_dir.as_str()))
        .collect();

    // Get all sessions from the index that are OpenCode and non-child
    for meta in index.non_child() {
        if meta.provider != Provider::OpenCode {
            continue;
        }

        let session_id = &meta.id;

        // Skip subagent sessions
        if children.contains(session_id) {
            continue;
        }

        let project_path = dir_map
            .get(session_id.as_str())
            .map(|d| d.to_string())
            .unwrap_or_else(|| "unknown".to_string());

        // Get messages for this session from DB
        let messages = match crate::opencode::db::get_messages_for_session(conn, session_id) {
            Ok(m) => m,
            Err(_) => continue,
        };

        for (msg_id, data) in &messages {
            let role = data
                .get("role")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            if role != "user" {
                continue;
            }

            let msg_content = match get_opencode_message_content_from_db(conn, msg_id) {
                Ok(c) => c,
                Err(_) => continue,
            };

            if msg_content.trim().is_empty() {
                continue;
            }

            // Timestamp is in milliseconds
            let time_created = data
                .get("time")
                .and_then(|t| t.get("created"))
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let timestamp = time_created / 1000;

            let ident = format!(
                "{}-{}@{}",
                Provider::OpenCode.as_str(),
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

/// Merge activity events from file and DB sources, deduplicating by
/// (timestamp, session_id, ident). DB wins on conflict.
fn merge_activity_events(
    file_events: Vec<ActivityEvent>,
    db_events: Vec<ActivityEvent>,
) -> Vec<ActivityEvent> {
    // Build a set of keys from DB events
    let db_keys: HashSet<(i64, &str, &str)> = db_events
        .iter()
        .map(|e| (e.timestamp, e.session_id.as_str(), e.ident.as_str()))
        .collect();

    let mut merged = Vec::new();

    // Add file events that don't conflict with DB
    for event in file_events {
        let key = (
            event.timestamp,
            event.session_id.as_str(),
            event.ident.as_str(),
        );
        if !db_keys.contains(&key) {
            merged.push(event);
        }
    }

    // Add all DB events
    merged.extend(db_events);

    merged
}

fn build_session_index(config: &Config, filter: &IdentifierFilter) -> SessionIndex {
    let t = std::time::Instant::now();
    let mut all_sessions = Vec::new();
    if filter.include_claude {
        match crate::claudecode::session_index::update_and_load(config) {
            Ok(idx) => {
                for s in idx.all() {
                    push_session_metas(
                        &mut all_sessions,
                        config,
                        s,
                        Provider::ClaudeCode,
                        s.path.clone(),
                    );
                }
            }
            Err(e) => {
                log::warn!(
                    "claudecode session-index cache failed ({}); falling back to full scan",
                    e
                );
                all_sessions.extend(scan_claudecode_sessions(config));
            }
        }
    }
    if filter.include_opencode {
        match crate::opencode::session_index::update_and_load(config) {
            Ok(idx) => {
                for s in idx.all() {
                    push_session_metas(&mut all_sessions, config, s, Provider::OpenCode, None);
                }
            }
            Err(e) => {
                log::warn!(
                    "opencode session-index cache failed ({}); falling back to full scan",
                    e
                );
                let oc_session_dir = crate::opencode_data_dir().join("storage").join("session");
                let file_oc_metas = scan_opencode_sessions_to_meta(&oc_session_dir, config);
                let db_oc_metas = scan_opencode_sessions_to_meta_from_db(config);
                all_sessions.extend(merge_session_metas(file_oc_metas, db_oc_metas));
            }
        }
    }
    if filter.include_pi {
        match crate::pi::session_index::update_and_load(config) {
            Ok(idx) => {
                for s in idx.all() {
                    push_session_metas(&mut all_sessions, config, s, Provider::Pi, s.path.clone());
                }
            }
            Err(e) => {
                log::warn!(
                    "pi session-index cache failed ({}); falling back to full scan",
                    e
                );
                all_sessions.extend(scan_pi_sessions(config));
            }
        }
    }
    log::debug!(
        "build_session_index: {} sessions ({:?})",
        all_sessions.len(),
        t.elapsed()
    );
    SessionIndex {
        sessions: all_sessions,
    }
}

/// Emit one `SessionMeta` per cwd recorded for this cached session.
///
/// Multi-cwd sessions (a session that recorded different cwds across
/// its lifetime — common in Claude after a `cd`) appear once per
/// distinct cwd in the resulting index.  Iteration in
/// ``collect_all_activity_events`` deduplicates by session id before
/// parsing the underlying file.  Lookup paths
/// (``enumerate_files_for_ident_with_index``,
/// ``list_identifiers_with_index``) naturally surface each
/// (session, cwd) pair as a distinct entry, with the existing
/// dedup-on-output handling collisions.
///
/// Sessions whose ``cwds`` set is empty (no cwd ever recorded —
/// e.g. an OpenCode row with NULL directory) emit a single entry
/// under the ``"unknown"`` project_dir, matching legacy behaviour.
fn push_session_metas(
    out: &mut Vec<SessionMeta>,
    config: &Config,
    cached: &crate::session_index::CachedSession,
    provider: Provider,
    path: Option<std::path::PathBuf>,
) {
    if cached.cwds.is_empty() {
        out.push(SessionMeta {
            id: cached.id.clone(),
            project_dir: "unknown".to_string(),
            is_child: cached.is_child,
            provider,
            session_file: path,
        });
        return;
    }
    for cwd_raw in &cached.cwds {
        let project_dir = if cwd_raw.is_empty() {
            "unknown".to_string()
        } else {
            config.simplify_path(cwd_raw)
        };
        out.push(SessionMeta {
            id: cached.id.clone(),
            project_dir,
            is_child: cached.is_child,
            provider,
            session_file: path.clone(),
        });
    }
}

pub fn build_full_session_index(config: &Config) -> SessionIndex {
    let filter = parse_identifier_filter(&[]);
    build_session_index(config, &filter)
}

fn ident_for(provider: Provider, activity_type: ActivityType, project_dir: &str) -> String {
    format!(
        "{}-{}@{}",
        provider.as_str(),
        activity_type.as_str(),
        project_dir
    )
}

pub fn list_identifiers_with_index(index: &SessionIndex) -> Vec<String> {
    let mut identifiers = Vec::new();

    for meta in index.non_child() {
        match meta.provider {
            Provider::ClaudeCode => {
                identifiers.push(ident_for(
                    Provider::ClaudeCode,
                    ActivityType::Message,
                    &meta.project_dir,
                ));
                identifiers.push(ident_for(
                    Provider::ClaudeCode,
                    ActivityType::Permission,
                    &meta.project_dir,
                ));
            }
            Provider::OpenCode => identifiers.push(ident_for(
                Provider::OpenCode,
                ActivityType::Message,
                &meta.project_dir,
            )),
            Provider::Pi => identifiers.push(ident_for(
                Provider::Pi,
                ActivityType::Message,
                &meta.project_dir,
            )),
        }
    }

    identifiers.sort();
    identifiers.dedup();
    identifiers
}

fn collect_all_activity_events(
    config: &Config,
    filter: &IdentifierFilter,
    index: &SessionIndex,
) -> Result<Vec<ActivityEvent>> {
    let t = std::time::Instant::now();
    let mut all_events = Vec::new();

    if filter.include_claude && filter.include_messages {
        // Multi-cwd sessions appear once per cwd in the index;
        // dedup by id so we parse the file only once.  Per-event
        // cwd in the JSONL drives ident assignment downstream.
        let mut parsed_ids: HashSet<String> = HashSet::new();
        for meta in index.non_child() {
            if meta.provider != Provider::ClaudeCode {
                continue;
            }
            if !parsed_ids.insert(meta.id.clone()) {
                continue;
            }
            let session_file = match &meta.session_file {
                Some(path) => path,
                None => continue,
            };
            // Pass `None` so each event's own ``cwd`` field decides
            // its ident — required for multi-cwd correctness.
            if let Ok(events) = parse_claudecode_messages(session_file, config, None) {
                all_events.extend(
                    events
                        .into_iter()
                        .filter(|event| filter.matches_ident(&event.ident)),
                );
            }
        }
    }

    if filter.include_claude && filter.include_permissions {
        let perm_dir_map: HashMap<&str, &str> = index
            .sessions
            .iter()
            .filter(|s| s.provider == Provider::ClaudeCode)
            .map(|s| (s.id.as_str(), s.project_dir.as_str()))
            .collect();

        let debug_dir = crate::claudecode::debug_dir();
        if debug_dir.exists() {
            for entry in fs::read_dir(&debug_dir)? {
                let entry = entry?;
                let path = entry.path();
                if path.extension().is_none_or(|ext| ext != "txt") {
                    continue;
                }
                let session_id = path.file_stem().map(|s| s.to_string_lossy().to_string());
                let session_file = session_id
                    .as_ref()
                    .and_then(|id| crate::claudecode::session::find_session_file(id));
                let perm_project_dir = session_id
                    .as_deref()
                    .and_then(|id| perm_dir_map.get(id).copied());

                if let Ok(events) = parse_claudecode_permissions(
                    &path,
                    session_file.as_deref(),
                    config,
                    perm_project_dir,
                ) {
                    all_events.extend(
                        events
                            .into_iter()
                            .filter(|event| filter.matches_ident(&event.ident)),
                    );
                }
            }
        }
    }

    if filter.include_pi && filter.include_messages {
        let mut parsed_ids: HashSet<String> = HashSet::new();
        for meta in index.non_child() {
            if meta.provider != Provider::Pi {
                continue;
            }
            if !parsed_ids.insert(meta.id.clone()) {
                continue;
            }
            let session_file = match &meta.session_file {
                Some(path) => path,
                None => continue,
            };
            if let Ok(events) = parse_pi_messages(session_file, config, Some(&meta.project_dir)) {
                all_events.extend(
                    events
                        .into_iter()
                        .filter(|event| filter.matches_ident(&event.ident)),
                );
            }
        }
    }

    if filter.include_opencode && filter.include_messages {
        let storage_dir = crate::opencode_data_dir().join("storage");
        let file_events =
            parse_opencode_messages_with_index(&storage_dir, config, index).unwrap_or_default();
        let db_events = if crate::opencode::db::db_exists() {
            crate::opencode::db::open_db()
                .ok()
                .and_then(|conn| parse_opencode_messages_from_db(&conn, config, index).ok())
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let merged_events = merge_activity_events(file_events, db_events);
        all_events.extend(
            merged_events
                .into_iter()
                .filter(|event| filter.matches_ident(&event.ident)),
        );
    }

    strip_preload_permissions(&mut all_events);
    all_events.sort_by_key(|event| event.timestamp);
    log::debug!(
        "collect_all_activity_events: {} events in {:?}",
        all_events.len(),
        t.elapsed()
    );
    Ok(all_events)
}

fn parse_exact_ident(ident: &str) -> Option<(Provider, ActivityType, &str)> {
    let (prefix, project_dir) = ident.split_once('@')?;
    let (provider_str, activity_str) = prefix.split_once('-')?;
    let provider = match provider_str {
        "claudecode" => Provider::ClaudeCode,
        "opencode" => Provider::OpenCode,
        "pi" => Provider::Pi,
        _ => return None,
    };
    let activity_type = match activity_str {
        "msg" => ActivityType::Message,
        "perm" => ActivityType::Permission,
        _ => return None,
    };
    Some((provider, activity_type, project_dir))
}

fn list_json_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return files;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "json") {
            files.push(path);
        }
    }
    files.sort();
    files
}

pub fn enumerate_files_for_ident_with_index(ident: &str, index: &SessionIndex) -> Vec<PathBuf> {
    let Some((provider, activity_type, project_dir)) = parse_exact_ident(ident) else {
        return Vec::new();
    };

    let mut files = Vec::new();

    match (provider, activity_type) {
        (Provider::ClaudeCode, ActivityType::Message) => {
            for meta in index.non_child() {
                if meta.provider == Provider::ClaudeCode && meta.project_dir == project_dir {
                    if let Some(path) = &meta.session_file {
                        files.push(path.clone());
                    }
                }
            }
        }
        (Provider::ClaudeCode, ActivityType::Permission) => {
            for meta in index.sessions.iter() {
                if meta.provider == Provider::ClaudeCode && meta.project_dir == project_dir {
                    let path = crate::claudecode::debug_dir().join(format!("{}.txt", meta.id));
                    if path.exists() {
                        files.push(path);
                    }
                }
            }
        }
        (Provider::OpenCode, ActivityType::Message) => {
            for meta in index.non_child() {
                if meta.provider == Provider::OpenCode && meta.project_dir == project_dir {
                    let dir = crate::opencode_data_dir()
                        .join("storage")
                        .join("message")
                        .join(&meta.id);
                    files.extend(list_json_files(&dir));
                }
            }
            let db_path = crate::opencode::db::db_path();
            if db_path.exists() {
                files.push(db_path);
            }
        }
        (Provider::Pi, ActivityType::Message) => {
            for meta in index.non_child() {
                if meta.provider == Provider::Pi && meta.project_dir == project_dir {
                    if let Some(path) = &meta.session_file {
                        files.push(path.clone());
                    }
                }
            }
        }
        _ => {}
    }

    files.sort();
    files.dedup();
    files
}

pub fn enumerate_files_for_ident(ident: &str, config: &Config) -> Vec<PathBuf> {
    let filter = parse_identifier_filter(&[ident.to_string()]);
    let index = build_session_index(config, &filter);
    enumerate_files_for_ident_with_index(ident, &index)
}

/// Fast-path file enumeration: bypass `SessionIndex` and hit the
/// per-harness session-index cache directly via its path-keyed map.
///
/// Used by the empty-segs hot path where we only need the files for
/// a single ident.  Cost is O(N_paths_in_harness × simplify_cost),
/// not O(N_total_sessions).  Returns paths in sorted order, deduped.
pub fn enumerate_files_for_ident_via_cache(ident: &str, config: &Config) -> Vec<PathBuf> {
    let Some((provider, activity_type, categ_id)) = parse_exact_ident(ident) else {
        return Vec::new();
    };
    let mut files = Vec::new();

    match (provider, activity_type) {
        (Provider::ClaudeCode, ActivityType::Message) => {
            if let Ok(idx) = crate::claudecode::session_index::update_and_load(config) {
                for s in idx.for_categ_id(categ_id, config) {
                    if s.is_child {
                        continue;
                    }
                    if let Some(p) = &s.path {
                        files.push(p.clone());
                    }
                }
            }
        }
        (Provider::ClaudeCode, ActivityType::Permission) => {
            if let Ok(idx) = crate::claudecode::session_index::update_and_load(config) {
                for s in idx.for_categ_id(categ_id, config) {
                    let p = crate::claudecode::debug_dir().join(format!("{}.txt", s.id));
                    if p.exists() {
                        files.push(p);
                    }
                }
            }
        }
        (Provider::OpenCode, ActivityType::Message) => {
            if let Ok(idx) = crate::opencode::session_index::update_and_load(config) {
                for s in idx.for_categ_id(categ_id, config) {
                    if s.is_child {
                        continue;
                    }
                    let dir = crate::opencode_data_dir()
                        .join("storage")
                        .join("message")
                        .join(&s.id);
                    files.extend(list_json_files(&dir));
                }
            }
            let db_path = crate::opencode::db::db_path();
            if db_path.exists() {
                files.push(db_path);
            }
        }
        (Provider::Pi, ActivityType::Message) => {
            if let Ok(idx) = crate::pi::session_index::update_and_load(config) {
                for s in idx.for_categ_id(categ_id, config) {
                    if s.is_child {
                        continue;
                    }
                    if let Some(p) = &s.path {
                        files.push(p.clone());
                    }
                }
            }
        }
        _ => {}
    }

    files.sort();
    files.dedup();
    files
}

pub fn fetch_all_event_timestamps_with_index(
    config: &Config,
    ident_filters: &[String],
    index: &SessionIndex,
) -> Result<HashMap<String, Vec<i64>>> {
    let t = std::time::Instant::now();
    let filter = parse_identifier_filter(ident_filters);
    let events = collect_all_activity_events(config, &filter, index)?;
    let mut by_ident: HashMap<String, Vec<i64>> = HashMap::new();
    for event in events {
        by_ident
            .entry(event.ident)
            .or_default()
            .push(event.timestamp);
    }
    let total: usize = by_ident.values().map(|v| v.len()).sum();
    log::debug!(
        "fetch_all_event_timestamps: {} idents, {} timestamps in {:?}",
        by_ident.len(),
        total,
        t.elapsed()
    );
    Ok(by_ident)
}

pub fn fetch_all_event_timestamps(
    config: &Config,
    ident_filters: &[String],
) -> Result<HashMap<String, Vec<i64>>> {
    let filter = parse_identifier_filter(ident_filters);
    let index = build_session_index(config, &filter);
    fetch_all_event_timestamps_with_index(config, ident_filters, &index)
}

/// List all available activity identifiers
pub fn list_identifiers(config: &Config) -> Result<Vec<String>> {
    let index = build_full_session_index(config);
    Ok(list_identifiers_with_index(&index))
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

/// Fetch all activity events within a time range, reusing a pre-built
/// session index.
///
/// Use this from CLI paths that also need the index for other work
/// (e.g. empty-segment emission) to avoid scanning all session metadata
/// twice in one invocation.
pub fn fetch_activities_with_index(
    config: &Config,
    start: DateTime<FixedOffset>,
    end: DateTime<FixedOffset>,
    identifiers: &[String],
    session_ids: &[String],
    index: &SessionIndex,
) -> Result<Vec<ActivityEvent>> {
    let t = std::time::Instant::now();
    let start_ts = start.timestamp();
    let end_ts = end.timestamp();
    let filter = parse_identifier_filter(identifiers);
    let mut all_events = collect_all_activity_events(config, &filter, index)?;

    let total_collected = all_events.len();
    all_events.retain(|event| event.timestamp >= start_ts && event.timestamp < end_ts);

    // Apply session ID filter
    if !session_ids.is_empty() {
        all_events.retain(|e| session_ids.contains(&e.session_id));
    }

    log::debug!(
        "fetch_activities: {} events (filtered from {}) in {:?}",
        all_events.len(),
        total_collected,
        t.elapsed()
    );
    Ok(all_events)
}

/// Fetch all activity events within a time range.
///
/// Convenience wrapper that builds its own session index — prefer
/// [`fetch_activities_with_index`] when you already have one.
pub fn fetch_activities(
    config: &Config,
    start: DateTime<FixedOffset>,
    end: DateTime<FixedOffset>,
    identifiers: &[String],
    session_ids: &[String],
) -> Result<Vec<ActivityEvent>> {
    let filter = parse_identifier_filter(identifiers);
    let index = build_session_index(config, &filter);
    fetch_activities_with_index(config, start, end, identifiers, session_ids, &index)
}

/// Remove permission events that precede the first user message in their
/// session.
///
/// When a session starts, saved/project-level permissions are loaded
/// automatically — these show up as a permission event before the user
/// has typed anything.  Such events are noise for activity tracking so
/// we drop them.  Any permission event that occurs *at or after* the
/// first message in the same session is kept.
fn strip_preload_permissions(events: &mut Vec<ActivityEvent>) {
    // Build a map: session_id → earliest message timestamp.
    let mut first_msg_ts: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    for e in events.iter() {
        if e.activity_type == ActivityType::Message {
            first_msg_ts
                .entry(e.session_id.clone())
                .and_modify(|ts| {
                    if e.timestamp < *ts {
                        *ts = e.timestamp;
                    }
                })
                .or_insert(e.timestamp);
        }
    }

    events.retain(|e| {
        if e.activity_type != ActivityType::Permission {
            return true;
        }
        match first_msg_ts.get(&e.session_id) {
            // Permission before first message → auto-loaded, drop it.
            Some(&msg_ts) => e.timestamp >= msg_ts,
            // No messages for this session → no reference point, keep it.
            None => true,
        }
    });
}

/// Filter for which activities to include
struct IdentifierFilter {
    include_claude: bool,
    include_opencode: bool,
    include_pi: bool,
    include_messages: bool,
    include_permissions: bool,
    /// Full identifier filters (empty = all)
    ident_filters: Vec<String>,
}

impl IdentifierFilter {
    fn matches_ident(&self, ident: &str) -> bool {
        if self.ident_filters.is_empty() {
            return true;
        }

        // Check for exact match against full identifiers
        self.ident_filters.iter().any(|f| f == ident)
    }
}

/// Parse identifier arguments into a filter
fn parse_identifier_filter(identifiers: &[String]) -> IdentifierFilter {
    if identifiers.is_empty() {
        return IdentifierFilter {
            include_claude: true,
            include_opencode: true,
            include_pi: true,
            include_messages: true,
            include_permissions: true,
            ident_filters: Vec::new(),
        };
    }

    let mut include_claude = false;
    let mut include_opencode = false;
    let mut include_pi = false;
    let mut include_messages = false;
    let mut include_permissions = false;
    let mut ident_filters = Vec::new();

    for ident in identifiers {
        // Store the full identifier for exact matching
        ident_filters.push(ident.clone());

        // Parse format: CLIENT-TYPE@PROJECT_PATH
        if let Some((prefix, _project)) = ident.split_once('@') {
            if prefix.starts_with("claudecode") {
                include_claude = true;
            }
            if prefix.starts_with("opencode") {
                include_opencode = true;
            }
            if prefix.starts_with("pi-") || prefix == "pi" {
                include_pi = true;
            }
            if prefix.ends_with("-msg") {
                include_messages = true;
            }
            if prefix.ends_with("-perm") {
                include_permissions = true;
            }
        } else {
            // Just a project path, include all types
            include_claude = true;
            include_opencode = true;
            include_pi = true;
            include_messages = true;
            include_permissions = true;
        }
    }

    // If no specific types selected, include all
    if !include_messages && !include_permissions {
        include_messages = true;
        include_permissions = true;
    }
    if !include_claude && !include_opencode && !include_pi {
        include_claude = true;
        include_opencode = true;
        include_pi = true;
    }

    IdentifierFilter {
        include_claude,
        include_opencode,
        include_pi,
        include_messages,
        include_permissions,
        ident_filters,
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
    use std::sync::Mutex;
    use tempfile::{tempdir, NamedTempFile};

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvGuard {
        home: Option<String>,
        xdg_cache_home: Option<String>,
        xdg_config_home: Option<String>,
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.home {
                Some(value) => unsafe {
                    std::env::set_var("HOME", value);
                },
                None => unsafe {
                    std::env::remove_var("HOME");
                },
            }
            match &self.xdg_cache_home {
                Some(value) => unsafe {
                    std::env::set_var("XDG_CACHE_HOME", value);
                },
                None => unsafe {
                    std::env::remove_var("XDG_CACHE_HOME");
                },
            }
            match &self.xdg_config_home {
                Some(value) => unsafe {
                    std::env::set_var("XDG_CONFIG_HOME", value);
                },
                None => unsafe {
                    std::env::remove_var("XDG_CONFIG_HOME");
                },
            }
        }
    }

    fn default_config() -> Config {
        Config::default()
    }

    fn with_temp_home(home: &Path) -> EnvGuard {
        let guard = EnvGuard {
            home: std::env::var("HOME").ok(),
            xdg_cache_home: std::env::var("XDG_CACHE_HOME").ok(),
            xdg_config_home: std::env::var("XDG_CONFIG_HOME").ok(),
        };
        unsafe {
            std::env::set_var("HOME", home);
            std::env::set_var("XDG_CACHE_HOME", home.join(".cache"));
            std::env::set_var("XDG_CONFIG_HOME", home.join(".config"));
        }
        guard
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
        assert_eq!(Provider::ClaudeCode.as_str(), "claudecode");
        assert_eq!(Provider::OpenCode.as_str(), "opencode");
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

        let events = parse_claudecode_messages(&file.path().to_path_buf(), &config, None).unwrap();

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

        let events = parse_claudecode_messages(&file.path().to_path_buf(), &config, None).unwrap();

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

        let events = parse_claudecode_messages(&file.path().to_path_buf(), &config, None).unwrap();

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

        let events = parse_claudecode_messages(&file.path().to_path_buf(), &config, None).unwrap();

        assert_eq!(events.len(), 0);
    }

    #[test]
    fn test_identifier_format() {
        let ident = format!(
            "{}-{}@{}",
            Provider::ClaudeCode.as_str(),
            ActivityType::Message.as_str(),
            "DEV>rs/project"
        );
        assert_eq!(ident, "claudecode-msg@DEV>rs/project");

        let ident = format!(
            "{}-{}@{}",
            Provider::OpenCode.as_str(),
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
            parse_claudecode_permissions(&file.path().to_path_buf(), None, &config, None).unwrap();

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
            parse_claudecode_permissions(&file.path().to_path_buf(), None, &config, None).unwrap();

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
            parse_claudecode_permissions(&file.path().to_path_buf(), None, &config, None).unwrap();

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
            None,
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

        let events = parse_claudecode_messages(&file.path().to_path_buf(), &config, None).unwrap();

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

        let events = parse_claudecode_messages(&file.path().to_path_buf(), &config, None).unwrap();

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

        let events = parse_claudecode_messages(&file.path().to_path_buf(), &config, None).unwrap();

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

        let events = parse_claudecode_messages(&session_file, &config, None).unwrap();

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

        let events = parse_claudecode_permissions(&debug_file, None, &config, None).unwrap();

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

    #[test]
    fn test_enumerate_files_for_ident_with_index_matches_standalone() {
        let _lock = ENV_LOCK.lock().unwrap();
        let temp = tempdir().unwrap();
        let _guard = with_temp_home(temp.path());

        let session_dir = temp.path().join(".claude/projects/fixture");
        fs::create_dir_all(&session_dir).unwrap();
        let session_path = session_dir.join("session-one.jsonl");
        fs::write(
            &session_path,
            r#"{"type":"user","timestamp":"1970-01-02T12:00:00Z","message":{"role":"user","content":"alpha"},"cwd":"/proj-one"}"#,
        )
        .unwrap();

        let config = default_config();
        let ident = "claudecode-msg@/proj-one";
        let expected = enumerate_files_for_ident(ident, &config);
        let index = build_full_session_index(&config);
        let actual = enumerate_files_for_ident_with_index(ident, &index);

        assert_eq!(actual, expected);
        assert_eq!(actual, vec![session_path]);
    }

    // =========================================================================
    // DB-based tests
    // =========================================================================

    fn setup_test_db() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::opencode::db::create_schema(&conn).unwrap();
        conn
    }

    fn insert_session_db(
        conn: &rusqlite::Connection,
        id: &str,
        parent_id: Option<&str>,
        directory: &str,
        time_created: i64,
    ) {
        conn.execute(
            "INSERT INTO session (id, project_id, parent_id, directory, title, time_created, time_updated) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![id, "proj_1", parent_id, directory, "", time_created, time_created],
        )
        .unwrap();
    }

    fn insert_message_db(
        conn: &rusqlite::Connection,
        id: &str,
        session_id: &str,
        role: &str,
        ts: i64,
    ) {
        let data = format!(r#"{{"role":"{}","time":{{"created":{}}}}}"#, role, ts);
        conn.execute(
            "INSERT INTO message (id, session_id, time_created, time_updated, data) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![id, session_id, ts, ts, data],
        )
        .unwrap();
    }

    fn insert_part_db(
        conn: &rusqlite::Connection,
        id: &str,
        msg_id: &str,
        session_id: &str,
        ts: i64,
        data: &str,
    ) {
        conn.execute(
            "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![id, msg_id, session_id, ts, ts, data],
        )
        .unwrap();
    }

    #[test]
    fn test_scan_opencode_sessions_to_meta_from_conn_basic() {
        let config = default_config();
        let conn = setup_test_db();

        insert_session_db(&conn, "ses_001", None, "/home/user/project", 1705314600000);
        insert_session_db(
            &conn,
            "ses_002",
            Some("ses_001"),
            "/home/user/project",
            1705314700000,
        );

        let metas = scan_opencode_sessions_to_meta_from_conn(&conn, &config);
        assert_eq!(metas.len(), 2);

        let main = metas.iter().find(|m| m.id == "ses_001").unwrap();
        assert!(!main.is_child);
        assert!(main.project_dir.contains("/home/user/project"));

        let child = metas.iter().find(|m| m.id == "ses_002").unwrap();
        assert!(child.is_child);
    }

    #[test]
    fn test_scan_opencode_sessions_to_meta_from_conn_empty() {
        let config = default_config();
        let conn = setup_test_db();

        let metas = scan_opencode_sessions_to_meta_from_conn(&conn, &config);
        assert!(metas.is_empty());
    }

    #[test]
    fn test_merge_session_metas_dedup_db_wins() {
        let file_metas = vec![SessionMeta {
            id: "ses_001".to_string(),
            project_dir: "/old/path".to_string(),
            is_child: false,
            provider: Provider::OpenCode,
            session_file: None,
        }];

        let db_metas = vec![SessionMeta {
            id: "ses_001".to_string(),
            project_dir: "/new/path".to_string(),
            is_child: false,
            provider: Provider::OpenCode,
            session_file: None,
        }];

        let merged = merge_session_metas(file_metas, db_metas);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].project_dir, "/new/path");
    }

    #[test]
    fn test_merge_session_metas_combines_unique() {
        let file_metas = vec![SessionMeta {
            id: "ses_file".to_string(),
            project_dir: "/proj".to_string(),
            is_child: false,
            provider: Provider::OpenCode,
            session_file: None,
        }];

        let db_metas = vec![SessionMeta {
            id: "ses_db".to_string(),
            project_dir: "/proj".to_string(),
            is_child: false,
            provider: Provider::OpenCode,
            session_file: None,
        }];

        let merged = merge_session_metas(file_metas, db_metas);
        assert_eq!(merged.len(), 2);
    }

    #[test]
    fn test_get_opencode_message_content_from_db() {
        let conn = setup_test_db();
        insert_part_db(
            &conn,
            "prt_001",
            "msg_001",
            "ses_001",
            1705314600000,
            r#"{"type":"text","text":"Hello from DB"}"#,
        );
        insert_part_db(
            &conn,
            "prt_002",
            "msg_001",
            "ses_001",
            1705314600100,
            r#"{"type":"text","text":"More text"}"#,
        );

        let content = get_opencode_message_content_from_db(&conn, "msg_001").unwrap();
        assert!(content.contains("Hello from DB"));
        assert!(content.contains("More text"));
    }

    #[test]
    fn test_get_opencode_message_content_from_db_skips_tool() {
        let conn = setup_test_db();
        insert_part_db(
            &conn,
            "prt_001",
            "msg_001",
            "ses_001",
            1705314600000,
            r#"{"type":"tool","tool":"bash","state":{"input":{"command":"ls"}}}"#,
        );

        let content = get_opencode_message_content_from_db(&conn, "msg_001").unwrap();
        assert!(content.is_empty());
    }

    #[test]
    fn test_parse_opencode_messages_from_db_basic() {
        let config = default_config();
        let conn = setup_test_db();

        insert_session_db(&conn, "ses_db1", None, "/home/user/project", 1705314600000);
        insert_message_db(&conn, "msg_001", "ses_db1", "user", 1705314600000);
        insert_part_db(
            &conn,
            "prt_001",
            "msg_001",
            "ses_db1",
            1705314600000,
            r#"{"type":"text","text":"Hello from DB"}"#,
        );

        let metas = scan_opencode_sessions_to_meta_from_conn(&conn, &config);
        let index = SessionIndex { sessions: metas };

        let events = parse_opencode_messages_from_db(&conn, &config, &index).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].activity_type, ActivityType::Message);
        assert!(events[0].ident.starts_with("opencode-msg@"));
        if let ActivityData::Message { content } = &events[0].data {
            assert_eq!(content, "Hello from DB");
        } else {
            panic!("Expected Message data");
        }
    }

    #[test]
    fn test_parse_opencode_messages_from_db_skips_assistant() {
        let config = default_config();
        let conn = setup_test_db();

        insert_session_db(&conn, "ses_db2", None, "/project", 1705314600000);
        insert_message_db(&conn, "msg_001", "ses_db2", "assistant", 1705314600000);
        insert_part_db(
            &conn,
            "prt_001",
            "msg_001",
            "ses_db2",
            1705314600000,
            r#"{"type":"text","text":"Assistant response"}"#,
        );

        let metas = scan_opencode_sessions_to_meta_from_conn(&conn, &config);
        let index = SessionIndex { sessions: metas };

        let events = parse_opencode_messages_from_db(&conn, &config, &index).unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn test_parse_opencode_messages_from_db_skips_children() {
        let config = default_config();
        let conn = setup_test_db();

        insert_session_db(&conn, "ses_parent", None, "/project", 1705314600000);
        insert_session_db(
            &conn,
            "ses_child",
            Some("ses_parent"),
            "/project",
            1705314700000,
        );
        insert_message_db(&conn, "msg_001", "ses_child", "user", 1705314700000);
        insert_part_db(
            &conn,
            "prt_001",
            "msg_001",
            "ses_child",
            1705314700000,
            r#"{"type":"text","text":"Child message"}"#,
        );

        let metas = scan_opencode_sessions_to_meta_from_conn(&conn, &config);
        let index = SessionIndex { sessions: metas };

        let events = parse_opencode_messages_from_db(&conn, &config, &index).unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn test_merge_activity_events_dedup_db_wins() {
        let file_events = vec![
            ActivityEvent {
                timestamp: 100,
                ident: "opencode-msg@/proj".to_string(),
                session_id: "ses_001".to_string(),
                activity_type: ActivityType::Message,
                data: ActivityData::Message {
                    content: "file version".to_string(),
                },
            },
            ActivityEvent {
                timestamp: 200,
                ident: "opencode-msg@/proj".to_string(),
                session_id: "ses_001".to_string(),
                activity_type: ActivityType::Message,
                data: ActivityData::Message {
                    content: "file only".to_string(),
                },
            },
        ];

        let db_events = vec![ActivityEvent {
            timestamp: 100,
            ident: "opencode-msg@/proj".to_string(),
            session_id: "ses_001".to_string(),
            activity_type: ActivityType::Message,
            data: ActivityData::Message {
                content: "db version".to_string(),
            },
        }];

        let merged = merge_activity_events(file_events, db_events);
        assert_eq!(merged.len(), 2);

        // The event at ts=100 should be the DB version
        let ts100 = merged.iter().find(|e| e.timestamp == 100).unwrap();
        if let ActivityData::Message { content } = &ts100.data {
            assert_eq!(content, "db version");
        } else {
            panic!("Expected Message data");
        }

        // The event at ts=200 should be the file version (unique)
        let ts200 = merged.iter().find(|e| e.timestamp == 200).unwrap();
        if let ActivityData::Message { content } = &ts200.data {
            assert_eq!(content, "file only");
        } else {
            panic!("Expected Message data");
        }
    }

    #[test]
    fn test_merge_activity_events_empty_sources() {
        let merged = merge_activity_events(Vec::new(), Vec::new());
        assert!(merged.is_empty());

        let events = vec![ActivityEvent {
            timestamp: 100,
            ident: "test".to_string(),
            session_id: "ses_001".to_string(),
            activity_type: ActivityType::Message,
            data: ActivityData::Message {
                content: "msg".to_string(),
            },
        }];

        let merged = merge_activity_events(events.clone(), Vec::new());
        assert_eq!(merged.len(), 1);

        let merged = merge_activity_events(Vec::new(), events);
        assert_eq!(merged.len(), 1);
    }
}

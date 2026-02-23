//! Auto-detect the current AI session that spawned this process.
//!
//! Detection strategy (in priority order):
//! 1. Check env vars for authoritative session ID (future-proof)
//! 2. Parse ancestor process cmdline for `-s`/`--session` flag
//! 3. Detect provider from env + process tree
//! 4. Gather candidate sessions for the current working directory
//! 5. Filter to non-child, recently updated sessions
//! 6. If exactly one candidate, return it
//! 7. Try tmux pane content matching to disambiguate
//!
//! If exactly one candidate matches, return it. Otherwise fail
//! with an actionable error listing candidates.

use anyhow::{bail, Context, Result};
use std::env;
use std::fs;
use std::path::Path;

pub use crate::provider::Provider;

/// Result of session auto-detection.
#[derive(Debug, Clone)]
pub struct DetectedSession {
    pub session_id: String,
    pub provider: Provider,
}

/// Candidate session with metadata for scoring.
#[derive(Debug, Clone)]
struct Candidate {
    session_id: String,
    provider: Provider,
    updated_ms: i64,
}

/// Options for last-session detection.
pub struct LastSessionOptions {
    /// Optional provider filter.
    pub provider_filter: Option<Provider>,
    /// Optional project directory override.
    pub project_dir: Option<String>,
}

/// Options for match-based session detection.
pub struct MatchOptions {
    /// Text to search for in recent messages.
    pub needle: String,
    /// Number of recent messages to search.
    pub last_messages: usize,
    /// Optional provider filter.
    pub provider_filter: Option<Provider>,
    /// Optional project directory filter.
    pub project_dir: Option<String>,
}

/// Find a session by matching text in its last N messages.
///
/// Gathers candidate sessions (optionally filtered by provider and project),
/// then searches each one's recent messages for the needle.
/// Returns the matching session, or an error if zero or multiple match.
pub fn find_session_by_match(opts: &MatchOptions) -> Result<DetectedSession> {
    // Resolve project dir to absolute path
    let project_path: Option<String> = match &opts.project_dir {
        Some(p) => {
            let path = std::path::PathBuf::from(p);
            let abs = if path.is_absolute() {
                path
            } else {
                std::env::current_dir().unwrap_or_default().join(path)
            };
            let resolved = abs.canonicalize().unwrap_or(abs);
            Some(resolved.to_string_lossy().to_string())
        }
        None => None,
    };

    let mut matches: Vec<DetectedSession> = Vec::new();

    let include_opencode =
        opts.provider_filter.is_none() || opts.provider_filter == Some(Provider::OpenCode);
    let include_claudecode =
        opts.provider_filter.is_none() || opts.provider_filter == Some(Provider::ClaudeCode);

    if include_opencode {
        if let Ok(sessions) = crate::opencode::list_sessions() {
            for s in sessions {
                if let Some(ref expected) = project_path {
                    if s.project_dir != *expected {
                        continue;
                    }
                }
                if crate::opencode::session_tail_contains_text(
                    &s.session_id,
                    &opts.needle,
                    opts.last_messages,
                ) {
                    matches.push(DetectedSession {
                        session_id: s.session_id,
                        provider: Provider::OpenCode,
                    });
                }
            }
        }
    }

    if include_claudecode {
        if let Ok(sessions) = crate::claudecode::session::list_sessions() {
            for s in sessions {
                if let Some(ref expected) = project_path {
                    if s.project_dir != *expected {
                        continue;
                    }
                }
                if crate::claudecode::session::session_tail_contains_text(
                    &s.session_id,
                    &opts.needle,
                    opts.last_messages,
                ) {
                    matches.push(DetectedSession {
                        session_id: s.session_id,
                        provider: Provider::ClaudeCode,
                    });
                }
            }
        }
    }

    match matches.len() {
        0 => bail!(
            "No session found matching \"{}\" in last {} messages",
            opts.needle,
            opts.last_messages
        ),
        1 => Ok(matches.into_iter().next().unwrap()),
        n => {
            let mut msg = format!(
                "Ambiguous: {} sessions match \"{}\". Use --type or --project to narrow:\n",
                n, opts.needle
            );
            for m in &matches {
                msg.push_str(&format!("  {} ({:?})\n", m.session_id, m.provider));
            }
            bail!("{}", msg.trim_end());
        }
    }
}

/// Extract session ID from a NUL-separated cmdline string.
///
/// Looks for `-s <session_id>` or `--session <session_id>` in the
/// argument list.
fn parse_session_from_cmdline_args(raw: &str) -> Option<String> {
    let args: Vec<&str> = raw.split('\0').collect();
    for window in args.windows(2) {
        if (window[0] == "-s" || window[0] == "--session") && !window[1].is_empty() {
            return Some(window[1].to_string());
        }
    }
    None
}

/// Walk the process tree upward (starting from the given PID itself)
/// looking for an opencode/claude process whose cmdline contains a
/// `-s`/`--session` flag with the session ID.
#[cfg(unix)]
fn find_session_from_ancestor_cmdline(start_pid: u32) -> Option<DetectedSession> {
    // Check the start PID itself first — it may be the opencode process
    if let Some(detected) = check_pid_cmdline_for_session(start_pid) {
        return Some(detected);
    }

    let mut pid = start_pid;

    for _ in 0..20 {
        let ppid = get_parent_pid(pid)?;
        if ppid <= 1 {
            break;
        }

        if let Some(detected) = check_pid_cmdline_for_session(ppid) {
            return Some(detected);
        }

        pid = ppid;
    }

    None
}

/// Check if a single PID is an opencode/claude process with `-s`/`--session`
/// in its cmdline.
#[cfg(unix)]
fn check_pid_cmdline_for_session(pid: u32) -> Option<DetectedSession> {
    let name = get_process_name(pid)?;
    let provider = match name.as_str() {
        "opencode" => Provider::OpenCode,
        "claude" => Provider::ClaudeCode,
        _ => return None,
    };
    let raw = fs::read_to_string(format!("/proc/{}/cmdline", pid)).ok()?;
    let session_id = parse_session_from_cmdline_args(&raw)?;
    Some(DetectedSession {
        session_id,
        provider,
    })
}

/// Try to auto-detect the current session.
///
/// Returns the session ID if exactly one candidate is found,
/// or an error with candidate list if ambiguous/none.
pub fn detect_current_session() -> Result<DetectedSession> {
    // Step 1: Check authoritative env vars (future-proof)
    if let Ok(sid) = env::var("OPENCODE_SESSION_ID") {
        if !sid.is_empty() {
            return Ok(DetectedSession {
                session_id: sid,
                provider: Provider::OpenCode,
            });
        }
    }
    if let Ok(sid) = env::var("CLAUDE_SESSION_ID") {
        if !sid.is_empty() {
            return Ok(DetectedSession {
                session_id: sid,
                provider: Provider::ClaudeCode,
            });
        }
    }

    // Step 2: Check ancestor process cmdline for session ID
    // (e.g. `opencode -s ses_xxx` in the parent's cmdline)
    #[cfg(unix)]
    if let Some(detected) = find_session_from_ancestor_cmdline(std::process::id()) {
        return Ok(detected);
    }

    // Step 3: Detect provider
    let provider = detect_provider()?;

    // Step 4: Get current working directory
    let cwd = env::current_dir().context("Failed to get current directory")?;

    // Step 5: Gather candidates
    let candidates = match provider {
        Provider::OpenCode => gather_opencode_candidates(&cwd)?,
        Provider::ClaudeCode => gather_claudecode_candidates(&cwd)?,
    };

    if candidates.is_empty() {
        bail!("No sessions found for directory: {}", cwd.display());
    }

    // Step 6: If only one candidate, return it directly
    if candidates.len() == 1 {
        return Ok(DetectedSession {
            session_id: candidates[0].session_id.clone(),
            provider,
        });
    }

    // Step 7: Multiple candidates — try tmux pane content matching
    #[cfg(unix)]
    {
        if is_tmux_available() {
            if let Some(detected) = match_by_tmux_pane_content(&candidates, provider) {
                return Ok(detected);
            }
        }
    }

    // All strategies exhausted — report candidates
    let mut msg = String::from(
        "Multiple sessions found for this directory. \
         Could not disambiguate",
    );
    #[cfg(unix)]
    {
        if is_tmux_available() {
            msg.push_str(" via tmux pane content");
        } else {
            msg.push_str(". Tmux not detected for pane matching");
        }
    }
    #[cfg(not(unix))]
    {
        msg.push_str(". Tmux not available on this platform");
    }
    msg.push_str(". Use --session <id> to specify:");
    for c in &candidates {
        msg.push_str(&format!("\n  {}", c.session_id));
    }
    bail!("{}", msg);
}

/// Detect the last AI session used in the current tmux pane or directory.
///
/// Detection strategy:
/// 1. If in tmux:
///    a. Check if current pane's process is an AI tool → extract session from cmdline
///    b. Capture pane content → extract session/project from opencode launch commands
///    c. Gather candidates for the detected project directory
///    d. If multiple, try snippet matching against pane content
///    e. Return best match or most recent by timestamp
/// 2. If not in tmux:
///    a. Use CWD (or provided --project dir) as project directory
///    b. Return the most recent session for that directory
pub fn detect_last_session(opts: &LastSessionOptions) -> Result<DetectedSession> {
    let project_override = opts.project_dir.as_ref().map(|p| resolve_to_absolute(p));

    #[cfg(unix)]
    if is_tmux_available() {
        if let Some(result) =
            detect_last_session_via_tmux(opts.provider_filter, project_override.as_deref())
        {
            return Ok(result);
        }
    }

    // Fallback: CWD + most recent session
    let dir = match project_override {
        Some(d) => std::path::PathBuf::from(d),
        None => env::current_dir().context("Failed to get current directory")?,
    };
    find_most_recent_session(&dir, opts.provider_filter)
}

/// Detect which AI provider spawned this process.
fn detect_provider() -> Result<Provider> {
    // Check env vars first (fast path)
    if env::var("OPENCODE").as_deref() == Ok("1") {
        return Ok(Provider::OpenCode);
    }

    // Walk process tree to find parent
    if let Some(provider) = detect_provider_from_process_tree() {
        return Ok(provider);
    }

    bail!(
        "Not running inside a known AI session.\n\
         Expected OPENCODE=1 env var or an 'opencode'/'claude' parent process."
    );
}

/// Walk the process tree upward looking for opencode/claude parent.
#[cfg(unix)]
fn detect_provider_from_process_tree() -> Option<Provider> {
    let mut pid = std::process::id();

    // Walk up to 20 levels (safety limit)
    for _ in 0..20 {
        let ppid = get_parent_pid(pid)?;
        if ppid <= 1 {
            break;
        }

        let name = get_process_name(ppid)?;
        match name.as_str() {
            "opencode" => return Some(Provider::OpenCode),
            "claude" => return Some(Provider::ClaudeCode),
            _ => {}
        }

        pid = ppid;
    }

    None
}

#[cfg(not(unix))]
fn detect_provider_from_process_tree() -> Option<Provider> {
    None
}

/// Get parent PID from /proc/<pid>/status.
#[cfg(unix)]
fn get_parent_pid(pid: u32) -> Option<u32> {
    let status = fs::read_to_string(format!("/proc/{}/status", pid)).ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("PPid:") {
            return rest.trim().parse().ok();
        }
    }
    None
}

/// Get process name from /proc/<pid>/comm.
#[cfg(unix)]
fn get_process_name(pid: u32) -> Option<String> {
    fs::read_to_string(format!("/proc/{}/comm", pid))
        .ok()
        .map(|s| s.trim().to_string())
}

/// Gather OpenCode candidate sessions for the given directory.
///
/// Reads `storage/session/<hash>/ses_*.json` and queries SQLite,
/// filters to:
/// - Non-child sessions (no `parentID`)
/// - Matching directory
/// - Sorted by `time.updated` descending
///
/// Deduplicates by session_id; DB wins on conflict.
fn gather_opencode_candidates(cwd: &Path) -> Result<Vec<Candidate>> {
    let file_candidates = gather_opencode_candidates_from_files(cwd);
    let db_candidates = gather_opencode_candidates_from_db(cwd);
    Ok(merge_candidates(file_candidates, db_candidates))
}

/// Gather candidates from file-based session storage.
fn gather_opencode_candidates_from_files(cwd: &Path) -> Vec<Candidate> {
    let session_base = crate::opencode_data_dir().join("storage/session");
    if !session_base.exists() {
        return Vec::new();
    }

    let cwd_str = cwd.to_string_lossy();
    let mut candidates = Vec::new();

    let project_entries = match fs::read_dir(&session_base) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };

    for project_entry in project_entries {
        let project_entry = match project_entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if !project_entry.path().is_dir() {
            continue;
        }
        let file_entries = match fs::read_dir(project_entry.path()) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for file_entry in file_entries {
            let file_entry = match file_entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            let path = file_entry.path();
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }
            if let Ok(content) = fs::read_to_string(&path) {
                if let Ok(session) = serde_json::from_str::<serde_json::Value>(&content) {
                    // Skip child sessions
                    if session.get("parentID").and_then(|v| v.as_str()).is_some() {
                        continue;
                    }
                    // Match directory
                    let dir = session
                        .get("directory")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if dir != cwd_str.as_ref() {
                        continue;
                    }
                    let id = session
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let updated = session
                        .get("time")
                        .and_then(|t| t.get("updated"))
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0);

                    if !id.is_empty() {
                        candidates.push(Candidate {
                            session_id: id,
                            provider: Provider::OpenCode,
                            updated_ms: updated,
                        });
                    }
                }
            }
        }
    }

    candidates
}

/// Gather candidates from the SQLite database.
fn gather_opencode_candidates_from_db(cwd: &Path) -> Vec<Candidate> {
    if !crate::opencode::db::db_exists() {
        return Vec::new();
    }
    let conn = match crate::opencode::db::open_db() {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    gather_opencode_candidates_from_conn(&conn, cwd)
}

/// Gather candidates from a DB connection (testable).
fn gather_opencode_candidates_from_conn(conn: &rusqlite::Connection, cwd: &Path) -> Vec<Candidate> {
    let cwd_str = cwd.to_string_lossy();
    let mut stmt = match conn.prepare(
        "SELECT id, parent_id, directory, time_updated \
         FROM session WHERE directory = ? AND parent_id IS NULL \
         ORDER BY time_updated DESC",
    ) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };

    let rows = match stmt.query_map([cwd_str.as_ref()], |row| {
        let id: String = row.get(0)?;
        let _parent_id: Option<String> = row.get(1)?;
        let _directory: String = row.get(2)?;
        let time_updated: i64 = row.get(3)?;
        Ok((id, time_updated))
    }) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };

    let mut candidates = Vec::new();
    for row in rows {
        if let Ok((id, time_updated)) = row {
            if !id.is_empty() {
                candidates.push(Candidate {
                    session_id: id,
                    provider: Provider::OpenCode,
                    updated_ms: time_updated,
                });
            }
        }
    }

    candidates
}

/// Merge file-based and DB-based candidates, deduplicating by session_id.
/// DB wins on conflict.
fn merge_candidates(
    file_candidates: Vec<Candidate>,
    db_candidates: Vec<Candidate>,
) -> Vec<Candidate> {
    use std::collections::HashMap;

    let mut by_id: HashMap<String, Candidate> = HashMap::new();

    // Insert file-based first
    for c in file_candidates {
        by_id.insert(c.session_id.clone(), c);
    }
    // DB overwrites on conflict
    for c in db_candidates {
        by_id.insert(c.session_id.clone(), c);
    }

    let mut merged: Vec<Candidate> = by_id.into_values().collect();
    merged.sort_by(|a, b| b.updated_ms.cmp(&a.updated_ms));
    merged
}

/// Gather Claude Code candidate sessions for the given directory.
///
/// Finds the encoded project directory in `~/.claude/projects/`,
/// lists JSONL files, filters to recent, sorted by mtime.
fn gather_claudecode_candidates(cwd: &Path) -> Result<Vec<Candidate>> {
    let projects_dir = crate::claudecode::projects_dir();
    if !projects_dir.exists() {
        return Ok(Vec::new());
    }

    let cwd_str = cwd.to_string_lossy();

    // Find project dir(s) matching cwd
    let mut candidates = Vec::new();

    for entry in fs::read_dir(&projects_dir)? {
        let entry = entry?;
        let project_path = entry.path();
        if !project_path.is_dir() {
            continue;
        }

        // Check if this project dir decodes to our cwd
        let dir_name = project_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();
        let decoded = crate::claudecode::session::decode_project_dir_name_pub(&dir_name);
        if decoded != cwd_str.as_ref() {
            continue;
        }

        // List JSONL session files
        for file_entry in fs::read_dir(&project_path)? {
            let file_entry = file_entry?;
            let path = file_entry.path();
            if path.extension().is_some_and(|e| e == "jsonl") {
                if let Some(stem) = path.file_stem() {
                    let session_id = stem.to_string_lossy().to_string();
                    // Use file mtime as proxy for last update
                    let updated_ms = path
                        .metadata()
                        .ok()
                        .and_then(|m| m.modified().ok())
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_millis() as i64)
                        .unwrap_or(0);

                    candidates.push(Candidate {
                        session_id,
                        provider: Provider::ClaudeCode,
                        updated_ms,
                    });
                }
            }
        }
    }

    // Sort by most recently updated first
    candidates.sort_by(|a, b| b.updated_ms.cmp(&a.updated_ms));
    Ok(candidates)
}

// === Tmux pane content matching (Step 7) ===

/// Check if we're running inside a tmux session.
#[cfg(unix)]
fn is_tmux_available() -> bool {
    env::var("TMUX").is_ok_and(|v| !v.is_empty())
}

/// List tmux panes running an opencode TUI (alternate screen active).
///
/// Returns `(pane_id, pane_pid)` pairs for panes where
/// `alternate_on=1` and `pane_current_command=opencode`.
#[cfg(unix)]
fn list_opencode_tui_panes() -> Vec<(String, u32)> {
    let output = match std::process::Command::new("tmux")
        .args([
            "list-panes",
            "-a",
            "-F",
            "#{pane_id} #{pane_pid} #{alternate_on} #{pane_current_command}",
        ])
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut result = Vec::new();

    for line in stdout.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        // Expected: %<id> <pid> <alternate_on> <command>
        if parts.len() < 4 {
            continue;
        }
        let pane_id = parts[0];
        let alternate_on = parts[2];
        let command = parts[3];

        if alternate_on != "1" || command != "opencode" {
            continue;
        }

        if let Ok(pid) = parts[1].parse::<u32>() {
            result.push((pane_id.to_string(), pid));
        }
    }

    result
}

/// Capture the alternate screen content of a tmux pane.
#[cfg(unix)]
fn capture_pane_content(pane_id: &str) -> Option<String> {
    let output = std::process::Command::new("tmux")
        .args(["capture-pane", "-a", "-p", "-t", pane_id])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    Some(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Safely truncate a string to at most `max_chars` characters,
/// respecting UTF-8 char boundaries.
fn safe_truncate(s: &str, max_chars: usize) -> &str {
    if s.chars().count() <= max_chars {
        return s;
    }
    let end = s
        .char_indices()
        .nth(max_chars)
        .map(|(i, _)| i)
        .unwrap_or(s.len());
    &s[..end]
}

/// Extract short user-message snippets from an OpenCode session.
///
/// Reads the last `max_messages` user messages, returning the first 60
/// chars of each (minimum 12 chars to be useful for matching).
///
/// Checks both file-based storage and SQLite database, combining results.
fn get_opencode_user_snippets(session_id: &str, max_messages: usize) -> Vec<String> {
    let storage_dir = crate::opencode_data_dir().join("storage");
    let mut snippets = get_opencode_user_snippets_in(&storage_dir, session_id, max_messages);

    // Also gather from DB and combine (snippets are used for matching,
    // no dedup needed — more snippets = better matching)
    if crate::opencode::db::db_exists() {
        if let Ok(conn) = crate::opencode::db::open_db() {
            let db_snippets = get_opencode_user_snippets_from_db(&conn, session_id, max_messages);
            for s in db_snippets {
                if !snippets.contains(&s) {
                    snippets.push(s);
                }
            }
        }
    }

    snippets
}

/// Testable variant that accepts a storage directory override.
fn get_opencode_user_snippets_in(
    storage_dir: &Path,
    session_id: &str,
    max_messages: usize,
) -> Vec<String> {
    let message_dir = storage_dir.join("message").join(session_id);
    let part_dir = storage_dir.join("part");

    if !message_dir.exists() {
        return Vec::new();
    }

    // List message files, sorted by name (chronological)
    let mut msg_files: Vec<_> = fs::read_dir(&message_dir)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "json"))
        .collect();
    msg_files.sort_by_key(|e| e.file_name());

    let mut snippets = Vec::new();

    // Walk from newest to oldest, collecting user messages
    for msg_entry in msg_files.iter().rev() {
        if snippets.len() >= max_messages {
            break;
        }

        let raw = match fs::read_to_string(msg_entry.path()) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let msg: serde_json::Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // Only user messages
        if msg.get("role").and_then(|v| v.as_str()) != Some("user") {
            continue;
        }

        let msg_id = msg_entry
            .path()
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();

        // Read parts for this message to extract text
        let msg_part_dir = part_dir.join(&msg_id);
        if !msg_part_dir.exists() {
            continue;
        }

        let part_files: Vec<_> = fs::read_dir(&msg_part_dir)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "json"))
            .collect();

        for part_entry in &part_files {
            let part_raw = match fs::read_to_string(part_entry.path()) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let part: serde_json::Value = match serde_json::from_str(&part_raw) {
                Ok(v) => v,
                Err(_) => continue,
            };

            if part.get("type").and_then(|v| v.as_str()) != Some("text") {
                continue;
            }

            if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                let trimmed = text.trim();
                if trimmed.len() >= 12 {
                    let snippet = safe_truncate(trimmed, 60).to_string();
                    snippets.push(snippet);
                    break; // one snippet per message
                }
            }
        }
    }

    snippets
}

/// Extract user-message snippets from the SQLite database.
fn get_opencode_user_snippets_from_db(
    conn: &rusqlite::Connection,
    session_id: &str,
    max_messages: usize,
) -> Vec<String> {
    // Get the last N user messages
    let messages = match crate::opencode::db::get_messages_for_session(conn, session_id) {
        Ok(m) => m,
        Err(_) => return Vec::new(),
    };

    let mut snippets = Vec::new();

    // Walk from newest to oldest, collecting user messages
    for (msg_id, role, _time_created) in messages.iter().rev() {
        if snippets.len() >= max_messages {
            break;
        }

        if role != "user" {
            continue;
        }

        let parts = match crate::opencode::db::get_parts_for_message(conn, msg_id) {
            Ok(p) => p,
            Err(_) => continue,
        };

        for part in &parts {
            if part.get("type").and_then(|v| v.as_str()) != Some("text") {
                continue;
            }

            if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                let trimmed = text.trim();
                if trimmed.len() >= 12 {
                    let snippet = safe_truncate(trimmed, 60).to_string();
                    snippets.push(snippet);
                    break; // one snippet per message
                }
            }
        }
    }

    snippets
}

/// Extract short user-message snippets from a Claude Code session.
///
/// Reads the JSONL file, finds the last `max_messages` user (human)
/// messages, returns the first 60 chars of each (minimum 12 chars).
fn get_claudecode_user_snippets(session_id: &str, max_messages: usize) -> Vec<String> {
    let session_file = match crate::claudecode::session::find_session_file(session_id) {
        Some(f) => f,
        None => return Vec::new(),
    };
    get_claudecode_user_snippets_in(&session_file, max_messages)
}

/// Testable variant that accepts a session file path directly.
fn get_claudecode_user_snippets_in(session_file: &Path, max_messages: usize) -> Vec<String> {
    let content = match fs::read_to_string(session_file) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    // Collect all human message texts, then take the last N
    let mut all_texts: Vec<String> = Vec::new();

    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }

        let entry: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        if entry.get("type").and_then(|v| v.as_str()) != Some("human") {
            continue;
        }

        // Content can be a string or an array of blocks
        let message = match entry.get("message") {
            Some(m) => m,
            None => continue,
        };

        let content_val = match message.get("content") {
            Some(c) => c,
            None => continue,
        };

        let text = match content_val {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Array(blocks) => {
                // Find first text block
                let mut found = String::new();
                for block in blocks {
                    if block.get("type").and_then(|v| v.as_str()) == Some("text") {
                        if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                            found = t.to_string();
                            break;
                        }
                    }
                }
                found
            }
            _ => continue,
        };

        let trimmed = text.trim();
        if trimmed.len() >= 12 {
            let snippet = safe_truncate(trimmed, 60).to_string();
            all_texts.push(snippet);
        }
    }

    // Return the last max_messages snippets
    let start = all_texts.len().saturating_sub(max_messages);
    all_texts[start..].to_vec()
}

/// Testable inner function: match candidate snippets against pane contents.
///
/// For each candidate, counts how many of its snippets appear in any pane.
/// Returns the candidate with the most hits (minimum 1 hit). On tie,
/// returns the first candidate (callers pass candidates sorted by recency).
fn match_snippets_against_panes(
    candidate_snippets: &[(String, Vec<String>)], // (session_id, snippets)
    provider: Provider,
    pane_contents: &[String],
) -> Option<DetectedSession> {
    if candidate_snippets.is_empty() || pane_contents.is_empty() {
        return None;
    }

    // For each candidate, find the max hit count across all panes
    let mut scores: Vec<(usize, &str)> = Vec::new(); // (max_hits, session_id)

    for (session_id, snippets) in candidate_snippets {
        if snippets.is_empty() {
            scores.push((0, session_id));
            continue;
        }

        let mut max_hits = 0usize;
        for pane_content in pane_contents {
            let hits = snippets
                .iter()
                .filter(|snippet| pane_content.contains(snippet.as_str()))
                .count();
            if hits > max_hits {
                max_hits = hits;
            }
        }
        scores.push((max_hits, session_id));
    }

    // Find the winner: must have at least 1 hit and strictly more than any other
    scores.sort_by(|a, b| b.0.cmp(&a.0));

    let best = scores[0];
    if best.0 == 0 {
        return None;
    }

    // On tie, prefer the first candidate (callers pass candidates sorted
    // by recency, so the first entry is the most recent session).
    Some(DetectedSession {
        session_id: best.1.to_string(),
        provider,
    })
}

/// Disambiguate candidates by matching their user messages against
/// tmux pane content.
#[cfg(unix)]
fn match_by_tmux_pane_content(
    candidates: &[Candidate],
    provider: Provider,
) -> Option<DetectedSession> {
    let tui_panes = list_opencode_tui_panes();
    if tui_panes.is_empty() {
        return None;
    }

    // Capture content from each TUI pane
    let pane_contents: Vec<String> = tui_panes
        .iter()
        .filter_map(|(pane_id, _pid)| capture_pane_content(pane_id))
        .collect();

    if pane_contents.is_empty() {
        return None;
    }

    // Gather snippets for each candidate
    let candidate_snippets: Vec<(String, Vec<String>)> = candidates
        .iter()
        .map(|c| {
            let snippets = match provider {
                Provider::OpenCode => get_opencode_user_snippets(&c.session_id, 5),
                Provider::ClaudeCode => get_claudecode_user_snippets(&c.session_id, 5),
            };
            (c.session_id.clone(), snippets)
        })
        .collect();

    match_snippets_against_panes(&candidate_snippets, provider, &pane_contents)
}

// === Last-session detection helpers ===

/// Resolve a path string to an absolute path.
fn resolve_to_absolute(path: &str) -> String {
    let p = std::path::PathBuf::from(path);
    let abs = if p.is_absolute() {
        p
    } else {
        env::current_dir().unwrap_or_default().join(p)
    };
    abs.canonicalize()
        .unwrap_or(abs)
        .to_string_lossy()
        .to_string()
}

/// Detect the last session by reading the current tmux pane's content.
#[cfg(unix)]
fn detect_last_session_via_tmux(
    provider_filter: Option<Provider>,
    project_override: Option<&str>,
) -> Option<DetectedSession> {
    let pane_id = env::var("TMUX_PANE").ok()?;

    // Get pane info from tmux
    let output = std::process::Command::new("tmux")
        .args([
            "display-message",
            "-t",
            &pane_id,
            "-p",
            "#{pane_pid} #{pane_current_command} #{pane_current_path}",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let info_line = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let parts: Vec<&str> = info_line.splitn(3, ' ').collect();
    if parts.len() < 3 {
        return None;
    }
    let pane_pid: u32 = parts[0].parse().ok()?;
    let pane_command = parts[1];
    let pane_path = parts[2];

    // Step 1: If AI process running, try to extract session from process tree
    let is_ai_command = pane_command == "opencode" || pane_command == "claude";
    if is_ai_command {
        let provider = if pane_command == "opencode" {
            Provider::OpenCode
        } else {
            Provider::ClaudeCode
        };
        if let Some(detected) = try_session_from_pane_process(pane_pid, pane_command, provider) {
            if provider_filter.is_none() || provider_filter == Some(detected.provider) {
                return Some(detected);
            }
        }
    }

    // Step 2: Capture pane content for snippet matching
    let visible = capture_pane_visible_content(&pane_id).unwrap_or_default();
    let alternate = capture_pane_content(&pane_id).unwrap_or_default();

    // Step 3: Determine project directory (override or pane's cwd)
    let project_dir = project_override
        .map(|s| s.to_string())
        .unwrap_or_else(|| pane_path.to_string());

    let dir = Path::new(&project_dir);

    // Step 4: Gather candidates
    let mut candidates = gather_candidates_for_dir(dir, provider_filter);
    if candidates.is_empty() {
        return None;
    }

    // Sort by updated_ms descending (most recent first)
    candidates.sort_by(|a, b| b.updated_ms.cmp(&a.updated_ms));

    if candidates.len() == 1 {
        return Some(DetectedSession {
            session_id: candidates[0].session_id.clone(),
            provider: candidates[0].provider,
        });
    }

    // Step 5: Match session log content against pane content (primary strategy)
    let candidate_snippets: Vec<(String, Vec<String>)> = candidates
        .iter()
        .map(|c| {
            let snippets = match c.provider {
                Provider::OpenCode => get_opencode_user_snippets(&c.session_id, 5),
                Provider::ClaudeCode => get_claudecode_user_snippets(&c.session_id, 5),
            };
            (c.session_id.clone(), snippets)
        })
        .collect();

    let pane_contents: Vec<String> = [visible, alternate]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect();

    if let Some(detected) = match_snippets_mixed(&candidate_snippets, &candidates, &pane_contents) {
        return Some(detected);
    }

    // Step 6: Fallback — return most recent candidate
    Some(DetectedSession {
        session_id: candidates[0].session_id.clone(),
        provider: candidates[0].provider,
    })
}

/// Capture the visible pane content including scrollback (up to 500 lines back).
#[cfg(unix)]
fn capture_pane_visible_content(pane_id: &str) -> Option<String> {
    let output = std::process::Command::new("tmux")
        .args(["capture-pane", "-p", "-S", "-500", "-t", pane_id])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Gather candidates from both providers for a given directory.
fn gather_candidates_for_dir(dir: &Path, provider_filter: Option<Provider>) -> Vec<Candidate> {
    let mut candidates = Vec::new();

    let include_opencode = provider_filter.is_none() || provider_filter == Some(Provider::OpenCode);
    let include_claudecode =
        provider_filter.is_none() || provider_filter == Some(Provider::ClaudeCode);

    if include_opencode {
        if let Ok(oc) = gather_opencode_candidates(dir) {
            candidates.extend(oc);
        }
    }
    if include_claudecode {
        if let Ok(cc) = gather_claudecode_candidates(dir) {
            candidates.extend(cc);
        }
    }

    candidates
}

/// Find the most recent session for a given directory.
fn find_most_recent_session(
    dir: &Path,
    provider_filter: Option<Provider>,
) -> Result<DetectedSession> {
    let mut candidates = gather_candidates_for_dir(dir, provider_filter);
    if candidates.is_empty() {
        bail!("No sessions found for directory: {}", dir.display());
    }
    candidates.sort_by(|a, b| b.updated_ms.cmp(&a.updated_ms));
    Ok(DetectedSession {
        session_id: candidates[0].session_id.clone(),
        provider: candidates[0].provider,
    })
}

/// Match snippets against pane contents for candidates with mixed providers.
///
/// Like `match_snippets_against_panes` but handles candidates from different
/// providers. Returns the candidate with the most snippet hits (minimum 1 hit).
/// On tie, returns the most recent candidate (lowest index, since candidates
/// are sorted by `updated_ms` descending).
fn match_snippets_mixed(
    candidate_snippets: &[(String, Vec<String>)],
    candidates: &[Candidate],
    pane_contents: &[String],
) -> Option<DetectedSession> {
    if candidate_snippets.is_empty() || pane_contents.is_empty() {
        return None;
    }

    let mut scores: Vec<(usize, usize)> = Vec::new(); // (max_hits, candidate_index)

    for (idx, (_session_id, snippets)) in candidate_snippets.iter().enumerate() {
        if snippets.is_empty() {
            scores.push((0, idx));
            continue;
        }
        let mut max_hits = 0usize;
        for pane_content in pane_contents {
            let hits = snippets
                .iter()
                .filter(|s| pane_content.contains(s.as_str()))
                .count();
            max_hits = max_hits.max(hits);
        }
        scores.push((max_hits, idx));
    }

    scores.sort_by(|a, b| b.0.cmp(&a.0));
    let best = scores[0];
    if best.0 == 0 {
        return None;
    }
    // On tie, prefer the candidate with the lowest index (most recent,
    // since candidates are sorted by updated_ms descending).
    if scores.len() > 1 && scores[1].0 == best.0 {
        let tied: Vec<&(usize, usize)> = scores.iter().filter(|s| s.0 == best.0).collect();
        let winner_idx = tied.iter().map(|s| s.1).min().unwrap();
        let winner = &candidates[winner_idx];
        return Some(DetectedSession {
            session_id: winner.session_id.clone(),
            provider: winner.provider,
        });
    }

    let winner = &candidates[best.1];
    Some(DetectedSession {
        session_id: winner.session_id.clone(),
        provider: winner.provider,
    })
}

/// Try to find an AI process in the pane's process tree and extract session info.
#[cfg(unix)]
fn try_session_from_pane_process(
    pane_pid: u32,
    command_name: &str,
    provider: Provider,
) -> Option<DetectedSession> {
    let ai_pid = find_descendant_process(pane_pid, command_name, 3)?;
    let raw = fs::read_to_string(format!("/proc/{}/cmdline", ai_pid)).ok()?;
    let session_id = parse_session_from_cmdline_args(&raw)?;
    Some(DetectedSession {
        session_id,
        provider,
    })
}

/// Find a descendant process with the given name, up to `max_depth` levels deep.
#[cfg(unix)]
fn find_descendant_process(parent_pid: u32, target_name: &str, max_depth: u32) -> Option<u32> {
    if max_depth == 0 {
        return None;
    }

    // Try /proc/<pid>/task/<pid>/children first (efficient, Linux 3.5+)
    let children_file = format!("/proc/{}/task/{}/children", parent_pid, parent_pid);
    let child_pids: Vec<u32> = if let Ok(content) = fs::read_to_string(&children_file) {
        content
            .split_whitespace()
            .filter_map(|s| s.parse().ok())
            .collect()
    } else {
        // Fallback: scan /proc for direct children
        let mut pids = Vec::new();
        if let Ok(proc_dir) = fs::read_dir("/proc") {
            for entry in proc_dir.flatten() {
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                if let Ok(pid) = name_str.parse::<u32>() {
                    if get_parent_pid(pid) == Some(parent_pid) {
                        pids.push(pid);
                    }
                }
            }
        }
        pids
    };

    // Check direct children first
    for &pid in &child_pids {
        if let Some(name) = get_process_name(pid) {
            if name == target_name {
                return Some(pid);
            }
        }
    }

    // Recurse into children
    for &pid in &child_pids {
        if let Some(found) = find_descendant_process(pid, target_name, max_depth - 1) {
            return Some(found);
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    // === Provider detection tests ===

    #[test]
    fn test_detect_provider_opencode_env() {
        // This test relies on OPENCODE=1 being set in the current env
        // (which it is when running inside OpenCode).
        // We test the logic directly instead.
        if env::var("OPENCODE").as_deref() == Ok("1") {
            let provider = detect_provider().unwrap();
            assert_eq!(provider, Provider::OpenCode);
        }
    }

    // === Candidate gathering tests ===

    #[test]
    fn test_gather_opencode_candidates_filters_children() {
        let temp = tempdir().unwrap();
        let session_dir = temp.path().join("storage/session/project1");
        fs::create_dir_all(&session_dir).unwrap();

        // Main session
        fs::write(
            session_dir.join("ses_main.json"),
            r#"{"id":"ses_main","directory":"/test/dir","time":{"created":1000,"updated":2000}}"#,
        )
        .unwrap();

        // Child session (should be filtered out)
        fs::write(
            session_dir.join("ses_child.json"),
            r#"{"id":"ses_child","parentID":"ses_main","directory":"/test/dir","time":{"created":1500,"updated":2500}}"#,
        ).unwrap();

        // Override data dir for test
        let cwd = Path::new("/test/dir");
        let candidates = gather_opencode_candidates_from(temp.path(), cwd).unwrap();

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].session_id, "ses_main");
    }

    #[test]
    fn test_gather_opencode_candidates_filters_by_directory() {
        let temp = tempdir().unwrap();
        let session_dir = temp.path().join("storage/session/project1");
        fs::create_dir_all(&session_dir).unwrap();

        // Session for a different directory
        fs::write(
            session_dir.join("ses_other.json"),
            r#"{"id":"ses_other","directory":"/other/dir","time":{"created":1000,"updated":2000}}"#,
        )
        .unwrap();

        let cwd = Path::new("/test/dir");
        let candidates = gather_opencode_candidates_from(temp.path(), cwd).unwrap();

        assert!(candidates.is_empty());
    }

    #[test]
    fn test_gather_opencode_candidates_sorts_by_updated() {
        let temp = tempdir().unwrap();
        let session_dir = temp.path().join("storage/session/project1");
        fs::create_dir_all(&session_dir).unwrap();

        fs::write(
            session_dir.join("ses_old.json"),
            r#"{"id":"ses_old","directory":"/test/dir","time":{"created":1000,"updated":1000}}"#,
        )
        .unwrap();

        fs::write(
            session_dir.join("ses_new.json"),
            r#"{"id":"ses_new","directory":"/test/dir","time":{"created":2000,"updated":3000}}"#,
        )
        .unwrap();

        let cwd = Path::new("/test/dir");
        let candidates = gather_opencode_candidates_from(temp.path(), cwd).unwrap();

        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].session_id, "ses_new"); // most recent first
        assert_eq!(candidates[1].session_id, "ses_old");
    }

    // === Testable variants of functions that accept path overrides ===

    fn gather_opencode_candidates_from(data_dir: &Path, cwd: &Path) -> Result<Vec<Candidate>> {
        let session_base = data_dir.join("storage/session");
        if !session_base.exists() {
            return Ok(Vec::new());
        }

        let cwd_str = cwd.to_string_lossy();
        let mut candidates = Vec::new();

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
                    if let Ok(session) = serde_json::from_str::<serde_json::Value>(&content) {
                        if session.get("parentID").and_then(|v| v.as_str()).is_some() {
                            continue;
                        }
                        let dir = session
                            .get("directory")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        if dir != cwd_str.as_ref() {
                            continue;
                        }
                        let id = session
                            .get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let updated = session
                            .get("time")
                            .and_then(|t| t.get("updated"))
                            .and_then(|v| v.as_i64())
                            .unwrap_or(0);
                        if !id.is_empty() {
                            candidates.push(Candidate {
                                session_id: id,
                                provider: Provider::OpenCode,
                                updated_ms: updated,
                            });
                        }
                    }
                }
            }
        }

        candidates.sort_by(|a, b| b.updated_ms.cmp(&a.updated_ms));
        Ok(candidates)
    }

    // === Cmdline session ID extraction tests ===

    #[test]
    fn test_parse_session_from_cmdline_short_flag() {
        let cmdline = "opencode\0-s\0ses_abc123\0";
        assert_eq!(
            parse_session_from_cmdline_args(cmdline),
            Some("ses_abc123".to_string())
        );
    }

    #[test]
    fn test_parse_session_from_cmdline_long_flag() {
        let cmdline = "opencode\0--session\0ses_def456\0";
        assert_eq!(
            parse_session_from_cmdline_args(cmdline),
            Some("ses_def456".to_string())
        );
    }

    #[test]
    fn test_parse_session_from_cmdline_attach_mode() {
        // opencode attach http://127.0.0.1:4096 -s ses_xxx --dir /some/path
        let cmdline =
            "opencode\0attach\0http://127.0.0.1:4096\0-s\0ses_attach789\0--dir\0/some/path\0";
        assert_eq!(
            parse_session_from_cmdline_args(cmdline),
            Some("ses_attach789".to_string())
        );
    }

    #[test]
    fn test_parse_session_from_cmdline_no_session() {
        let cmdline = "opencode\0serve\0--hostname\0127.0.0.1\0--port\04096\0";
        assert_eq!(parse_session_from_cmdline_args(cmdline), None);
    }

    #[test]
    fn test_parse_session_from_cmdline_empty_value() {
        let cmdline = "opencode\0-s\0\0";
        assert_eq!(parse_session_from_cmdline_args(cmdline), None);
    }

    // === Tmux pane content matching tests ===

    #[test]
    fn test_safe_truncate_ascii() {
        assert_eq!(safe_truncate("hello world", 5), "hello");
        assert_eq!(safe_truncate("hello", 10), "hello");
        assert_eq!(safe_truncate("", 5), "");
    }

    #[test]
    fn test_safe_truncate_utf8() {
        // "café" is 4 chars but 5 bytes (é = 2 bytes)
        assert_eq!(safe_truncate("café", 3), "caf");
        assert_eq!(safe_truncate("café", 4), "café");
        // Japanese: each char is 3 bytes
        assert_eq!(safe_truncate("日本語テスト", 3), "日本語");
    }

    #[test]
    fn test_get_opencode_user_snippets_basic() {
        let temp = tempdir().unwrap();
        let storage = temp.path();

        let session_id = "ses_snip1";
        let message_dir = storage.join("message").join(session_id);
        let part_dir = storage.join("part");
        fs::create_dir_all(&message_dir).unwrap();

        // Create a user message with a text part
        let msg_id = "msg_001";
        fs::write(
            message_dir.join(format!("{}.json", msg_id)),
            r#"{"id":"msg_001","sessionID":"ses_snip1","role":"user","time":{"created":1000}}"#,
        )
        .unwrap();

        let msg_part_dir = part_dir.join(msg_id);
        fs::create_dir_all(&msg_part_dir).unwrap();
        fs::write(
            msg_part_dir.join("prt_001.json"),
            r#"{"id":"prt_001","type":"text","text":"Please help me refactor the authentication module"}"#,
        )
        .unwrap();

        let snippets = get_opencode_user_snippets_in(storage, session_id, 5);
        assert_eq!(snippets.len(), 1);
        assert_eq!(
            snippets[0],
            "Please help me refactor the authentication module"
        );
    }

    #[test]
    fn test_get_opencode_user_snippets_skips_short_messages() {
        let temp = tempdir().unwrap();
        let storage = temp.path();

        let session_id = "ses_snip2";
        let message_dir = storage.join("message").join(session_id);
        let part_dir = storage.join("part");
        fs::create_dir_all(&message_dir).unwrap();

        // Short message (< 12 chars) should be skipped
        let msg_id = "msg_001";
        fs::write(
            message_dir.join(format!("{}.json", msg_id)),
            r#"{"id":"msg_001","sessionID":"ses_snip2","role":"user","time":{"created":1000}}"#,
        )
        .unwrap();

        let msg_part_dir = part_dir.join(msg_id);
        fs::create_dir_all(&msg_part_dir).unwrap();
        fs::write(
            msg_part_dir.join("prt_001.json"),
            r#"{"id":"prt_001","type":"text","text":"yes"}"#,
        )
        .unwrap();

        let snippets = get_opencode_user_snippets_in(storage, session_id, 5);
        assert!(snippets.is_empty());
    }

    #[test]
    fn test_get_opencode_user_snippets_skips_assistant_messages() {
        let temp = tempdir().unwrap();
        let storage = temp.path();

        let session_id = "ses_snip3";
        let message_dir = storage.join("message").join(session_id);
        let part_dir = storage.join("part");
        fs::create_dir_all(&message_dir).unwrap();

        // Assistant message should be skipped
        let msg_id = "msg_001";
        fs::write(
            message_dir.join(format!("{}.json", msg_id)),
            r#"{"id":"msg_001","sessionID":"ses_snip3","role":"assistant","time":{"created":1000}}"#,
        )
        .unwrap();

        let msg_part_dir = part_dir.join(msg_id);
        fs::create_dir_all(&msg_part_dir).unwrap();
        fs::write(
            msg_part_dir.join("prt_001.json"),
            r#"{"id":"prt_001","type":"text","text":"Here is the refactored authentication module"}"#,
        )
        .unwrap();

        let snippets = get_opencode_user_snippets_in(storage, session_id, 5);
        assert!(snippets.is_empty());
    }

    #[test]
    fn test_get_opencode_user_snippets_truncates_long_messages() {
        let temp = tempdir().unwrap();
        let storage = temp.path();

        let session_id = "ses_snip4";
        let message_dir = storage.join("message").join(session_id);
        let part_dir = storage.join("part");
        fs::create_dir_all(&message_dir).unwrap();

        let long_text = "A".repeat(200);
        let msg_id = "msg_001";
        fs::write(
            message_dir.join(format!("{}.json", msg_id)),
            r#"{"id":"msg_001","sessionID":"ses_snip4","role":"user","time":{"created":1000}}"#,
        )
        .unwrap();

        let msg_part_dir = part_dir.join(msg_id);
        fs::create_dir_all(&msg_part_dir).unwrap();
        fs::write(
            msg_part_dir.join("prt_001.json"),
            format!(r#"{{"id":"prt_001","type":"text","text":"{}"}}"#, long_text),
        )
        .unwrap();

        let snippets = get_opencode_user_snippets_in(storage, session_id, 5);
        assert_eq!(snippets.len(), 1);
        assert_eq!(snippets[0].chars().count(), 60);
    }

    #[test]
    fn test_get_opencode_user_snippets_respects_max_messages() {
        let temp = tempdir().unwrap();
        let storage = temp.path();

        let session_id = "ses_snip5";
        let message_dir = storage.join("message").join(session_id);
        let part_dir = storage.join("part");
        fs::create_dir_all(&message_dir).unwrap();

        // Create 5 user messages
        for i in 1..=5 {
            let msg_id = format!("msg_{:03}", i);
            fs::write(
                message_dir.join(format!("{}.json", msg_id)),
                format!(
                    r#"{{"id":"{}","sessionID":"ses_snip5","role":"user","time":{{"created":{}}}}}"#,
                    msg_id,
                    i * 1000
                ),
            )
            .unwrap();

            let msg_part_dir = part_dir.join(&msg_id);
            fs::create_dir_all(&msg_part_dir).unwrap();
            fs::write(
                msg_part_dir.join("prt_001.json"),
                format!(
                    r#"{{"id":"prt_001","type":"text","text":"User message number {} with enough text"}}"#,
                    i
                ),
            )
            .unwrap();
        }

        // Request only 2
        let snippets = get_opencode_user_snippets_in(storage, session_id, 2);
        assert_eq!(snippets.len(), 2);
        // Should be the most recent (msg_005, msg_004)
        assert!(snippets[0].contains("number 5"));
        assert!(snippets[1].contains("number 4"));
    }

    #[test]
    fn test_get_opencode_user_snippets_nonexistent_session() {
        let temp = tempdir().unwrap();
        let snippets = get_opencode_user_snippets_in(temp.path(), "ses_nonexistent", 5);
        assert!(snippets.is_empty());
    }

    #[test]
    fn test_get_claudecode_user_snippets_basic() {
        let temp = tempdir().unwrap();
        let session_file = temp.path().join("session.jsonl");

        let lines = vec![
            r#"{"type":"human","timestamp":"2024-01-15T10:00:00.000Z","message":{"role":"user","content":"Please help me refactor the authentication module"}}"#,
        ];
        fs::write(&session_file, lines.join("\n") + "\n").unwrap();

        let snippets = get_claudecode_user_snippets_in(&session_file, 5);
        assert_eq!(snippets.len(), 1);
        assert_eq!(
            snippets[0],
            "Please help me refactor the authentication module"
        );
    }

    #[test]
    fn test_get_claudecode_user_snippets_array_content() {
        let temp = tempdir().unwrap();
        let session_file = temp.path().join("session.jsonl");

        let line = r#"{"type":"human","timestamp":"2024-01-15T10:00:00.000Z","message":{"role":"user","content":[{"type":"text","text":"Implement the new feature for user profiles"}]}}"#;
        fs::write(&session_file, format!("{}\n", line)).unwrap();

        let snippets = get_claudecode_user_snippets_in(&session_file, 5);
        assert_eq!(snippets.len(), 1);
        assert_eq!(snippets[0], "Implement the new feature for user profiles");
    }

    #[test]
    fn test_get_claudecode_user_snippets_skips_assistant() {
        let temp = tempdir().unwrap();
        let session_file = temp.path().join("session.jsonl");

        let lines = vec![
            r#"{"type":"assistant","timestamp":"2024-01-15T10:00:00.000Z","message":{"role":"assistant","content":"Here is the refactored code for the module"}}"#,
        ];
        fs::write(&session_file, lines.join("\n") + "\n").unwrap();

        let snippets = get_claudecode_user_snippets_in(&session_file, 5);
        assert!(snippets.is_empty());
    }

    #[test]
    fn test_get_claudecode_user_snippets_returns_last_n() {
        let temp = tempdir().unwrap();
        let session_file = temp.path().join("session.jsonl");

        let mut lines = Vec::new();
        for i in 1..=5 {
            lines.push(format!(
                r#"{{"type":"human","timestamp":"2024-01-15T10:{:02}:00.000Z","message":{{"role":"user","content":"User message number {} with enough text to pass minimum"}}}}"#,
                i, i
            ));
        }
        fs::write(&session_file, lines.join("\n") + "\n").unwrap();

        let snippets = get_claudecode_user_snippets_in(&session_file, 2);
        assert_eq!(snippets.len(), 2);
        assert!(snippets[0].contains("number 4"));
        assert!(snippets[1].contains("number 5"));
    }

    #[test]
    fn test_get_claudecode_user_snippets_nonexistent_file() {
        let snippets = get_claudecode_user_snippets_in(Path::new("/nonexistent/session.jsonl"), 5);
        assert!(snippets.is_empty());
    }

    #[test]
    fn test_match_snippets_against_panes_single_clear_winner() {
        let candidate_snippets = vec![
            (
                "ses_aaa".to_string(),
                vec![
                    "Please refactor the auth module".to_string(),
                    "Add unit tests for login".to_string(),
                ],
            ),
            (
                "ses_bbb".to_string(),
                vec![
                    "Deploy to production server".to_string(),
                    "Check the CI pipeline status".to_string(),
                ],
            ),
        ];

        let pane_contents = vec![
            "... Please refactor the auth module ... Add unit tests for login ...".to_string(),
        ];

        let result =
            match_snippets_against_panes(&candidate_snippets, Provider::OpenCode, &pane_contents);
        assert!(result.is_some());
        let detected = result.unwrap();
        assert_eq!(detected.session_id, "ses_aaa");
        assert_eq!(detected.provider, Provider::OpenCode);
    }

    #[test]
    fn test_match_snippets_against_panes_no_matches() {
        let candidate_snippets = vec![(
            "ses_aaa".to_string(),
            vec!["Some unique text snippet".to_string()],
        )];

        let pane_contents = vec!["Completely unrelated pane content here".to_string()];

        let result =
            match_snippets_against_panes(&candidate_snippets, Provider::OpenCode, &pane_contents);
        assert!(result.is_none());
    }

    #[test]
    fn test_match_snippets_against_panes_tie_returns_first() {
        let candidate_snippets = vec![
            (
                "ses_aaa".to_string(),
                vec!["shared snippet text here".to_string()],
            ),
            (
                "ses_bbb".to_string(),
                vec!["shared snippet text here".to_string()],
            ),
        ];

        let pane_contents = vec!["... shared snippet text here ...".to_string()];

        let result =
            match_snippets_against_panes(&candidate_snippets, Provider::OpenCode, &pane_contents);
        // On tie, first candidate wins (callers pass candidates sorted by recency)
        let detected = result.unwrap();
        assert_eq!(detected.session_id, "ses_aaa");
    }

    #[test]
    fn test_match_snippets_against_panes_empty_inputs() {
        // Empty candidates
        let result =
            match_snippets_against_panes(&[], Provider::OpenCode, &["content".to_string()]);
        assert!(result.is_none());

        // Empty panes
        let candidate_snippets = vec![("ses_aaa".to_string(), vec!["some snippet".to_string()])];
        let result = match_snippets_against_panes(&candidate_snippets, Provider::OpenCode, &[]);
        assert!(result.is_none());
    }

    #[test]
    fn test_match_snippets_against_panes_multiple_panes() {
        let candidate_snippets = vec![
            (
                "ses_aaa".to_string(),
                vec!["refactor the auth module".to_string()],
            ),
            (
                "ses_bbb".to_string(),
                vec!["deploy to production now".to_string()],
            ),
        ];

        // ses_bbb's snippet is in pane 2
        let pane_contents = vec![
            "pane 1: unrelated content here".to_string(),
            "pane 2: deploy to production now please".to_string(),
        ];

        let result =
            match_snippets_against_panes(&candidate_snippets, Provider::OpenCode, &pane_contents);
        assert!(result.is_some());
        assert_eq!(result.unwrap().session_id, "ses_bbb");
    }

    #[test]
    fn test_match_snippets_against_panes_winner_by_count() {
        // ses_aaa has 2 hits, ses_bbb has 1 hit → ses_aaa wins
        let candidate_snippets = vec![
            (
                "ses_aaa".to_string(),
                vec![
                    "refactor the auth module".to_string(),
                    "add tests for login flow".to_string(),
                ],
            ),
            (
                "ses_bbb".to_string(),
                vec!["refactor the auth module".to_string()],
            ),
        ];

        let pane_contents =
            vec!["... refactor the auth module ... add tests for login flow ...".to_string()];

        let result =
            match_snippets_against_panes(&candidate_snippets, Provider::OpenCode, &pane_contents);
        assert!(result.is_some());
        assert_eq!(result.unwrap().session_id, "ses_aaa");
    }

    #[test]
    fn test_match_snippets_against_panes_candidate_with_no_snippets() {
        let candidate_snippets = vec![
            ("ses_aaa".to_string(), vec![]),
            (
                "ses_bbb".to_string(),
                vec!["deploy to production now".to_string()],
            ),
        ];

        let pane_contents = vec!["deploy to production now".to_string()];

        let result =
            match_snippets_against_panes(&candidate_snippets, Provider::OpenCode, &pane_contents);
        assert!(result.is_some());
        assert_eq!(result.unwrap().session_id, "ses_bbb");
    }

    // === DB-based tests ===

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
        time_updated: i64,
    ) {
        conn.execute(
            "INSERT INTO session (id, project_id, parent_id, directory, title, time_created, time_updated) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![id, "proj_1", parent_id, directory, "", time_updated, time_updated],
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
    fn test_gather_opencode_candidates_from_conn_basic() {
        let conn = setup_test_db();
        insert_session_db(&conn, "ses_001", None, "/test/dir", 2000);
        insert_session_db(&conn, "ses_002", None, "/test/dir", 3000);
        insert_session_db(&conn, "ses_003", None, "/other/dir", 4000);

        let candidates = gather_opencode_candidates_from_conn(&conn, Path::new("/test/dir"));
        assert_eq!(candidates.len(), 2);
        // Most recently updated first
        assert_eq!(candidates[0].session_id, "ses_002");
        assert_eq!(candidates[1].session_id, "ses_001");
    }

    #[test]
    fn test_gather_opencode_candidates_from_conn_filters_children() {
        let conn = setup_test_db();
        insert_session_db(&conn, "ses_main", None, "/test/dir", 2000);
        insert_session_db(&conn, "ses_child", Some("ses_main"), "/test/dir", 3000);

        let candidates = gather_opencode_candidates_from_conn(&conn, Path::new("/test/dir"));
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].session_id, "ses_main");
    }

    #[test]
    fn test_gather_opencode_candidates_from_conn_empty() {
        let conn = setup_test_db();
        let candidates = gather_opencode_candidates_from_conn(&conn, Path::new("/test/dir"));
        assert!(candidates.is_empty());
    }

    #[test]
    fn test_merge_candidates_dedup_db_wins() {
        let file_candidates = vec![
            Candidate {
                session_id: "ses_001".to_string(),
                provider: Provider::OpenCode,
                updated_ms: 1000,
            },
            Candidate {
                session_id: "ses_002".to_string(),
                provider: Provider::OpenCode,
                updated_ms: 2000,
            },
        ];

        let db_candidates = vec![Candidate {
            session_id: "ses_001".to_string(),
            provider: Provider::OpenCode,
            updated_ms: 5000, // DB has newer timestamp
        }];

        let merged = merge_candidates(file_candidates, db_candidates);
        assert_eq!(merged.len(), 2);
        // ses_001 should have DB's updated_ms
        let ses_001 = merged.iter().find(|c| c.session_id == "ses_001").unwrap();
        assert_eq!(ses_001.updated_ms, 5000);
    }

    #[test]
    fn test_merge_candidates_combines_unique() {
        let file_candidates = vec![Candidate {
            session_id: "ses_file".to_string(),
            provider: Provider::OpenCode,
            updated_ms: 1000,
        }];

        let db_candidates = vec![Candidate {
            session_id: "ses_db".to_string(),
            provider: Provider::OpenCode,
            updated_ms: 2000,
        }];

        let merged = merge_candidates(file_candidates, db_candidates);
        assert_eq!(merged.len(), 2);
    }

    #[test]
    fn test_get_opencode_user_snippets_from_db_basic() {
        let conn = setup_test_db();
        insert_message_db(&conn, "msg_001", "ses_snip_db1", "user", 1705314600000);
        insert_part_db(
            &conn,
            "prt_001",
            "msg_001",
            "ses_snip_db1",
            1705314600000,
            r#"{"type":"text","text":"Please help me refactor the authentication module"}"#,
        );

        let snippets = get_opencode_user_snippets_from_db(&conn, "ses_snip_db1", 5);
        assert_eq!(snippets.len(), 1);
        assert_eq!(
            snippets[0],
            "Please help me refactor the authentication module"
        );
    }

    #[test]
    fn test_get_opencode_user_snippets_from_db_skips_short() {
        let conn = setup_test_db();
        insert_message_db(&conn, "msg_001", "ses_snip_db2", "user", 1705314600000);
        insert_part_db(
            &conn,
            "prt_001",
            "msg_001",
            "ses_snip_db2",
            1705314600000,
            r#"{"type":"text","text":"yes"}"#,
        );

        let snippets = get_opencode_user_snippets_from_db(&conn, "ses_snip_db2", 5);
        assert!(snippets.is_empty());
    }

    #[test]
    fn test_get_opencode_user_snippets_from_db_skips_assistant() {
        let conn = setup_test_db();
        insert_message_db(&conn, "msg_001", "ses_snip_db3", "assistant", 1705314600000);
        insert_part_db(
            &conn,
            "prt_001",
            "msg_001",
            "ses_snip_db3",
            1705314600000,
            r#"{"type":"text","text":"Here is the refactored authentication module"}"#,
        );

        let snippets = get_opencode_user_snippets_from_db(&conn, "ses_snip_db3", 5);
        assert!(snippets.is_empty());
    }

    #[test]
    fn test_get_opencode_user_snippets_from_db_respects_max() {
        let conn = setup_test_db();
        for i in 1..=5 {
            let msg_id = format!("msg_{:03}", i);
            let prt_id = format!("prt_{:03}", i);
            insert_message_db(
                &conn,
                &msg_id,
                "ses_snip_db4",
                "user",
                1705314600000 + i * 1000,
            );
            insert_part_db(
                &conn,
                &prt_id,
                &msg_id,
                "ses_snip_db4",
                1705314600000 + i * 1000,
                &format!(
                    r#"{{"type":"text","text":"User message number {} with enough text"}}"#,
                    i
                ),
            );
        }

        let snippets = get_opencode_user_snippets_from_db(&conn, "ses_snip_db4", 2);
        assert_eq!(snippets.len(), 2);
        // Should be the most recent (msg_005, msg_004)
        assert!(snippets[0].contains("number 5"));
        assert!(snippets[1].contains("number 4"));
    }

    // === match_snippets_mixed tests ===

    #[test]
    fn test_match_snippets_mixed_single_winner() {
        let candidates = vec![
            Candidate {
                session_id: "ses_aaa".to_string(),
                provider: Provider::OpenCode,
                updated_ms: 1000,
            },
            Candidate {
                session_id: "ses_bbb".to_string(),
                provider: Provider::OpenCode,
                updated_ms: 2000,
            },
        ];
        let candidate_snippets = vec![
            (
                "ses_aaa".to_string(),
                vec!["refactor the auth module".to_string()],
            ),
            (
                "ses_bbb".to_string(),
                vec!["deploy to production now".to_string()],
            ),
        ];
        let pane_contents = vec!["... refactor the auth module ...".to_string()];

        let result = match_snippets_mixed(&candidate_snippets, &candidates, &pane_contents);
        assert!(result.is_some());
        assert_eq!(result.unwrap().session_id, "ses_aaa");
    }

    #[test]
    fn test_match_snippets_mixed_no_match() {
        let candidates = vec![Candidate {
            session_id: "ses_aaa".to_string(),
            provider: Provider::OpenCode,
            updated_ms: 1000,
        }];
        let candidate_snippets = vec![("ses_aaa".to_string(), vec!["unique text".to_string()])];
        let pane_contents = vec!["completely unrelated content".to_string()];

        let result = match_snippets_mixed(&candidate_snippets, &candidates, &pane_contents);
        assert!(result.is_none());
    }

    #[test]
    fn test_match_snippets_mixed_tie_returns_first() {
        // Candidates sorted by updated_ms descending (as in real usage)
        let candidates = vec![
            Candidate {
                session_id: "ses_bbb".to_string(),
                provider: Provider::OpenCode,
                updated_ms: 2000,
            },
            Candidate {
                session_id: "ses_aaa".to_string(),
                provider: Provider::OpenCode,
                updated_ms: 1000,
            },
        ];
        let candidate_snippets = vec![
            ("ses_bbb".to_string(), vec!["shared text".to_string()]),
            ("ses_aaa".to_string(), vec!["shared text".to_string()]),
        ];
        let pane_contents = vec!["... shared text ...".to_string()];

        // On tie, returns the first candidate (most recent by updated_ms)
        let result = match_snippets_mixed(&candidate_snippets, &candidates, &pane_contents);
        let detected = result.unwrap();
        assert_eq!(detected.session_id, "ses_bbb");
    }

    #[test]
    fn test_match_snippets_mixed_empty_inputs() {
        let result = match_snippets_mixed(&[], &[], &["content".to_string()]);
        assert!(result.is_none());

        let candidates = vec![Candidate {
            session_id: "ses_aaa".to_string(),
            provider: Provider::OpenCode,
            updated_ms: 1000,
        }];
        let candidate_snippets = vec![("ses_aaa".to_string(), vec!["some snippet".to_string()])];
        let result = match_snippets_mixed(&candidate_snippets, &candidates, &[]);
        assert!(result.is_none());
    }

    #[test]
    fn test_match_snippets_mixed_providers() {
        let candidates = vec![
            Candidate {
                session_id: "ses_oc".to_string(),
                provider: Provider::OpenCode,
                updated_ms: 1000,
            },
            Candidate {
                session_id: "uuid-cc-1234".to_string(),
                provider: Provider::ClaudeCode,
                updated_ms: 2000,
            },
        ];
        let candidate_snippets = vec![
            (
                "ses_oc".to_string(),
                vec!["opencode specific text here".to_string()],
            ),
            (
                "uuid-cc-1234".to_string(),
                vec!["claude specific text here".to_string()],
            ),
        ];
        let pane_contents = vec!["... claude specific text here ...".to_string()];

        let result = match_snippets_mixed(&candidate_snippets, &candidates, &pane_contents);
        assert!(result.is_some());
        let detected = result.unwrap();
        assert_eq!(detected.session_id, "uuid-cc-1234");
        assert_eq!(detected.provider, Provider::ClaudeCode);
    }

    // === CLI parsing test for LastSession ===

    #[test]
    fn cli_accepts_last_session_command() {
        use clap::Parser;

        let args = crate::cli::Args::try_parse_from(["ai-audit", "last-session"])
            .expect("bare last-session should work");
        match args.command {
            crate::cli::Commands::LastSession {
                session_type,
                project,
                ..
            } => {
                assert!(session_type.is_none());
                assert!(project.is_none());
            }
            _ => panic!("expected LastSession command"),
        }
    }

    #[test]
    fn cli_last_session_with_type_and_project() {
        use clap::Parser;

        let args = crate::cli::Args::try_parse_from([
            "ai-audit",
            "last-session",
            "-t",
            "opencode",
            "-p",
            "/home/user/project",
        ])
        .expect("last-session with flags should work");
        match args.command {
            crate::cli::Commands::LastSession {
                session_type,
                project,
                ..
            } => {
                assert!(session_type.is_some());
                assert_eq!(project, Some("/home/user/project".to_string()));
            }
            _ => panic!("expected LastSession command"),
        }
    }
}

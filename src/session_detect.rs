//! Auto-detect the current AI session that spawned this process.
//!
//! Detection strategy (in priority order):
//! 1. Check env vars for authoritative session ID (future-proof)
//! 2. Parse ancestor process cmdline for `-s`/`--session` flag
//! 3. Detect provider from env + process tree
//! 4. Gather candidate sessions for the current working directory
//! 5. Filter to non-child, recently updated sessions
//! 6. Fingerprint: check each candidate's transcript for our own
//!    invocation (the bash tool call that spawned us)
//!
//! If exactly one candidate matches, return it. Otherwise fail
//! with an actionable error listing candidates.

use anyhow::{bail, Context, Result};
use std::env;
use std::fs;
use std::path::Path;

/// Detected provider type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    OpenCode,
    ClaudeCode,
}

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
    #[allow(dead_code)]
    provider: Provider,
    updated_ms: i64,
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

/// Find a session by examining a process identified by its PID.
///
/// Detection chain:
/// 1. Read `/proc/<pid>/environ` for `OPENCODE_SESSION_ID` or `CLAUDE_SESSION_ID`
/// 2. Check cmdline of the target PID and ancestors for `-s`/`--session` flag
/// 3. Detect provider from the process name or its ancestors
/// 4. Determine working directory from `/proc/<pid>/cwd` or `--dir` in cmdline
/// 5. Gather candidate sessions for that directory
/// 6. If one candidate → return it; if multiple → fingerprint then most-recent fallback
#[cfg(unix)]
pub fn find_session_by_pid(pid: u32, provider_filter: Option<Provider>) -> Result<DetectedSession> {
    // Step 1: Check env vars in the target process
    if let Some(detected) = check_process_env_vars(pid) {
        // If a provider filter is set, ensure it matches
        if provider_filter.is_none() || provider_filter == Some(detected.provider) {
            return Ok(detected);
        }
    }

    // Step 2: Check cmdline of the target PID and its ancestors for session ID
    if let Some(detected) = find_session_from_ancestor_cmdline(pid) {
        if provider_filter.is_none() || provider_filter == Some(detected.provider) {
            return Ok(detected);
        }
    }

    // Step 3: Detect provider from process name / ancestors
    let provider = if let Some(p) = provider_filter {
        p
    } else {
        detect_provider_from_pid(pid)
            .with_context(|| format!("Cannot determine AI provider from PID {}", pid))?
    };

    // Step 4: Determine working directory
    let cwd = get_process_cwd(pid)
        .or_else(|| get_dir_from_cmdline(pid))
        .with_context(|| format!("Cannot read working directory for PID {}", pid))?;

    // Step 5: Gather candidates
    let candidates = match provider {
        Provider::OpenCode => gather_opencode_candidates(&cwd)?,
        Provider::ClaudeCode => gather_claudecode_candidates(&cwd)?,
    };

    if candidates.is_empty() {
        bail!(
            "No sessions found for PID {} (directory: {})",
            pid,
            cwd.display()
        );
    }

    // Step 6: Single candidate → return directly
    if candidates.len() == 1 {
        return Ok(DetectedSession {
            session_id: candidates[0].session_id.clone(),
            provider,
        });
    }

    // Step 7: Fingerprint, then most-recent fallback
    let fingerprint = build_fingerprint();
    let mut matched: Vec<&Candidate> = Vec::new();

    for c in &candidates {
        if transcript_contains_fingerprint(&c.session_id, provider, &fingerprint) {
            matched.push(c);
        }
    }

    match matched.len() {
        1 => Ok(DetectedSession {
            session_id: matched[0].session_id.clone(),
            provider,
        }),
        0 => {
            // No fingerprint match — fall back to most recently updated
            let best = candidates
                .iter()
                .max_by_key(|c| c.updated_ms)
                .expect("candidates is non-empty");
            Ok(DetectedSession {
                session_id: best.session_id.clone(),
                provider,
            })
        }
        _ => {
            let mut msg = format!(
                "Ambiguous: {} sessions match for PID {}. Use --type to narrow:\n",
                matched.len(),
                pid
            );
            for c in &matched {
                msg.push_str(&format!("  {}\n", c.session_id));
            }
            bail!("{}", msg.trim_end());
        }
    }
}

#[cfg(not(unix))]
pub fn find_session_by_pid(
    _pid: u32,
    _provider_filter: Option<Provider>,
) -> Result<DetectedSession> {
    bail!("--pid is only supported on Unix/Linux systems");
}

/// Read env vars from `/proc/<pid>/environ` looking for session IDs.
#[cfg(unix)]
fn check_process_env_vars(pid: u32) -> Option<DetectedSession> {
    let environ = fs::read_to_string(format!("/proc/{}/environ", pid)).ok()?;
    // environ is NUL-separated
    for entry in environ.split('\0') {
        if let Some(val) = entry.strip_prefix("OPENCODE_SESSION_ID=") {
            if !val.is_empty() {
                return Some(DetectedSession {
                    session_id: val.to_string(),
                    provider: Provider::OpenCode,
                });
            }
        }
        if let Some(val) = entry.strip_prefix("CLAUDE_SESSION_ID=") {
            if !val.is_empty() {
                return Some(DetectedSession {
                    session_id: val.to_string(),
                    provider: Provider::ClaudeCode,
                });
            }
        }
    }
    None
}

/// Detect AI provider by checking the process name at `pid` and walking up.
#[cfg(unix)]
fn detect_provider_from_pid(pid: u32) -> Result<Provider> {
    // Check the process itself first
    if let Some(name) = get_process_name(pid) {
        match name.as_str() {
            "opencode" => return Ok(Provider::OpenCode),
            "claude" => return Ok(Provider::ClaudeCode),
            _ => {}
        }
    }

    // Walk up the process tree
    let mut current = pid;
    for _ in 0..20 {
        let ppid = match get_parent_pid(current) {
            Some(p) if p > 1 => p,
            _ => break,
        };
        if let Some(name) = get_process_name(ppid) {
            match name.as_str() {
                "opencode" => return Ok(Provider::OpenCode),
                "claude" => return Ok(Provider::ClaudeCode),
                _ => {}
            }
        }
        current = ppid;
    }

    bail!(
        "Cannot determine AI provider from PID {} or its ancestors",
        pid
    );
}

/// Read `/proc/<pid>/cwd` symlink.
#[cfg(unix)]
fn get_process_cwd(pid: u32) -> Option<std::path::PathBuf> {
    fs::read_link(format!("/proc/{}/cwd", pid)).ok()
}

/// Try to extract `--dir <path>` from `/proc/<pid>/cmdline`.
///
/// OpenCode uses `opencode attach <url> --dir <path>`.
#[cfg(unix)]
fn get_dir_from_cmdline(pid: u32) -> Option<std::path::PathBuf> {
    let raw = fs::read_to_string(format!("/proc/{}/cmdline", pid)).ok()?;
    let args: Vec<&str> = raw.split('\0').collect();
    // Look for --dir followed by a path
    for window in args.windows(2) {
        if window[0] == "--dir" && !window[1].is_empty() {
            return Some(std::path::PathBuf::from(window[1]));
        }
    }
    None
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

/// Try to extract `-s <session_id>` or `--session <session_id>` from
/// `/proc/<pid>/cmdline`.
///
/// OpenCode passes the session ID as `opencode -s ses_xxx` or
/// `opencode attach <url> -s ses_xxx`.
#[cfg(unix)]
fn get_session_from_cmdline(pid: u32) -> Option<String> {
    let raw = fs::read_to_string(format!("/proc/{}/cmdline", pid)).ok()?;
    parse_session_from_cmdline_args(&raw)
}

/// Walk the process tree upward (starting from the given PID itself)
/// looking for an opencode/claude process whose cmdline contains a
/// `-s`/`--session` flag with the session ID.
///
/// This is more authoritative than fingerprinting: the session ID is
/// directly available in the process's cmdline.
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
    let session_id = get_session_from_cmdline(pid)?;
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

    // Step 7: Fingerprint — find our own invocation in candidates' transcripts
    let fingerprint = build_fingerprint();
    let mut matched: Vec<&Candidate> = Vec::new();

    for c in &candidates {
        if transcript_contains_fingerprint(&c.session_id, provider, &fingerprint) {
            matched.push(c);
        }
    }

    match matched.len() {
        1 => Ok(DetectedSession {
            session_id: matched[0].session_id.clone(),
            provider,
        }),
        0 => {
            // No fingerprint match — fall back to most recently updated
            // This can happen if the transcript hasn't been flushed yet
            let best = candidates
                .iter()
                .max_by_key(|c| c.updated_ms)
                .expect("candidates is non-empty");
            Ok(DetectedSession {
                session_id: best.session_id.clone(),
                provider,
            })
        }
        _ => {
            // Multiple matches — report ambiguity
            let mut msg = format!(
                "Ambiguous: {} sessions match. Use --session <id> to specify:\n",
                matched.len()
            );
            for c in &matched {
                msg.push_str(&format!("  {}\n", c.session_id));
            }
            bail!("{}", msg.trim_end());
        }
    }
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
/// Reads `storage/session/<hash>/ses_*.json`, filters to:
/// - Non-child sessions (no `parentID`)
/// - Matching directory
/// - Sorted by `time.updated` descending
fn gather_opencode_candidates(cwd: &Path) -> Result<Vec<Candidate>> {
    let session_base = crate::opencode_data_dir().join("storage/session");
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

    // Sort by most recently updated first
    candidates.sort_by(|a, b| b.updated_ms.cmp(&a.updated_ms));
    Ok(candidates)
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

/// Build a fingerprint string to search for in transcripts.
///
/// We look for our own invocation: a bash tool call containing "ai-audit".
/// The process args give us the exact command that was run.
fn build_fingerprint() -> String {
    // Use the program name as a simple fingerprint.
    // The transcript will contain something like "ai-audit transcript ..."
    // in a bash tool_use part.
    "ai-audit".to_string()
}

/// Check if a session's transcript contains our fingerprint
/// in a recent tool call.
///
/// For OpenCode: look in the last few parts for a "tool" type
///   with tool="bash" or tool="Bash" whose input.command contains the fingerprint.
/// For Claude Code: look in the last few JSONL entries for a tool_use
///   block with name="Bash" whose input.command contains the fingerprint.
fn transcript_contains_fingerprint(
    session_id: &str,
    provider: Provider,
    fingerprint: &str,
) -> bool {
    match provider {
        Provider::OpenCode => opencode_transcript_has_fingerprint(session_id, fingerprint),
        Provider::ClaudeCode => claudecode_transcript_has_fingerprint(session_id, fingerprint),
    }
}

/// Check OpenCode session transcript for a recent bash tool call
/// containing the fingerprint.
fn opencode_transcript_has_fingerprint(session_id: &str, fingerprint: &str) -> bool {
    let storage_dir = crate::opencode_data_dir().join("storage");
    let message_dir = storage_dir.join("message").join(session_id);
    let part_dir = storage_dir.join("part");

    if !message_dir.exists() {
        return false;
    }

    // Get the last few messages (sorted by filename = chronological)
    let mut msg_files: Vec<_> = fs::read_dir(&message_dir)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "json"))
        .collect();
    msg_files.sort_by_key(|e| e.file_name());

    // Only check the last 5 messages
    let recent_msgs: Vec<_> = msg_files.iter().rev().take(5).collect();

    for msg_entry in &recent_msgs {
        let msg_id = msg_entry
            .path()
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();

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
            let raw = match fs::read_to_string(part_entry.path()) {
                Ok(c) => c,
                Err(_) => continue,
            };

            // Fast pre-filter
            if !raw.contains(fingerprint) {
                continue;
            }

            let part: serde_json::Value = match serde_json::from_str(&raw) {
                Ok(v) => v,
                Err(_) => continue,
            };

            // Check for tool type with bash command containing fingerprint
            if part.get("type").and_then(|v| v.as_str()) == Some("tool") {
                let tool_name = part.get("tool").and_then(|v| v.as_str()).unwrap_or("");
                if tool_name.eq_ignore_ascii_case("bash") {
                    if let Some(input) = part
                        .get("state")
                        .and_then(|s| s.get("input"))
                        .and_then(|i| i.get("command"))
                        .and_then(|c| c.as_str())
                    {
                        if input.contains(fingerprint) {
                            return true;
                        }
                    }
                }
            }
        }
    }

    false
}

/// Check Claude Code session transcript for a recent bash tool call
/// containing the fingerprint.
fn claudecode_transcript_has_fingerprint(session_id: &str, fingerprint: &str) -> bool {
    let session_file = match crate::claudecode::session::find_session_file(session_id) {
        Some(f) => f,
        None => return false,
    };

    let content = match fs::read_to_string(&session_file) {
        Ok(c) => c,
        Err(_) => return false,
    };

    // Only check the last few lines (tool call should be recent)
    let lines: Vec<&str> = content.lines().collect();
    let recent_lines = &lines[lines.len().saturating_sub(10)..];

    for line in recent_lines {
        if line.trim().is_empty() || !line.contains(fingerprint) {
            continue;
        }

        let entry: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // Check assistant messages with tool_use blocks
        if entry.get("type").and_then(|v| v.as_str()) != Some("assistant") {
            continue;
        }

        let content = match entry.get("message").and_then(|m| m.get("content")) {
            Some(c) => c,
            None => continue,
        };

        if let serde_json::Value::Array(blocks) = content {
            for block in blocks {
                if block.get("type").and_then(|v| v.as_str()) == Some("tool_use") {
                    let name = block.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    if name.eq_ignore_ascii_case("bash") {
                        if let Some(cmd) = block
                            .get("input")
                            .and_then(|i| i.get("command"))
                            .and_then(|c| c.as_str())
                        {
                            if cmd.contains(fingerprint) {
                                return true;
                            }
                        }
                    }
                }
            }
        }
    }

    false
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

    // === Fingerprint tests ===

    #[test]
    fn test_opencode_fingerprint_match() {
        let temp = tempdir().unwrap();
        let storage = temp.path();

        // Create a session with a bash tool call containing "ai-audit"
        let message_dir = storage.join("message/ses_test1");
        let part_dir = storage.join("part");
        fs::create_dir_all(&message_dir).unwrap();

        let msg_id = "msg_001";
        fs::write(
            message_dir.join(format!("{}.json", msg_id)),
            r#"{"id":"msg_001","sessionID":"ses_test1","role":"assistant","time":{"created":1000}}"#,
        )
        .unwrap();

        let msg_part_dir = part_dir.join(msg_id);
        fs::create_dir_all(&msg_part_dir).unwrap();

        fs::write(
            msg_part_dir.join("prt_001.json"),
            r#"{"id":"prt_001","type":"tool","tool":"bash","state":{"status":"running","input":{"command":"ai-audit transcript"}}}"#,
        ).unwrap();

        let result = opencode_transcript_has_fingerprint_in(storage, "ses_test1", "ai-audit");
        assert!(result);
    }

    #[test]
    fn test_opencode_fingerprint_no_match() {
        let temp = tempdir().unwrap();
        let storage = temp.path();

        // Create a session with an unrelated tool call
        let message_dir = storage.join("message/ses_test2");
        let part_dir = storage.join("part");
        fs::create_dir_all(&message_dir).unwrap();

        let msg_id = "msg_001";
        fs::write(
            message_dir.join(format!("{}.json", msg_id)),
            r#"{"id":"msg_001","sessionID":"ses_test2","role":"assistant","time":{"created":1000}}"#,
        )
        .unwrap();

        let msg_part_dir = part_dir.join(msg_id);
        fs::create_dir_all(&msg_part_dir).unwrap();

        fs::write(
            msg_part_dir.join("prt_001.json"),
            r#"{"id":"prt_001","type":"tool","tool":"bash","state":{"status":"running","input":{"command":"cargo test"}}}"#,
        ).unwrap();

        let result = opencode_transcript_has_fingerprint_in(storage, "ses_test2", "ai-audit");
        assert!(!result);
    }

    #[test]
    fn test_opencode_fingerprint_only_checks_bash_tools() {
        let temp = tempdir().unwrap();
        let storage = temp.path();

        // Create a session with a text part containing "ai-audit" (not a tool call)
        let message_dir = storage.join("message/ses_test3");
        let part_dir = storage.join("part");
        fs::create_dir_all(&message_dir).unwrap();

        let msg_id = "msg_001";
        fs::write(
            message_dir.join(format!("{}.json", msg_id)),
            r#"{"id":"msg_001","sessionID":"ses_test3","role":"assistant","time":{"created":1000}}"#,
        )
        .unwrap();

        let msg_part_dir = part_dir.join(msg_id);
        fs::create_dir_all(&msg_part_dir).unwrap();

        fs::write(
            msg_part_dir.join("prt_001.json"),
            r#"{"id":"prt_001","type":"text","text":"Let me run ai-audit for you"}"#,
        )
        .unwrap();

        let result = opencode_transcript_has_fingerprint_in(storage, "ses_test3", "ai-audit");
        assert!(!result);
    }

    #[test]
    fn test_claudecode_fingerprint_match() {
        let temp = tempdir().unwrap();
        let session_file = temp.path().join("session.jsonl");

        let line = r#"{"type":"assistant","timestamp":"2024-01-15T10:30:00.000Z","message":{"role":"assistant","content":[{"type":"tool_use","name":"Bash","input":{"command":"ai-audit transcript abc123"}}]}}"#;
        fs::write(&session_file, format!("{}\n", line)).unwrap();

        let result = claudecode_transcript_has_fingerprint_in(&session_file, "ai-audit");
        assert!(result);
    }

    #[test]
    fn test_claudecode_fingerprint_no_match() {
        let temp = tempdir().unwrap();
        let session_file = temp.path().join("session.jsonl");

        let line = r#"{"type":"assistant","timestamp":"2024-01-15T10:30:00.000Z","message":{"role":"assistant","content":[{"type":"tool_use","name":"Bash","input":{"command":"cargo build"}}]}}"#;
        fs::write(&session_file, format!("{}\n", line)).unwrap();

        let result = claudecode_transcript_has_fingerprint_in(&session_file, "ai-audit");
        assert!(!result);
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

    fn opencode_transcript_has_fingerprint_in(
        storage_dir: &Path,
        session_id: &str,
        fingerprint: &str,
    ) -> bool {
        let message_dir = storage_dir.join("message").join(session_id);
        let part_dir = storage_dir.join("part");

        if !message_dir.exists() {
            return false;
        }

        let mut msg_files: Vec<_> = fs::read_dir(&message_dir)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "json"))
            .collect();
        msg_files.sort_by_key(|e| e.file_name());

        let recent_msgs: Vec<_> = msg_files.iter().rev().take(5).collect();

        for msg_entry in &recent_msgs {
            let msg_id = msg_entry
                .path()
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();

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
                let raw = match fs::read_to_string(part_entry.path()) {
                    Ok(c) => c,
                    Err(_) => continue,
                };

                if !raw.contains(fingerprint) {
                    continue;
                }

                let part: serde_json::Value = match serde_json::from_str(&raw) {
                    Ok(v) => v,
                    Err(_) => continue,
                };

                if part.get("type").and_then(|v| v.as_str()) == Some("tool") {
                    let tool_name = part.get("tool").and_then(|v| v.as_str()).unwrap_or("");
                    if tool_name.eq_ignore_ascii_case("bash") {
                        if let Some(input) = part
                            .get("state")
                            .and_then(|s| s.get("input"))
                            .and_then(|i| i.get("command"))
                            .and_then(|c| c.as_str())
                        {
                            if input.contains(fingerprint) {
                                return true;
                            }
                        }
                    }
                }
            }
        }

        false
    }

    // === PID-based detection tests ===

    #[test]
    fn test_get_process_cwd_current_process() {
        // Current process has a valid cwd
        let pid = std::process::id();
        let cwd = get_process_cwd(pid);
        assert!(cwd.is_some());
        // Should match env::current_dir()
        let expected = env::current_dir().unwrap();
        assert_eq!(cwd.unwrap(), expected);
    }

    #[test]
    fn test_get_process_cwd_nonexistent_pid() {
        // PID 999999999 almost certainly doesn't exist
        let cwd = get_process_cwd(999_999_999);
        assert!(cwd.is_none());
    }

    #[test]
    fn test_check_process_env_vars_current_process() {
        // Current process likely doesn't have OPENCODE_SESSION_ID set
        // (it's not passed to subprocesses by opencode)
        let result = check_process_env_vars(std::process::id());
        // We don't assert true/false — just that it doesn't crash
        // If OPENCODE_SESSION_ID is in env, it should return Some
        if env::var("OPENCODE_SESSION_ID")
            .ok()
            .is_some_and(|v| !v.is_empty())
        {
            assert!(result.is_some());
        }
    }

    #[test]
    fn test_check_process_env_vars_nonexistent_pid() {
        let result = check_process_env_vars(999_999_999);
        assert!(result.is_none());
    }

    #[test]
    fn test_get_dir_from_cmdline_nonexistent_pid() {
        let result = get_dir_from_cmdline(999_999_999);
        assert!(result.is_none());
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

    #[test]
    fn test_get_session_from_cmdline_nonexistent_pid() {
        let result = get_session_from_cmdline(999_999_999);
        assert!(result.is_none());
    }

    #[test]
    fn test_find_session_from_ancestor_cmdline_nonexistent_pid() {
        let result = find_session_from_ancestor_cmdline(999_999_999);
        assert!(result.is_none());
    }

    #[test]
    fn test_detect_provider_from_pid_nonexistent() {
        let result = detect_provider_from_pid(999_999_999);
        assert!(result.is_err());
    }

    #[test]
    fn test_find_session_by_pid_nonexistent() {
        let result = find_session_by_pid(999_999_999, None);
        assert!(result.is_err());
    }

    fn claudecode_transcript_has_fingerprint_in(session_file: &Path, fingerprint: &str) -> bool {
        let content = match fs::read_to_string(session_file) {
            Ok(c) => c,
            Err(_) => return false,
        };

        let lines: Vec<&str> = content.lines().collect();
        let recent_lines = &lines[lines.len().saturating_sub(10)..];

        for line in recent_lines {
            if line.trim().is_empty() || !line.contains(fingerprint) {
                continue;
            }

            let entry: serde_json::Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(_) => continue,
            };

            if entry.get("type").and_then(|v| v.as_str()) != Some("assistant") {
                continue;
            }

            let content = match entry.get("message").and_then(|m| m.get("content")) {
                Some(c) => c,
                None => continue,
            };

            if let serde_json::Value::Array(blocks) = content {
                for block in blocks {
                    if block.get("type").and_then(|v| v.as_str()) == Some("tool_use") {
                        let name = block.get("name").and_then(|v| v.as_str()).unwrap_or("");
                        if name.eq_ignore_ascii_case("bash") {
                            if let Some(cmd) = block
                                .get("input")
                                .and_then(|i| i.get("command"))
                                .and_then(|c| c.as_str())
                            {
                                if cmd.contains(fingerprint) {
                                    return true;
                                }
                            }
                        }
                    }
                }
            }
        }

        false
    }
}

//! Auto-detect the current AI session that spawned this process.
//!
//! Detection strategy:
//! 1. Check env vars for authoritative session ID (future-proof)
//! 2. Detect provider from env + process tree
//! 3. Gather candidate sessions for the current working directory
//! 4. Filter to non-child, recently updated sessions
//! 5. Fingerprint: check each candidate's transcript for our own
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

    // Step 2: Detect provider
    let provider = detect_provider()?;

    // Step 3: Get current working directory
    let cwd = env::current_dir().context("Failed to get current directory")?;

    // Step 4: Gather candidates
    let candidates = match provider {
        Provider::OpenCode => gather_opencode_candidates(&cwd)?,
        Provider::ClaudeCode => gather_claudecode_candidates(&cwd)?,
    };

    if candidates.is_empty() {
        bail!("No sessions found for directory: {}", cwd.display());
    }

    // Step 5: If only one candidate, return it directly
    if candidates.len() == 1 {
        return Ok(DetectedSession {
            session_id: candidates[0].session_id.clone(),
            provider,
        });
    }

    // Step 6: Fingerprint — find our own invocation in candidates' transcripts
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

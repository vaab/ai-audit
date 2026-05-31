//! Permission events for an OpenCode session.
//!
//! Joins two sources:
//!
//! 1. **Tool-call attempts** — reused verbatim from
//!    [`crate::opencode::transcript::parse_transcript`] (filtered to
//!    `EntryType::ToolUse`).  This is intentional: tool calls live in
//!    message parts, and the transcript parser is the canonical,
//!    session-scoped, indexed reader for those parts.  We must NOT
//!    re-implement part scanning here — earlier iterations of this
//!    file did, and degraded a sub-second lookup into a 35 s
//!    full-disk crawl (50 k+ `openat` calls under `storage/part/`).
//!
//! 2. **Permission decisions** — `service=permission ... evaluated`
//!    log lines emitted by the running opencode process into
//!    `~/.local/share/opencode/log/*.log`.  These are NOT message
//!    parts; they are a genuinely separate source.
//!
//! Join key: `tool_name` + `pattern` + a 5 s timestamp window.
//!
//! ## Cost discipline
//!
//! - Tool calls: O(parts_for_session) via the `part_session_idx`
//!   SQLite index (transcript parser handles this).
//! - Log decisions: O(bytes_in_relevant_log_files), where "relevant"
//!   is bounded by the [tool_call_min - 5 s, tool_call_max + 5 s]
//!   window AND further reduced by an mtime-keyed on-disk cache
//!   (rotated logs are parsed once, ever; only the currently-active
//!   log is re-scanned each call).
//!
//! Empirically the second cost dominates because logs accumulate
//! indefinitely (multi-GB).  The cache is what keeps `aa s perm`
//! fast on repeat invocations against the same machine.

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::transcript::EntryType;
use crate::OutputFormat;

#[derive(Debug, Clone, Serialize)]
pub struct PermissionEvent {
    pub timestamp: DateTime<Utc>,
    pub tool: String,
    pub pattern: String,
    pub action: String,
}

/// Time tolerance for joining a tool-call timestamp to a log decision.
///
/// Permission decisions are emitted by opencode immediately around
/// the tool-call start time, but the two timestamps come from
/// different code paths (one from `state.time.start`, one from the
/// log writer's wall-clock) so we allow a small drift.
const JOIN_TOLERANCE: chrono::Duration = chrono::Duration::seconds(5);

/// Build permission events for `session_id`.
///
/// Architecture: see module docs.  This function is a thin overlay —
/// it pulls tool calls from the transcript and joins them with cached
/// log decisions.  It MUST stay thin; do not re-derive tool calls
/// from message parts here.
pub fn parse_events(session_id: &str) -> Result<Vec<PermissionEvent>> {
    // (1) Tool calls: reuse the transcript extractor.  Session-scoped
    //     and indexed; the slowness of the old per-part scan came
    //     from NOT doing this.
    let transcript = crate::opencode::transcript::parse_transcript(session_id).unwrap_or_default();

    let tool_calls: Vec<(DateTime<Utc>, String, String)> = transcript
        .into_iter()
        .filter(|e| matches!(e.entry_type, EntryType::ToolUse))
        .filter_map(|e| {
            let tool = e.tool_name?;
            let pattern = extract_pattern(&tool, &e.tool_input);
            Some((e.timestamp, tool, pattern))
        })
        .collect();

    if tool_calls.is_empty() {
        return Ok(Vec::new());
    }

    // (2) Log decisions: bounded scan + per-file mtime cache.
    let (t_min, t_max) = tool_call_window(&tool_calls);
    let log_decisions = load_log_decisions_cached(&log_dir(), t_min, t_max)?;

    let mut events: Vec<PermissionEvent> = tool_calls
        .into_iter()
        .map(|(timestamp, tool, pattern)| {
            let action = find_permission_decision(&log_decisions, &tool, &pattern, timestamp)
                .unwrap_or_else(|| "unknown".to_string());
            PermissionEvent {
                timestamp,
                tool,
                pattern,
                action,
            }
        })
        .collect();

    events.sort_by_key(|e| e.timestamp);
    Ok(events)
}

fn log_dir() -> PathBuf {
    super::log_dir()
}

/// Compute the [min, max] timestamp envelope of the tool calls,
/// padded by [`JOIN_TOLERANCE`] on each side so we don't miss a
/// decision logged slightly before/after its tool call.
fn tool_call_window(
    tool_calls: &[(DateTime<Utc>, String, String)],
) -> (DateTime<Utc>, DateTime<Utc>) {
    // Caller guarantees non-empty before invoking us, but defend
    // anyway — `min`/`max` on an empty slice would panic.
    debug_assert!(!tool_calls.is_empty());

    let mut min = tool_calls[0].0;
    let mut max = tool_calls[0].0;
    for (ts, _, _) in tool_calls.iter().skip(1) {
        if *ts < min {
            min = *ts;
        }
        if *ts > max {
            max = *ts;
        }
    }
    (min - JOIN_TOLERANCE, max + JOIN_TOLERANCE)
}

fn extract_pattern(tool: &str, input: &Option<serde_json::Value>) -> String {
    let input = match input {
        Some(v) => v,
        None => return String::new(),
    };

    match tool {
        "bash" => input
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        "read" | "write" | "edit" => input
            .get("filePath")
            .or_else(|| input.get("file_path"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        "glob" | "grep" => input
            .get("pattern")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        _ => serde_json::to_string(input).unwrap_or_default(),
    }
}

// =====================================================================
// Log-decision overlay
// =====================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LogDecision {
    timestamp: DateTime<Utc>,
    permission: String,
    pattern: String,
    action: String,
}

/// Regex matching the `service=permission ... evaluated` log lines.
///
/// Lazy-compiled once per process via `OnceLock`.  Captures:
///   1. RFC3339-ish timestamp prefix (no TZ — opencode writes UTC).
///   2. `permission=<tool>`
///   3. `pattern=<glob-or-cmd>` (greedy up to ` action=`)
///   4. `"action":"<allow|ask|deny>"` from the action JSON object.
fn decision_regex() -> &'static Regex {
    use std::sync::OnceLock;
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"^INFO\s+(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}).*service=permission permission=(\w+) pattern=(.+?) action=\{[^}]*"action":"(\w+)"[^}]*\} evaluated"#,
        )
        .expect("hard-coded regex must compile")
    })
}

/// Load all relevant log decisions, with **incremental** on-disk cache.
///
/// "Relevant" = log files whose filename-timestamp falls within the
/// `[t_min - 1h, t_max + 1h]` window AND whose mtime is at or after
/// `t_min` (so we skip files that stopped being written before our
/// earliest tool call).
///
/// The cache is INCREMENTAL: one cache file per log file, holding
/// the parsed decisions up to a known byte offset.  On the next
/// call we seek to that offset and parse only the newly-appended
/// bytes.  This is the only design that works for opencode's
/// long-lived "active" log files (currently 14 GB and growing) —
/// re-parsing the whole file every call is exactly the regression
/// the rewrite is meant to fix.
///
/// Rotation / truncation is detected by `current_len < cached_len`:
/// when that happens we discard the cache entry and reparse from
/// byte 0.
fn load_log_decisions_cached(
    log_dir: &Path,
    t_min: DateTime<Utc>,
    t_max: DateTime<Utc>,
) -> Result<Vec<LogDecision>> {
    if !log_dir.exists() {
        return Ok(Vec::new());
    }

    let cache_root = log_decisions_cache_dir();
    let cache_ok = match &cache_root {
        Ok(dir) => fs::create_dir_all(dir).is_ok(),
        Err(_) => false,
    };

    let mut all = Vec::new();
    for path in select_log_files(log_dir, t_min, t_max)? {
        let decisions = if cache_ok {
            let cache_path = cache_root.as_ref().unwrap().join(cache_key(&path));
            load_log_decisions_incremental(&path, &cache_path).unwrap_or_else(|err| {
                log::debug!(
                    "incremental cache failed for {}: {err}; falling back to full reparse",
                    path.display()
                );
                parse_log_file(&path).unwrap_or_default()
            })
        } else {
            parse_log_file(&path).unwrap_or_default()
        };

        // Time-filter decisions to the requested window.  Keeps the
        // merged vector small even when a cache holds a long-lived
        // log's worth of entries.
        for d in decisions {
            if d.timestamp >= t_min && d.timestamp <= t_max {
                all.push(d);
            }
        }
    }

    Ok(all)
}

/// Cache root: `~/.cache/ai-audit/opencode/log-decisions/`.
fn log_decisions_cache_dir() -> Result<PathBuf> {
    let base =
        dirs::cache_dir().ok_or_else(|| anyhow!("Could not determine user cache directory"))?;
    Ok(base.join("ai-audit/opencode/log-decisions"))
}

/// On-disk schema for the incremental cache.
///
/// `last_offset` is the byte offset at which the next parse should
/// resume.  It MUST point at a line boundary — `parse_log_tail`
/// guarantees this by stopping before the final newline character
/// it has processed.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedLogState {
    /// Schema version.  Bump if the on-disk shape changes.
    #[serde(default = "default_schema_version")]
    schema_version: u32,
    /// Decisions parsed from `[0, last_offset)` of the source log.
    decisions: Vec<LogDecision>,
    /// Next byte offset to resume from.  See type docs.
    last_offset: u64,
}

fn default_schema_version() -> u32 {
    1
}
const CACHE_SCHEMA_VERSION: u32 = 1;

/// Slack between a log file's filename timestamp and the timestamps
/// of events inside it.  Used as a padding factor when deciding
/// which log files to scan: a file with `filename_ts <= t_max + SLACK`
/// is kept.  An hour is comfortable: opencode never opens a log a
/// full hour before writing the first event.
const LOG_FILENAME_SLACK: chrono::Duration = chrono::Duration::hours(1);

/// Cache filename for a log file.  Pure function of the path —
/// stable across calls so the same log keeps the same cache file.
fn cache_key(path: &Path) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(path.to_string_lossy().as_bytes());
    let hash = hasher.finalize();
    format!("{}.json", &hash.to_hex().as_str()[..16])
}

fn read_cached_state(path: &Path) -> Option<CachedLogState> {
    let bytes = fs::read(path).ok()?;
    let state: CachedLogState = serde_json::from_slice(&bytes).ok()?;
    if state.schema_version != CACHE_SCHEMA_VERSION {
        return None;
    }
    Some(state)
}

fn write_cached_state(path: &Path, state: &CachedLogState) -> Result<()> {
    let temp = path.with_extension("json.tmp");
    {
        let mut f = fs::File::create(&temp)
            .with_context(|| format!("Failed to create cache temp file: {}", temp.display()))?;
        let bytes = serde_json::to_vec(state).context("Failed to serialize log cache state")?;
        f.write_all(&bytes)?;
        f.sync_all()?;
    }
    fs::rename(&temp, path)
        .with_context(|| format!("Failed to rename cache file: {}", path.display()))?;
    Ok(())
}

/// Incremental decision loader.
///
/// - On first call (no cache file): parse the entire log, persist
///   `{decisions, last_offset = file_len}`.
/// - On subsequent calls: read the cached `{decisions, last_offset}`,
///   seek to `last_offset` in the log, parse the tail, append new
///   decisions, update `last_offset = file_len`, persist.
/// - Rotation/truncation (`file_len < last_offset`): discard the
///   cache and reparse from byte 0.
///
/// Returns the FULL decision list for the file (including pre-cached
/// portion).
fn load_log_decisions_incremental(log_path: &Path, cache_path: &Path) -> Result<Vec<LogDecision>> {
    let meta = fs::metadata(log_path)
        .with_context(|| format!("Failed to stat log file: {}", log_path.display()))?;
    let file_len = meta.len();

    let (mut decisions, start_offset) = match read_cached_state(cache_path) {
        Some(state) if state.last_offset <= file_len => (state.decisions, state.last_offset),
        // No cache, schema mismatch, or rotation/truncation:
        // discard and reparse from 0.
        _ => (Vec::new(), 0u64),
    };

    if start_offset < file_len {
        let (tail_decisions, parsed_to) = parse_log_tail(log_path, start_offset, file_len)?;
        decisions.extend(tail_decisions);

        let new_state = CachedLogState {
            schema_version: CACHE_SCHEMA_VERSION,
            decisions: decisions.clone(),
            last_offset: parsed_to,
        };
        // Best-effort persist — failure leaves us slow next call but correct.
        let _ = write_cached_state(cache_path, &new_state);
    }

    Ok(decisions)
}

/// Parse a tail slice `[start, end_hint)` of a log file.
///
/// Returns `(new_decisions, parsed_through_offset)`.
/// `parsed_through_offset` is the byte position immediately AFTER
/// the last complete line consumed — line-boundary aligned, so it
/// is safe to resume from on the next call.
///
/// We stop one line short of EOF when the file does NOT end with a
/// newline (last line may still be growing as opencode writes it).
fn parse_log_tail(path: &Path, start: u64, end_hint: u64) -> Result<(Vec<LogDecision>, u64)> {
    let mut file = fs::File::open(path)
        .with_context(|| format!("Failed to open log file: {}", path.display()))?;
    file.seek(SeekFrom::Start(start))
        .with_context(|| format!("Failed to seek to {start} in {}", path.display()))?;

    let mut reader = BufReader::new(file);
    let re = decision_regex();
    let mut decisions = Vec::new();
    let mut consumed = start;
    let mut last_complete_line_end = start;

    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            break;
        }
        consumed += n as u64;
        let ends_with_newline = line.ends_with('\n');
        if ends_with_newline {
            last_complete_line_end = consumed;
        } else {
            // Partial trailing line: stop here, don't advance the
            // "safe to resume from" marker past it.
            break;
        }

        // Pre-filter: skip lines without the marker before running the regex.
        if !line.contains("service=permission") {
            continue;
        }
        if let Some(caps) = re.captures(line.trim_end_matches('\n')) {
            let ts_str = &caps[1];
            if let Ok(ts) =
                DateTime::parse_from_str(&format!("{}+00:00", ts_str), "%Y-%m-%dT%H:%M:%S%:z")
            {
                decisions.push(LogDecision {
                    timestamp: ts.with_timezone(&Utc),
                    permission: caps[2].to_string(),
                    pattern: caps[3].to_string(),
                    action: caps[4].to_string(),
                });
            }
        }

        // If the caller hinted an end and we've crossed it after a
        // full line, stop.  This is just a sanity bound — we trust
        // EOF more than the hint.
        if consumed >= end_hint {
            break;
        }
    }

    Ok((decisions, last_complete_line_end))
}

/// Pick the log files whose contents could plausibly contain
/// decisions in `[t_min, t_max]`.
///
/// Heuristic: each log filename is itself a timestamp
/// (`YYYY-MM-DDTHHMMSS.log`) representing the moment the opencode
/// process opened it.  A log file is RELEVANT when:
///
/// - `filename_ts <= t_max + ACTIVE_LOG_WINDOW`, AND
/// - `mtime >= t_min` (the file kept getting written to at least
///   until `t_min`).
///
/// Files we cannot parse the filename of are kept (safer to scan
/// extra than to miss).
fn select_log_files(
    log_dir: &Path,
    t_min: DateTime<Utc>,
    t_max: DateTime<Utc>,
) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for entry in fs::read_dir(log_dir)?.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "log") {
            continue;
        }

        // mtime gate: skip files that stopped being written before
        // the earliest tool call we care about.
        if let Ok(meta) = entry.metadata() {
            if let Ok(mtime) = meta.modified() {
                if let Ok(d) = mtime.duration_since(SystemTime::UNIX_EPOCH) {
                    if let Some(mtime_dt) =
                        chrono::Utc.timestamp_opt(d.as_secs() as i64, 0).single()
                    {
                        if mtime_dt < t_min {
                            continue;
                        }
                    }
                }
            }
        }

        // filename gate: if we can parse the filename timestamp,
        // skip files that opened AFTER our window ended (decisions
        // for our calls can't be in a file that didn't exist yet).
        if let Some(file_ts) = parse_log_filename_ts(&path) {
            // Pad by LOG_FILENAME_SLACK to be conservative — handles
            // rounding / clock skew between the filename and the
            // events written into the file.
            if file_ts > t_max + LOG_FILENAME_SLACK {
                continue;
            }
        }

        out.push(path);
    }

    // Deterministic order so cached results merge identically across
    // invocations.
    out.sort();
    Ok(out)
}

/// Parse a log filename of the shape `YYYY-MM-DDTHHMMSS.log` into a
/// UTC `DateTime`.  Returns `None` when the filename doesn't match
/// the expected shape (in which case the caller should keep the
/// file — better extra work than a wrong skip).
fn parse_log_filename_ts(path: &Path) -> Option<DateTime<Utc>> {
    let stem = path.file_stem()?.to_str()?;
    let naive = NaiveDateTime::parse_from_str(stem, "%Y-%m-%dT%H%M%S").ok()?;
    Utc.from_utc_datetime(&naive).into()
}

fn parse_log_file(path: &Path) -> Result<Vec<LogDecision>> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("Failed to read log file: {}", path.display()))?;
    let re = decision_regex();
    let mut decisions = Vec::new();
    for line in content.lines() {
        // Cheap pre-filter: skip lines without the marker before
        // running the regex.  Logs are mostly noise; this avoids
        // millions of wasted regex evaluations on rotated files.
        if !line.contains("service=permission") {
            continue;
        }
        if let Some(caps) = re.captures(line) {
            let ts_str = &caps[1];
            if let Ok(ts) =
                DateTime::parse_from_str(&format!("{}+00:00", ts_str), "%Y-%m-%dT%H:%M:%S%:z")
            {
                decisions.push(LogDecision {
                    timestamp: ts.with_timezone(&Utc),
                    permission: caps[2].to_string(),
                    pattern: caps[3].to_string(),
                    action: caps[4].to_string(),
                });
            }
        }
    }
    Ok(decisions)
}

fn find_permission_decision(
    decisions: &[LogDecision],
    tool: &str,
    pattern: &str,
    timestamp: DateTime<Utc>,
) -> Option<String> {
    for decision in decisions {
        if decision.permission != tool {
            continue;
        }

        let time_diff = if decision.timestamp > timestamp {
            decision.timestamp - timestamp
        } else {
            timestamp - decision.timestamp
        };

        if time_diff > JOIN_TOLERANCE {
            continue;
        }

        // Patterns might be truncated or slightly different between
        // the log line and the recorded tool input — accept either
        // direction of prefix match in addition to exact equality.
        if decision.pattern == pattern
            || pattern.starts_with(&decision.pattern)
            || decision.pattern.starts_with(pattern)
        {
            return Some(decision.action.clone());
        }
    }

    None
}

// =====================================================================
// Display
// =====================================================================

pub fn display_events(events: &[PermissionEvent], format: OutputFormat) {
    match format {
        OutputFormat::Json => display_json(events),
        OutputFormat::Nul => display_nul(events),
        OutputFormat::Human => display_human(events),
    }
}

fn display_json(events: &[PermissionEvent]) {
    for event in events {
        let ts = event.timestamp.timestamp() as f64
            + event.timestamp.timestamp_subsec_nanos() as f64 / 1_000_000_000.0;
        println!(
            r#"{{"timestamp":{},"tool":"{}","pattern":"{}","action":"{}"}}"#,
            ts,
            event.tool,
            event.pattern.replace('\\', "\\\\").replace('"', "\\\""),
            event.action
        );
    }
}

fn display_nul(events: &[PermissionEvent]) {
    use std::io::{self, Write};
    let stdout = io::stdout();
    let mut handle = stdout.lock();

    for event in events {
        let ts = event.timestamp.timestamp() as f64
            + event.timestamp.timestamp_subsec_nanos() as f64 / 1_000_000_000.0;
        let _ = write!(
            handle,
            "{}\t{}\t{}\t{}\0",
            ts, event.tool, event.pattern, event.action
        );
    }
}

fn display_human(events: &[PermissionEvent]) {
    for event in events {
        let action_display = match event.action.as_str() {
            "allow" => "ALLOW",
            "ask" => "ASK",
            "deny" => "DENY",
            _ => &event.action,
        };

        let pattern_short = if event.pattern.len() > 60 {
            format!("{}...", &event.pattern[..57])
        } else {
            event.pattern.clone()
        };

        println!(
            "{:<24} {:<8} {:<12} {}",
            event.timestamp.format("%Y-%m-%d %H:%M:%S"),
            action_display,
            event.tool,
            pattern_short
        );
    }
}

// =====================================================================
// Tests
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use indoc::indoc;
    use std::fs;
    use tempfile::tempdir;

    fn ts(ms: i64) -> DateTime<Utc> {
        Utc.timestamp_millis_opt(ms).unwrap()
    }

    // ---- extract_pattern -----------------------------------------

    #[test]
    fn extract_pattern_bash_uses_command_field() {
        let input = serde_json::json!({"command": "ls -la"});
        assert_eq!(extract_pattern("bash", &Some(input)), "ls -la");
    }

    #[test]
    fn extract_pattern_read_prefers_camelcase_file_path() {
        let input = serde_json::json!({"filePath": "/a/b.rs", "file_path": "/wrong.rs"});
        assert_eq!(extract_pattern("read", &Some(input)), "/a/b.rs");
    }

    #[test]
    fn extract_pattern_read_falls_back_to_snake_case() {
        let input = serde_json::json!({"file_path": "/legacy.rs"});
        assert_eq!(extract_pattern("read", &Some(input)), "/legacy.rs");
    }

    #[test]
    fn extract_pattern_unknown_tool_serializes_input() {
        let input = serde_json::json!({"foo": 1});
        let p = extract_pattern("mystery", &Some(input));
        assert!(p.contains("\"foo\":1"));
    }

    #[test]
    fn extract_pattern_none_input_returns_empty() {
        assert_eq!(extract_pattern("bash", &None), "");
    }

    // ---- tool_call_window ----------------------------------------

    #[test]
    fn tool_call_window_pads_by_tolerance() {
        let calls = vec![
            (ts(10_000), "bash".to_string(), "ls".to_string()),
            (ts(20_000), "bash".to_string(), "pwd".to_string()),
        ];
        let (lo, hi) = tool_call_window(&calls);
        assert_eq!(lo, ts(10_000) - JOIN_TOLERANCE);
        assert_eq!(hi, ts(20_000) + JOIN_TOLERANCE);
    }

    #[test]
    fn tool_call_window_single_call() {
        let calls = vec![(ts(5_000), "bash".to_string(), "ls".to_string())];
        let (lo, hi) = tool_call_window(&calls);
        assert_eq!(lo, ts(5_000) - JOIN_TOLERANCE);
        assert_eq!(hi, ts(5_000) + JOIN_TOLERANCE);
    }

    // ---- parse_log_file / regex ----------------------------------

    #[test]
    fn parse_log_file_picks_up_decision_line() {
        let temp = tempdir().unwrap();
        let log = temp.path().join("2026-05-30T000000.log");
        fs::write(
            &log,
            indoc! {r#"
                INFO 2026-05-30T00:00:01 some unrelated line
                INFO 2026-05-30T00:00:02 service=permission permission=bash pattern=ls -la action={"action":"allow"} evaluated
                INFO 2026-05-30T00:00:03 another unrelated line
            "#},
        )
        .unwrap();

        let decisions = parse_log_file(&log).unwrap();
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].permission, "bash");
        assert_eq!(decisions[0].pattern, "ls -la");
        assert_eq!(decisions[0].action, "allow");
    }

    #[test]
    fn parse_log_file_ignores_non_permission_lines_fast() {
        let temp = tempdir().unwrap();
        let log = temp.path().join("2026-05-30T000000.log");
        fs::write(
            &log,
            "INFO 2026-05-30T00:00:01 totally unrelated\n".repeat(100),
        )
        .unwrap();

        let decisions = parse_log_file(&log).unwrap();
        assert!(decisions.is_empty());
    }

    // ---- parse_log_filename_ts -----------------------------------

    #[test]
    fn parse_log_filename_ts_accepts_canonical_form() {
        let p = Path::new("/tmp/2026-05-30T013220.log");
        let parsed = parse_log_filename_ts(p).unwrap();
        assert_eq!(
            parsed.format("%Y-%m-%d %H:%M:%S").to_string(),
            "2026-05-30 01:32:20"
        );
    }

    #[test]
    fn parse_log_filename_ts_rejects_garbage() {
        assert!(parse_log_filename_ts(Path::new("/tmp/garbage.log")).is_none());
    }

    // ---- find_permission_decision --------------------------------

    #[test]
    fn find_permission_decision_matches_exact() {
        let d = vec![LogDecision {
            timestamp: ts(1_000_000),
            permission: "bash".to_string(),
            pattern: "ls -la".to_string(),
            action: "allow".to_string(),
        }];
        let action = find_permission_decision(&d, "bash", "ls -la", ts(1_000_000));
        assert_eq!(action.as_deref(), Some("allow"));
    }

    #[test]
    fn find_permission_decision_respects_tolerance() {
        let d = vec![LogDecision {
            timestamp: ts(1_000_000),
            permission: "bash".to_string(),
            pattern: "ls".to_string(),
            action: "allow".to_string(),
        }];
        // 10 seconds away → outside the 5 s window
        let action = find_permission_decision(&d, "bash", "ls", ts(11_000_000));
        assert_eq!(action, None);
    }

    #[test]
    fn find_permission_decision_prefix_match_either_direction() {
        let d = vec![LogDecision {
            timestamp: ts(1_000_000),
            permission: "bash".to_string(),
            pattern: "ls -la /tmp".to_string(),
            action: "allow".to_string(),
        }];
        // Tool pattern is a prefix of the log pattern → matches.
        let action = find_permission_decision(&d, "bash", "ls", ts(1_000_000));
        assert_eq!(action.as_deref(), Some("allow"));
    }

    // ---- log_decisions_cached + select_log_files -----------------

    #[test]
    fn select_log_files_skips_outside_window() {
        let temp = tempdir().unwrap();
        // log opened well before the window → kept (mtime gate would
        // also need to drop it, but here we only test the filename
        // gate by giving each file a recent mtime).
        let inside = temp.path().join("2026-05-30T120000.log");
        let outside = temp.path().join("2027-01-01T000000.log");
        fs::write(&inside, "x").unwrap();
        fs::write(&outside, "x").unwrap();

        let t_min = Utc.with_ymd_and_hms(2026, 5, 30, 11, 0, 0).unwrap();
        let t_max = Utc.with_ymd_and_hms(2026, 5, 30, 13, 0, 0).unwrap();

        let picked = select_log_files(temp.path(), t_min, t_max).unwrap();
        let names: Vec<_> = picked
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&"2026-05-30T120000.log".to_string()));
        assert!(!names.contains(&"2027-01-01T000000.log".to_string()));
    }

    #[test]
    fn cache_state_round_trip_preserves_data() {
        let temp = tempdir().unwrap();
        let cache_path = temp.path().join("cache.json");
        let state = CachedLogState {
            schema_version: CACHE_SCHEMA_VERSION,
            decisions: vec![LogDecision {
                timestamp: ts(1_000_000),
                permission: "bash".to_string(),
                pattern: "ls".to_string(),
                action: "allow".to_string(),
            }],
            last_offset: 4242,
        };
        write_cached_state(&cache_path, &state).unwrap();
        let round = read_cached_state(&cache_path).unwrap();
        assert_eq!(round.last_offset, 4242);
        assert_eq!(round.decisions.len(), 1);
        assert_eq!(round.decisions[0].permission, "bash");
    }

    #[test]
    fn cache_state_corrupt_file_returns_none() {
        let temp = tempdir().unwrap();
        let cache_path = temp.path().join("bad.json");
        fs::write(&cache_path, "not json").unwrap();
        assert!(read_cached_state(&cache_path).is_none());
    }

    #[test]
    fn cache_state_schema_mismatch_returns_none() {
        let temp = tempdir().unwrap();
        let cache_path = temp.path().join("v0.json");
        // Hand-crafted future-schema blob
        fs::write(
            &cache_path,
            r#"{"schema_version":99,"decisions":[],"last_offset":0}"#,
        )
        .unwrap();
        assert!(read_cached_state(&cache_path).is_none());
    }

    #[test]
    fn incremental_cache_cold_parses_full_file() {
        let temp = tempdir().unwrap();
        let log = temp.path().join("2026-05-30T000000.log");
        let cache = temp.path().join("cache.json");
        fs::write(
            &log,
            indoc! {r#"
                INFO 2026-05-30T00:00:01 service=permission permission=bash pattern=ls action={"action":"allow"} evaluated
                INFO 2026-05-30T00:00:02 service=permission permission=bash pattern=pwd action={"action":"deny"} evaluated
            "#},
        )
        .unwrap();

        let decisions = load_log_decisions_incremental(&log, &cache).unwrap();
        assert_eq!(decisions.len(), 2);
        assert_eq!(decisions[0].pattern, "ls");
        assert_eq!(decisions[1].pattern, "pwd");

        // Cache file was persisted with last_offset = full file length.
        let state = read_cached_state(&cache).unwrap();
        assert_eq!(state.last_offset, fs::metadata(&log).unwrap().len());
        assert_eq!(state.decisions.len(), 2);
    }

    #[test]
    fn incremental_cache_warm_parses_only_appended_tail() {
        let temp = tempdir().unwrap();
        let log = temp.path().join("2026-05-30T000000.log");
        let cache = temp.path().join("cache.json");

        // First write + cold parse
        fs::write(
            &log,
            "INFO 2026-05-30T00:00:01 service=permission permission=bash pattern=ls action={\"action\":\"allow\"} evaluated\n",
        )
        .unwrap();
        let first = load_log_decisions_incremental(&log, &cache).unwrap();
        assert_eq!(first.len(), 1);
        let first_offset = read_cached_state(&cache).unwrap().last_offset;

        // Append a new decision
        use std::io::Write as _;
        let mut f = fs::OpenOptions::new().append(true).open(&log).unwrap();
        writeln!(
            f,
            r#"INFO 2026-05-30T00:00:02 service=permission permission=read pattern=/tmp/x action={{"action":"ask"}} evaluated"#
        )
        .unwrap();
        drop(f);

        // Warm parse: should return ALL decisions (1 cached + 1 new),
        // and advance the offset.
        let second = load_log_decisions_incremental(&log, &cache).unwrap();
        assert_eq!(second.len(), 2);
        assert_eq!(second[0].pattern, "ls");
        assert_eq!(second[1].pattern, "/tmp/x");
        let second_offset = read_cached_state(&cache).unwrap().last_offset;
        assert!(
            second_offset > first_offset,
            "offset should advance after tail parse"
        );
    }

    #[test]
    fn incremental_cache_handles_partial_trailing_line() {
        let temp = tempdir().unwrap();
        let log = temp.path().join("2026-05-30T000000.log");
        let cache = temp.path().join("cache.json");

        // File without trailing newline: simulate a still-being-written line.
        fs::write(
            &log,
            "INFO 2026-05-30T00:00:01 service=permission permission=bash pattern=ls action={\"action\":\"allow\"} evaluated\nINFO 2026-05-30T00:00:02 service=permission permission=read pattern=/tmp/y action={\"action\":\"ask\"} eval",
        )
        .unwrap();

        let decisions = load_log_decisions_incremental(&log, &cache).unwrap();
        // Only the FIRST line is complete → only one decision parsed.
        assert_eq!(decisions.len(), 1);

        let state = read_cached_state(&cache).unwrap();
        // last_offset must align with the end of the FIRST line (not EOF).
        let first_line_end = "INFO 2026-05-30T00:00:01 service=permission permission=bash pattern=ls action={\"action\":\"allow\"} evaluated\n".len() as u64;
        assert_eq!(state.last_offset, first_line_end);

        // Now complete the second line + add a third
        use std::io::Write as _;
        let mut f = fs::OpenOptions::new().append(true).open(&log).unwrap();
        writeln!(f, "uated").unwrap();
        writeln!(
            f,
            r#"INFO 2026-05-30T00:00:03 service=permission permission=bash pattern=pwd action={{"action":"deny"}} evaluated"#
        )
        .unwrap();
        drop(f);

        let decisions = load_log_decisions_incremental(&log, &cache).unwrap();
        assert_eq!(decisions.len(), 3);
        assert_eq!(decisions[1].pattern, "/tmp/y");
        assert_eq!(decisions[2].pattern, "pwd");
    }

    #[test]
    fn incremental_cache_detects_truncation_and_reparses() {
        let temp = tempdir().unwrap();
        let log = temp.path().join("2026-05-30T000000.log");
        let cache = temp.path().join("cache.json");

        fs::write(
            &log,
            "INFO 2026-05-30T00:00:01 service=permission permission=bash pattern=ls action={\"action\":\"allow\"} evaluated\nINFO 2026-05-30T00:00:02 service=permission permission=bash pattern=pwd action={\"action\":\"deny\"} evaluated\n",
        )
        .unwrap();
        let initial = load_log_decisions_incremental(&log, &cache).unwrap();
        assert_eq!(initial.len(), 2);

        // Truncate / rotate: rewrite with a single shorter line.
        fs::write(
            &log,
            "INFO 2026-05-30T00:01:00 service=permission permission=read pattern=/etc/x action={\"action\":\"allow\"} evaluated\n",
        )
        .unwrap();

        let after = load_log_decisions_incremental(&log, &cache).unwrap();
        // Cache was discarded → only the new decision remains.
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].pattern, "/etc/x");
    }

    #[test]
    fn load_log_decisions_cached_filters_to_window() {
        let temp = tempdir().unwrap();
        let log = temp.path().join("2026-05-30T000000.log");
        fs::write(
            &log,
            indoc! {r#"
                INFO 2026-05-30T00:00:02 service=permission permission=bash pattern=ls action={"action":"allow"} evaluated
                INFO 2026-05-30T00:10:00 service=permission permission=bash pattern=pwd action={"action":"deny"} evaluated
            "#},
        )
        .unwrap();

        let t_min = Utc.with_ymd_and_hms(2026, 5, 30, 0, 0, 0).unwrap();
        let t_max = Utc.with_ymd_and_hms(2026, 5, 30, 0, 0, 5).unwrap();

        let decisions = load_log_decisions_cached(temp.path(), t_min, t_max).unwrap();
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].pattern, "ls");
    }
}

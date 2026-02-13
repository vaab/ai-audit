//! List sessions command handler.

use anyhow::Result;
use serde::Serialize;

use super::super::def::SessionType;
use crate::{claudecode, opencode, OutputFormat};

/// Session record for JSON/NUL output
#[derive(Debug, Serialize)]
struct SessionRecord {
    /// UTC timestamp as float seconds since epoch
    timestamp: f64,
    session_id: String,
    #[serde(rename = "type")]
    session_type: &'static str,
}

/// Parsed timespan bounds as UTC epoch seconds.
struct TimespanFilter {
    start: i64,
    end: i64,
}

impl TimespanFilter {
    fn contains(&self, timestamp_secs: f64) -> bool {
        let ts = timestamp_secs as i64;
        ts >= self.start && ts <= self.end
    }
}

pub fn run(
    session_type: Option<SessionType>,
    search: Option<&str>,
    timespan: Option<&str>,
    format: OutputFormat,
    quiet: bool,
) -> Result<()> {
    let ts_filter = match timespan {
        Some(ts_str) => {
            let (start, end) = kal_time::parse_timespan(ts_str)
                .map_err(|e| anyhow::anyhow!("Failed to parse timespan '{}': {}", ts_str, e))?;
            Some(TimespanFilter {
                start: start.timestamp(),
                end: end.timestamp(),
            })
        }
        None => None,
    };

    let mut sessions: Vec<SessionRecord> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    let include_claudecode = session_type.is_none_or(|t| matches!(t, SessionType::ClaudeCode));
    let include_opencode = session_type.is_none_or(|t| matches!(t, SessionType::OpenCode));

    if include_claudecode {
        match claudecode::session::list_sessions() {
            Ok(cc_sessions) => {
                for s in cc_sessions {
                    let ts = s.timestamp.timestamp() as f64
                        + s.timestamp.timestamp_subsec_nanos() as f64 / 1_000_000_000.0;
                    if let Some(ref filter) = ts_filter {
                        if !filter.contains(ts) {
                            continue;
                        }
                    }
                    if let Some(needle) = search {
                        if !claudecode::session::session_contains_text(&s.session_id, needle) {
                            continue;
                        }
                    }
                    sessions.push(SessionRecord {
                        timestamp: ts,
                        session_id: s.session_id,
                        session_type: "claudecode",
                    });
                }
            }
            Err(e) => {
                errors.push(format!("claudecode: {}", e));
            }
        }
    }

    if include_opencode {
        match opencode::list_sessions() {
            Ok(oc_sessions) => {
                for s in oc_sessions {
                    let ts = s.timestamp.timestamp() as f64
                        + s.timestamp.timestamp_subsec_nanos() as f64 / 1_000_000_000.0;
                    if let Some(ref filter) = ts_filter {
                        if !filter.contains(ts) {
                            continue;
                        }
                    }
                    if let Some(needle) = search {
                        if !opencode::session_contains_text(&s.session_id, needle) {
                            continue;
                        }
                    }
                    sessions.push(SessionRecord {
                        timestamp: ts,
                        session_id: s.session_id,
                        session_type: "opencode",
                    });
                }
            }
            Err(e) => {
                errors.push(format!("opencode: {}", e));
            }
        }
    }

    // Sort by timestamp (oldest first)
    sessions.sort_by(|a, b| a.timestamp.partial_cmp(&b.timestamp).unwrap());

    // Output based on format
    match format {
        OutputFormat::Json => {
            for s in &sessions {
                println!("{}", serde_json::to_string(s)?);
            }
        }
        OutputFormat::Nul => {
            use std::io::{self, Write};
            let stdout = io::stdout();
            let mut handle = stdout.lock();
            for s in &sessions {
                // Format: timestamp\0session_id\0type\0
                write!(
                    handle,
                    "{}\0{}\0{}\0",
                    s.timestamp, s.session_id, s.session_type
                )?;
            }
        }
        OutputFormat::Human => {
            for s in &sessions {
                // Human-readable uses ISO timestamp
                let dt = chrono::DateTime::from_timestamp(
                    s.timestamp as i64,
                    ((s.timestamp.fract()) * 1_000_000_000.0) as u32,
                )
                .unwrap_or_default();
                println!("{}\t{}\t{}", dt.to_rfc3339(), s.session_id, s.session_type);
            }
        }
    }

    // Report errors to stderr (unless quiet)
    if !errors.is_empty() && !quiet {
        for e in &errors {
            eprintln!("Warning: failed to list sessions from {}", e);
        }
    }

    // Return error if ALL providers failed
    if sessions.is_empty() && !errors.is_empty() {
        anyhow::bail!("Failed to list sessions: {}", errors.join("; "));
    }

    Ok(())
}

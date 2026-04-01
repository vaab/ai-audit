//! List sessions command handler.

use anyhow::Result;
use colored::Colorize;
use serde::Serialize;
use std::path::PathBuf;

use super::super::def::SessionType;
use crate::provider::{self, Provider};
use crate::OutputFormat;

/// Session record for JSON/NUL output
#[derive(Debug, Serialize)]
struct SessionRecord {
    /// UTC timestamp as float seconds since epoch
    timestamp: f64,
    session_id: String,
    #[serde(rename = "type")]
    session_type: &'static str,
    project_dir: String,
    title: String,
}

/// Parsed timespan bounds as UTC epoch seconds.
struct TimespanFilter {
    start: i64,
    end: i64,
}

impl TimespanFilter {
    /// Check if a session's [started, updated] range overlaps with the filter.
    /// A session is included if any of its activity falls within the timespan.
    fn overlaps(&self, started_secs: f64, updated_secs: f64) -> bool {
        let started = started_secs as i64;
        let updated = updated_secs as i64;
        started <= self.end && updated >= self.start
    }
}

pub fn run(
    session_type: Option<SessionType>,
    session_id: Option<&str>,
    search: Option<&str>,
    timespan: Option<&str>,
    project: Option<&str>,
    file: Option<&str>,
    all: bool,
    children_of: Option<&str>,
    format: OutputFormat,
    _quiet: bool,
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

    // Resolve --project to an absolute path for exact matching
    let project_path: Option<String> = match project {
        Some(p) => {
            let path = PathBuf::from(p);
            let abs = if path.is_absolute() {
                path
            } else {
                std::env::current_dir().unwrap_or_default().join(path)
            };
            // Canonicalize to resolve symlinks and ../ components;
            // fall back to the joined path if the directory doesn't exist.
            let resolved = abs.canonicalize().unwrap_or(abs);
            Some(resolved.to_string_lossy().to_string())
        }
        None => None,
    };

    // Resolve --file to an absolute path for structured matching
    let file_path: Option<String> = match file {
        Some(f) => {
            let path = PathBuf::from(f);
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

    let mut sessions: Vec<SessionRecord> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    // Determine which providers to query
    let providers: Vec<Box<dyn provider::SessionProvider>> = match session_type {
        Some(SessionType::ClaudeCode) => vec![provider::provider_for(Provider::ClaudeCode)],
        Some(SessionType::OpenCode) => vec![provider::provider_for(Provider::OpenCode)],
        None => provider::all_providers(),
    };

    for p in &providers {
        match p.list_sessions() {
            Ok(provider_sessions) => {
                for s in provider_sessions {
                    // Filters: cheapest first (parent/children, session_id, project, timespan, file, then search)
                    if let Some(parent) = children_of {
                        // --children-of: only include sessions whose parent_id matches
                        match &s.parent_id {
                            Some(pid) if pid == parent => {}
                            _ => continue,
                        }
                    } else if !all && s.parent_id.is_some() {
                        continue;
                    }
                    if let Some(id) = session_id {
                        if s.session_id != id {
                            continue;
                        }
                    }
                    if let Some(ref expected) = project_path {
                        if s.project_dir != *expected {
                            continue;
                        }
                    }
                    let started = s.started_at.timestamp() as f64
                        + s.started_at.timestamp_subsec_nanos() as f64 / 1_000_000_000.0;
                    let updated = s.updated_at.timestamp() as f64
                        + s.updated_at.timestamp_subsec_nanos() as f64 / 1_000_000_000.0;
                    if let Some(ref filter) = ts_filter {
                        if !filter.overlaps(started, updated) {
                            continue;
                        }
                    }
                    if let Some(ref target) = file_path {
                        if !p.session_edited_file(&s.session_id, target) {
                            continue;
                        }
                    }
                    if let Some(needle) = search {
                        if !p.session_contains_text(&s.session_id, needle) {
                            continue;
                        }
                    }
                    sessions.push(SessionRecord {
                        timestamp: started,
                        session_id: s.session_id,
                        session_type: s.provider.as_str(),
                        project_dir: s.project_dir,
                        title: s.title,
                    });
                }
            }
            Err(e) => {
                errors.push(format!("{}: {}", p.provider(), e));
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
                // Format: timestamp\0session_id\0type\0project_dir\0title\0
                write!(
                    handle,
                    "{}\0{}\0{}\0{}\0{}\0",
                    s.timestamp, s.session_id, s.session_type, s.project_dir, s.title
                )?;
            }
        }
        OutputFormat::Human => {
            let home_dir = dirs::home_dir().unwrap_or_default();
            let home_prefix = format!("{}/", home_dir.display());
            // Convert to local timezone for human display
            let to_local = |ts: f64| -> chrono::DateTime<chrono::Local> {
                chrono::DateTime::from_timestamp(ts as i64, 0)
                    .unwrap_or_default()
                    .with_timezone(&chrono::Local)
            };
            // Show time only when the timespan filter covers a single calendar day
            // (checked in local time since kal_time produces local boundaries),
            // or when all sessions happen to fall on the same local day.
            let same_day = if let Some(ref filter) = ts_filter {
                let start_local = to_local(filter.start as f64);
                // end is exclusive (start of next day), so subtract 1 second
                let end_local = to_local((filter.end - 1) as f64);
                start_local.date_naive() == end_local.date_naive()
            } else if sessions.len() > 1 {
                let first = to_local(sessions[0].timestamp);
                let last = to_local(sessions[sessions.len() - 1].timestamp);
                first.date_naive() == last.date_naive()
            } else {
                sessions.len() == 1
            };
            let ts_fmt = if same_day {
                "%H:%M:%S"
            } else {
                "%Y-%m-%dT%H:%M:%S"
            };
            // Hide columns that are forced via CLI or where all values are identical
            let show_type = session_type.is_none()
                && sessions
                    .iter()
                    .any(|s| s.session_type != sessions[0].session_type);
            let show_dir = project.is_none()
                && sessions
                    .iter()
                    .any(|s| s.project_dir != sessions[0].project_dir);
            for s in &sessions {
                let dt = to_local(s.timestamp);
                let ts = dt.format(ts_fmt).to_string();
                let mut parts = vec![ts.cyan().to_string(), s.session_id.yellow().to_string()];
                if show_type {
                    parts.push(s.session_type.purple().to_string());
                }
                if show_dir {
                    // Replace $HOME prefix with ~
                    let dir = if s.project_dir.starts_with(&home_prefix) {
                        format!("~/{}", &s.project_dir[home_prefix.len()..])
                    } else if s.project_dir == home_dir.to_string_lossy() {
                        "~".to_string()
                    } else {
                        s.project_dir.clone()
                    };
                    parts.push(dir.blue().to_string());
                }
                parts.push(s.title.white().bold().to_string());
                println!("{}", parts.join(" "));
            }
        }
    }

    for e in &errors {
        log::warn!("Failed to list sessions from {}", e);
    }

    // Return error if ALL providers failed
    if sessions.is_empty() && !errors.is_empty() {
        anyhow::bail!("Failed to list sessions: {}", errors.join("; "));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use crate::cli::def::Args;

    #[test]
    fn cli_accepts_session_id_option() {
        let args =
            Args::try_parse_from(["ai-audit", "list-sessions", "--session-id", "ses_abc123"])
                .expect("--session-id should be accepted");
        match args.command {
            crate::cli::def::Commands::ListSessions { session_id, .. } => {
                assert_eq!(session_id.as_deref(), Some("ses_abc123"));
            }
            _ => panic!("expected ListSessions command"),
        }
    }

    #[test]
    fn cli_session_id_default_is_none() {
        let args =
            Args::try_parse_from(["ai-audit", "list-sessions"]).expect("bare list-sessions works");
        match args.command {
            crate::cli::def::Commands::ListSessions { session_id, .. } => {
                assert!(session_id.is_none());
            }
            _ => panic!("expected ListSessions command"),
        }
    }

    #[test]
    fn cli_all_flag_default_is_false() {
        let args =
            Args::try_parse_from(["ai-audit", "list-sessions"]).expect("bare list-sessions works");
        match args.command {
            crate::cli::def::Commands::ListSessions { all, .. } => {
                assert!(!all);
            }
            _ => panic!("expected ListSessions command"),
        }
    }

    #[test]
    fn cli_accepts_all_short_flag() {
        let args = Args::try_parse_from(["ai-audit", "list-sessions", "-a"])
            .expect("-a should be accepted");
        match args.command {
            crate::cli::def::Commands::ListSessions { all, .. } => {
                assert!(all);
            }
            _ => panic!("expected ListSessions command"),
        }
    }

    #[test]
    fn cli_accepts_all_long_flag() {
        let args = Args::try_parse_from(["ai-audit", "list-sessions", "--all"])
            .expect("--all should be accepted");
        match args.command {
            crate::cli::def::Commands::ListSessions { all, .. } => {
                assert!(all);
            }
            _ => panic!("expected ListSessions command"),
        }
    }

    #[test]
    fn cli_children_of_default_is_none() {
        let args =
            Args::try_parse_from(["ai-audit", "list-sessions"]).expect("bare list-sessions works");
        match args.command {
            crate::cli::def::Commands::ListSessions { children_of, .. } => {
                assert!(children_of.is_none());
            }
            _ => panic!("expected ListSessions command"),
        }
    }

    #[test]
    fn cli_accepts_children_of() {
        let args = Args::try_parse_from([
            "ai-audit",
            "list-sessions",
            "--children-of",
            "ses_parent123",
        ])
        .expect("--children-of should be accepted");
        match args.command {
            crate::cli::def::Commands::ListSessions { children_of, .. } => {
                assert_eq!(children_of.as_deref(), Some("ses_parent123"));
            }
            _ => panic!("expected ListSessions command"),
        }
    }

    #[test]
    fn cli_children_of_implies_showing_subsessions() {
        // --children-of without --all should still work (children_of bypasses the all filter)
        let args = Args::try_parse_from([
            "ai-audit",
            "list-sessions",
            "--children-of",
            "ses_parent123",
        ])
        .expect("--children-of without --all should be accepted");
        match args.command {
            crate::cli::def::Commands::ListSessions {
                children_of, all, ..
            } => {
                assert_eq!(children_of.as_deref(), Some("ses_parent123"));
                assert!(
                    !all,
                    "--all should default to false when using --children-of"
                );
            }
            _ => panic!("expected ListSessions command"),
        }
    }
}

//! Activity command handler.

use anyhow::{Context, Result};
use serde::Serialize;
use std::io::{self, Write};

use super::super::def::ActivityAction;
use crate::{activity, config, OutputFormat};

/// Activity record for JSON output
#[derive(Debug, Serialize)]
struct ActivityRecord<'a> {
    /// UTC timestamp as float seconds since epoch
    timestamp: f64,
    ident: &'a str,
    session_id: &'a str,
    #[serde(flatten)]
    data: &'a crate::activity::ActivityData,
}

/// Payload with embedded ``session_id`` for NUL-separated output.
///
/// The 0k-activity contract requires 3 NUL-separated fields per record:
/// ``timestamp\0ident\0payload_json\0``.  The ``session_id`` is folded
/// into the JSON payload so the field count matches.
#[derive(Debug, Serialize)]
struct NulPayload<'a> {
    session_id: &'a str,
    #[serde(flatten)]
    data: &'a crate::activity::ActivityData,
}

pub fn run(action: ActivityAction) -> Result<()> {
    let config = config::Config::load().context("Failed to load configuration")?;

    match action {
        ActivityAction::List { output } => {
            let identifiers = activity::list_identifiers(&config)?;
            let format = output.format();

            match format {
                OutputFormat::Json => {
                    for ident in identifiers {
                        println!("{}", serde_json::to_string(&ident)?);
                    }
                }
                OutputFormat::Nul => {
                    let stdout = io::stdout();
                    let mut handle = stdout.lock();
                    for ident in identifiers {
                        write!(handle, "{}\0", ident)?;
                    }
                }
                OutputFormat::Human => {
                    for ident in identifiers {
                        println!("{}", ident);
                    }
                }
            }
        }
        ActivityAction::Get {
            timespan,
            identifiers,
            sessions,
            output,
        } => {
            let (start, end) = kal_time::parse_timespan(&timespan)
                .map_err(|e| anyhow::anyhow!("Failed to parse timespan '{}': {}", timespan, e))?;

            let events = activity::fetch_activities(&config, start, end, &identifiers, &sessions)?;
            let format = output.format();

            let stdout = io::stdout();
            let mut handle = stdout.lock();

            match format {
                OutputFormat::Json => {
                    for event in &events {
                        let record = ActivityRecord {
                            timestamp: event.timestamp as f64,
                            ident: &event.ident,
                            session_id: &event.session_id,
                            data: &event.data,
                        };
                        writeln!(handle, "{}", serde_json::to_string(&record)?)?;
                    }
                }
                OutputFormat::Nul => {
                    // Format: timestamp\0ident\0json_data\0
                    // Matches the 0k-activity 3-field contract;
                    // session_id is embedded inside the JSON payload.
                    for event in events {
                        let payload = NulPayload {
                            session_id: &event.session_id,
                            data: &event.data,
                        };
                        let json = serde_json::to_string(&payload)?;
                        write!(handle, "{}\0{}\0{}\0", event.timestamp, event.ident, json)?;
                    }
                }
                OutputFormat::Human => {
                    for event in events {
                        let timestamp_str = activity::format_timestamp_display(event.timestamp);
                        let summary = activity::activity_summary(&event);
                        let short_session = truncate_session_id(&event.session_id);
                        writeln!(
                            handle,
                            "{} {} [{}] {}",
                            timestamp_str, event.ident, short_session, summary
                        )?;
                    }
                }
            }
        }
    }

    Ok(())
}

/// Truncate a session ID for human-readable display.
///
/// UUIDs are shortened to their first 8 characters; other formats
/// (e.g., OpenCode `ses_*`) are kept as-is.
fn truncate_session_id(session_id: &str) -> &str {
    // UUIDs are 36 chars (8-4-4-4-12 with dashes)
    if session_id.len() == 36 && session_id.chars().nth(8) == Some('-') {
        &session_id[..8]
    } else {
        session_id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_truncate_uuid() {
        assert_eq!(
            truncate_session_id("a1b2c3d4-e5f6-7890-abcd-ef1234567890"),
            "a1b2c3d4"
        );
    }

    #[test]
    fn test_truncate_opencode_session_id() {
        assert_eq!(truncate_session_id("ses_abc123"), "ses_abc123");
    }

    #[test]
    fn test_truncate_short_id() {
        assert_eq!(truncate_session_id("short"), "short");
    }

    #[test]
    fn test_truncate_empty() {
        assert_eq!(truncate_session_id(""), "");
    }
}

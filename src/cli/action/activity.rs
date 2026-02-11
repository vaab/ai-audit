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
            output,
        } => {
            let (start, end) = kal_time::parse_timespan(&timespan)
                .map_err(|e| anyhow::anyhow!("Failed to parse timespan '{}': {}", timespan, e))?;

            let events = activity::fetch_activities(&config, start, end, &identifiers)?;
            let format = output.format();

            let stdout = io::stdout();
            let mut handle = stdout.lock();

            match format {
                OutputFormat::Json => {
                    for event in &events {
                        let record = ActivityRecord {
                            timestamp: event.timestamp as f64,
                            ident: &event.ident,
                            data: &event.data,
                        };
                        writeln!(handle, "{}", serde_json::to_string(&record)?)?;
                    }
                }
                OutputFormat::Nul => {
                    // Format: timestamp\0ident\0json_data\0
                    for event in events {
                        let json = serde_json::to_string(&event.data)?;
                        write!(handle, "{}\0{}\0{}\0", event.timestamp, event.ident, json)?;
                    }
                }
                OutputFormat::Human => {
                    for event in events {
                        let timestamp_str = activity::format_timestamp_display(event.timestamp);
                        let summary = activity::activity_summary(&event);
                        writeln!(handle, "{} {} {}", timestamp_str, event.ident, summary)?;
                    }
                }
            }
        }
    }

    Ok(())
}

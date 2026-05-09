//! Activity command handler.

use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::HashMap;
use std::io::{self, Write};

use super::super::def::ActivityAction;
use crate::empty_segments::Bounds;
use crate::{activity, config, empty_segments, OutputFormat};

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
            let t_total = std::time::Instant::now();
            log::info!("activity get: timespan={}", timespan);

            let (start, end) = kal_time::parse_timespan(&timespan)
                .map_err(|e| anyhow::anyhow!("Failed to parse timespan '{}': {}", timespan, e))?;
            let now = chrono::Utc::now().timestamp();

            // Build the session index ONCE and share it with both
            // ``fetch_activities`` and the empty-segment path so we
            // don't scan all provider metadata twice.
            let session_index = activity::build_full_session_index(&config);

            let events = activity::fetch_activities_with_index(
                &config,
                start,
                end,
                &identifiers,
                &sessions,
                &session_index,
            )?;
            let event_count = events.len();
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
                    log::info!(
                        "activity get: {} events in {:?}",
                        event_count,
                        t_total.elapsed()
                    );
                }
                OutputFormat::Nul => {
                    let requested_idents = if identifiers.is_empty() {
                        activity::list_identifiers_with_index(&session_index)
                    } else {
                        identifiers.clone()
                    };
                    log::info!("activity get: scanning {} idents", requested_idents.len());

                    let cache = empty_segments::Cache::new()?;
                    let mut cached_bounds = HashMap::new();
                    let mut misses = Vec::new();
                    let t_cache_scan = std::time::Instant::now();

                    for ident in &requested_idents {
                        let files =
                            activity::enumerate_files_for_ident_with_index(ident, &session_index);
                        let fingerprint = empty_segments::Cache::fingerprint_for_files(&files)?;
                        match cache.load(ident) {
                            Some(entry) if entry.fingerprint == fingerprint => {
                                log::trace!("ident={}: cache hit", ident);
                                cached_bounds.insert(ident.clone(), entry.bounds);
                                continue;
                            }
                            Some(_) => {
                                log::trace!("ident={}: cache miss (fingerprint changed)", ident)
                            }
                            None => log::trace!("ident={}: cache miss (no entry)", ident),
                        }
                        misses.push((ident.clone(), fingerprint));
                    }
                    log::debug!(
                        "empty-segments cache scan: {} hits / {} misses in {:?}",
                        cached_bounds.len(),
                        misses.len(),
                        t_cache_scan.elapsed()
                    );

                    let missing_idents = misses
                        .iter()
                        .map(|(ident, _)| ident.clone())
                        .collect::<Vec<_>>();
                    let fresh_timestamps = if missing_idents.is_empty() {
                        HashMap::new()
                    } else {
                        activity::fetch_all_event_timestamps_with_index(
                            &config,
                            &missing_idents,
                            &session_index,
                        )?
                    };

                    let t_emit_controls = std::time::Instant::now();
                    let mut control_record_count = 0usize;
                    for ident in &requested_idents {
                        // Resolve bounds: cache hit → cached `Option<Bounds>`;
                        // miss → derive from freshly-scanned timestamps
                        // (`None` means "no events ever for this ident").
                        let bounds_opt: Option<Bounds> = match cached_bounds.remove(ident) {
                            Some(b) => b,
                            None => fresh_timestamps
                                .get(ident)
                                .and_then(|timestamps| Bounds::from_timestamps(timestamps)),
                        };

                        // Save cache for every miss — including `None`
                        // bounds, so zero-event idents are not rescanned
                        // on every invocation.
                        if let Some((_, fingerprint)) =
                            misses.iter().find(|(name, _)| name == ident)
                        {
                            cache.save(
                                ident,
                                &empty_segments::CacheEntry {
                                    fingerprint: fingerprint.clone(),
                                    bounds: bounds_opt.clone(),
                                    last_run_at: now,
                                },
                            )?;
                        }

                        let Some(bounds) = bounds_opt else {
                            continue;
                        };
                        let intervals = empty_segments::intervals_for(&bounds, now);
                        if !intervals.is_empty() {
                            write!(
                                handle,
                                "{}",
                                empty_segments::format_record(ident, &intervals)
                            )?;
                            control_record_count += 1;
                        }
                    }
                    log::debug!(
                        "emitted {} empty-segment control records in {:?}",
                        control_record_count,
                        t_emit_controls.elapsed()
                    );

                    // Format: timestamp\0ident\0json_data\0
                    // Matches the 0k-activity 3-field contract;
                    // session_id is embedded inside the JSON payload.
                    let t_emit_events = std::time::Instant::now();
                    for event in events {
                        let payload = NulPayload {
                            session_id: &event.session_id,
                            data: &event.data,
                        };
                        let json = serde_json::to_string(&payload)?;
                        write!(handle, "{}\0{}\0{}\0", event.timestamp, event.ident, json)?;
                    }
                    log::debug!(
                        "emitted {} event records in {:?}",
                        event_count,
                        t_emit_events.elapsed()
                    );
                    log::info!(
                        "activity get: {} controls + {} events in {:?}",
                        control_record_count,
                        event_count,
                        t_total.elapsed()
                    );
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
                    log::info!(
                        "activity get: {} events in {:?}",
                        event_count,
                        t_total.elapsed()
                    );
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

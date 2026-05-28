//! Activity command handler.

use anyhow::{anyhow, Context, Result};
use serde::Serialize;
use std::collections::HashMap;
use std::fs;
use std::io::{self, Read, Write};
use std::path::Path;

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
            categs_file,
            output,
        } => {
            let t_total = std::time::Instant::now();
            log::info!("activity get: timespan={}", timespan);

            let (start, end) = kal_time::parse_timespan(&timespan)
                .map_err(|e| anyhow::anyhow!("Failed to parse timespan '{}': {}", timespan, e))?;
            let now = chrono::Utc::now().timestamp();

            // Merge positional IDENT args with --categs-file contents (if any).
            // Both forms are additive; either can be empty.
            let merged_idents: Vec<String> =
                if let Some(path) = categs_file.as_deref() {
                    let mut all = identifiers;
                    all.extend(load_categs_file(path).with_context(|| {
                        format!("Failed to load --categs-file {}", path.display())
                    })?);
                    all
                } else {
                    identifiers
                };

            // Build the session index ONCE and share it with both
            // ``fetch_activities`` and the empty-segment path so we
            // don't scan all provider metadata twice.
            let session_index = activity::build_full_session_index(&config);

            let events = activity::fetch_activities_with_index(
                &config,
                start,
                end,
                &merged_idents,
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
                    let requested_idents = if merged_idents.is_empty() {
                        activity::list_identifiers_with_index(&session_index)
                    } else {
                        merged_idents.clone()
                    };
                    log::info!("activity get: scanning {} idents", requested_idents.len());

                    let cache = empty_segments::Cache::new()?;
                    let mut cached_bounds = HashMap::new();
                    let mut misses = Vec::new();
                    let t_cache_scan = std::time::Instant::now();

                    for ident in &requested_idents {
                        let files = activity::enumerate_files_for_ident_via_cache(ident, &config);
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

                        // Zero-event ident: emit a single [null, now)
                        // empty-zone declaration so fyl learns "this
                        // category has produced no events from the
                        // beginning of time up to this query".
                        // Otherwise: emit the bounded leading/gap/
                        // trailing intervals derived from observed
                        // events.
                        let intervals = match &bounds_opt {
                            Some(bounds) => empty_segments::intervals_for(bounds, now),
                            None => empty_segments::intervals_for_no_events(now),
                        };
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

/// Load NUL-separated identifiers from a file or stdin.
///
/// `path == "-"` reads from `io::stdin()`. Any other path is opened as
/// a regular file. The contents are split on `\0`; trailing empty
/// fragments (from a file ending in `\0`) are dropped, but
/// **interior** empty entries are rejected as malformed input rather
/// than silently swallowed (an empty identifier would match nothing
/// and is almost certainly a bug in the caller's NUL serialisation).
///
/// All bytes must be valid UTF-8.
fn load_categs_file(path: &Path) -> Result<Vec<String>> {
    let mut buf = Vec::new();
    if path == Path::new("-") {
        io::stdin()
            .lock()
            .read_to_end(&mut buf)
            .context("Failed to read identifiers from stdin")?;
    } else {
        let mut f =
            fs::File::open(path).with_context(|| format!("Failed to open {}", path.display()))?;
        f.read_to_end(&mut buf)
            .with_context(|| format!("Failed to read {}", path.display()))?;
    }

    // Drop a single trailing NUL (common when the producer terminates
    // every record with NUL, including the last one).
    if buf.last() == Some(&0) {
        buf.pop();
    }

    if buf.is_empty() {
        return Ok(Vec::new());
    }

    let mut out = Vec::new();
    for (idx, chunk) in buf.split(|&b| b == 0).enumerate() {
        if chunk.is_empty() {
            return Err(anyhow!(
                "empty identifier at NUL-separated position {} (malformed input)",
                idx
            ));
        }
        let s = std::str::from_utf8(chunk).with_context(|| {
            format!(
                "identifier at NUL-separated position {} is not valid UTF-8",
                idx
            )
        })?;
        out.push(s.to_string());
    }
    Ok(out)
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

    // --- load_categs_file ---

    fn write_tmp(bytes: &[u8]) -> tempfile::NamedTempFile {
        use std::io::Write as _;
        let mut f = tempfile::NamedTempFile::new().expect("tmpfile");
        f.write_all(bytes).expect("write");
        f.flush().expect("flush");
        f
    }

    #[test]
    fn load_categs_file_three_idents_no_trailing_nul() {
        let f = write_tmp(b"alpha\0beta\0gamma");
        let got = load_categs_file(f.path()).expect("load");
        assert_eq!(
            got,
            vec!["alpha".to_string(), "beta".to_string(), "gamma".to_string()]
        );
    }

    #[test]
    fn load_categs_file_three_idents_with_trailing_nul() {
        // Producer terminates every record with NUL — common pattern.
        let f = write_tmp(b"alpha\0beta\0gamma\0");
        let got = load_categs_file(f.path()).expect("load");
        assert_eq!(
            got,
            vec!["alpha".to_string(), "beta".to_string(), "gamma".to_string()]
        );
    }

    #[test]
    fn load_categs_file_empty_file_yields_empty_vec() {
        let f = write_tmp(b"");
        let got = load_categs_file(f.path()).expect("load");
        assert!(got.is_empty());
    }

    #[test]
    fn load_categs_file_single_trailing_nul_is_empty() {
        let f = write_tmp(b"\0");
        let got = load_categs_file(f.path()).expect("load");
        assert!(got.is_empty());
    }

    #[test]
    fn load_categs_file_interior_empty_field_errors() {
        // Two NULs in a row → empty field in the middle → reject.
        let f = write_tmp(b"alpha\0\0beta");
        let err = load_categs_file(f.path()).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("empty identifier"),
            "expected 'empty identifier' error, got: {msg}"
        );
    }

    #[test]
    fn load_categs_file_invalid_utf8_errors() {
        // 0xFF is invalid UTF-8.
        let f = write_tmp(b"alpha\0\xFFbeta");
        let err = load_categs_file(f.path()).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("not valid UTF-8") || msg.contains("UTF-8"),
            "expected UTF-8 error, got: {msg}"
        );
    }

    #[test]
    fn load_categs_file_missing_path_errors() {
        let path = Path::new("/nonexistent/definitely/not/here.nul");
        let err = load_categs_file(path).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("Failed to open") || msg.contains("No such file"),
            "expected open error, got: {msg}"
        );
    }
}

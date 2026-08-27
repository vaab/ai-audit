//! Opencode per-message latency derivation from part-level
//! `time.start` / `time.end` data.
//!
//! Why this lives in opencode (not in `cli::action::token_usage`):
//! opencode is the only harness whose `Message.timestamp` is
//! start-of-turn AND whose tool execution is bundled inside the same
//! assistant message via the part model.  The unified
//! `TranscriptEntry` shape (timestamp + role + entry_type) collapses
//! this information — part-level `time.start`/`time.end` are NOT
//! exposed there — so a clean `llm_generation_s` for opencode can
//! only come from walking the SQLite `part` table directly.
//!
//! Relationship to `crate::cli::action::token_usage::derive_latencies`:
//! that helper produces *harness-defined* wall-clock from a unified
//! transcript.  This module produces *uniform* part-attributed
//! latency from opencode-native data.  The call site in
//! `token_usage::run` prefers part-attributed values when present
//! and falls back to the transcript-walked wall-clock otherwise.
//!
//! Part-type mapping (mirrors `crate::opencode::transcript`):
//! - `text`, `reasoning` → counted toward `llm_generation_s`
//!   (the model is generating output tokens).
//! - `tool` → counted toward `tool_latency_s_before`
//!   (a tool ran for this message; the duration is wall-clock from
//!   the tool starting to the tool returning).
//! - `step-start`, `step-finish`, `compaction` → skipped (control
//!   parts, not work).
//! - Anything else → skipped with a `debug!` trace.

use anyhow::Result;
use serde_json::Value;

use crate::provider::{MessageLatencyPair, MessagePartLatencies};

/// Build a `Message.timestamp → (llm_generation_s, tool_latency_s_before)`
/// map for every assistant message of `session_id` that has at least
/// one part with non-null `time.start`/`time.end` data.
///
/// Messages with no part-timed data are *omitted* from the map (the
/// caller treats absence as "no override" and falls back to the
/// transcript-walked wall-clock).
///
/// Each output value is computed per message:
/// - `llm_generation_s` = `Some(sum of (end-start) over text+reasoning parts)`
///   if any such part has both timestamps, else `None`.
/// - `tool_latency_s_before` = `Some(sum of (end-start) over tool parts)`
///   if any tool part has both timestamps, else `None`.
///
/// A non-empty entry where ONE of the two fields is `None` is
/// meaningful: it says "we have data for the other but not this one"
/// — the caller MUST NOT fall back to the transcript value for the
/// `None` field of an entry that's already in the map.  Falling back
/// per-field would mix part-attributed and transcript-derived values
/// for the same message, which is exactly the cross-harness
/// confusion we are trying to retire.
pub fn message_part_latencies(session_id: &str) -> Result<MessagePartLatencies> {
    if !super::db::db_exists() {
        return Ok(std::collections::HashMap::new());
    }
    let conn = super::db::open_db()?;
    message_part_latencies_from_conn(&conn, session_id)
}

/// SQL-decoupled core (testable without a real on-disk DB).
pub fn message_part_latencies_from_conn(
    conn: &rusqlite::Connection,
    session_id: &str,
) -> Result<MessagePartLatencies> {
    let messages = super::db::get_messages_for_session(conn, session_id)?;

    let mut out: MessagePartLatencies = std::collections::HashMap::new();

    for (msg_id, data) in &messages {
        // Only assistant messages carry token data and warrant a
        // latency entry; user / system messages are not emitted by
        // `token_usage` and therefore would never key against this
        // map.  Skipping them keeps the map size proportional to the
        // actual output stream.
        if data.get("role").and_then(|v| v.as_str()) != Some("assistant") {
            continue;
        }

        let Some(msg_ts) = message_timestamp(data) else {
            // No `time.created` → no key; cannot attribute parts.
            continue;
        };

        let parts = super::db::get_parts_for_message(conn, msg_id)?;
        let (gen_s, tool_s) = sum_part_latencies(&parts);

        // Only insert the entry when at least one of the two fields
        // is derivable.  An entry with both `None` would suppress
        // the transcript fallback for a message that has no
        // part-level timing at all — which is the wrong choice
        // (rather than emit two nulls, we want the transcript
        // wall-clock to fill in).
        if gen_s.is_some() || tool_s.is_some() {
            out.insert(msg_ts, (gen_s, tool_s));
        }
    }

    Ok(out)
}

/// Extract the `Message.timestamp` (same anchor as
/// `crate::opencode::list_messages` uses for the unified `Message`).
fn message_timestamp(data: &Value) -> Option<chrono::DateTime<chrono::Utc>> {
    use chrono::TimeZone;
    use chrono::Utc;
    let ms = data
        .get("time")
        .and_then(|t| t.get("created"))
        .and_then(|v| v.as_i64())?;
    Utc.timestamp_millis_opt(ms).single()
}

/// Sum part-level latency by category for one message's parts.
///
/// Returns `(llm_generation_s, tool_latency_s_before)`.  `None` for a
/// category means "no parts in that category had both `time.start`
/// AND `time.end`" (so the sum would be vacuous and we'd rather
/// signal that than emit a misleading 0.0).
pub(crate) fn sum_part_latencies(parts: &[Value]) -> MessageLatencyPair {
    let mut gen_total_ms: i64 = 0;
    let mut gen_count: usize = 0;
    let mut tool_total_ms: i64 = 0;
    let mut tool_count: usize = 0;

    for part in parts {
        let part_type = part.get("type").and_then(|v| v.as_str()).unwrap_or("");
        // Compute duration in ms; both endpoints must be present.
        let dur_ms = match part_duration_ms(part) {
            Some(d) if d >= 0 => d,
            Some(neg) => {
                // Non-monotonic part timing — surface, do not clamp.
                log::warn!(
                    "opencode part {} has negative duration ({} ms); skipping in latency sum",
                    part.get("id").and_then(|v| v.as_str()).unwrap_or("<no-id>"),
                    neg
                );
                continue;
            }
            None => continue,
        };

        match part_type {
            "text" | "reasoning" => {
                gen_total_ms += dur_ms;
                gen_count += 1;
            }
            "tool" => {
                tool_total_ms += dur_ms;
                tool_count += 1;
            }
            "step-start" | "step-finish" | "compaction" => {}
            other => {
                log::debug!(
                    "opencode part type '{}' not classified for latency sum; skipping",
                    other
                );
            }
        }
    }

    let gen = if gen_count > 0 {
        Some(gen_total_ms as f64 / 1000.0)
    } else {
        None
    };
    let tool = if tool_count > 0 {
        Some(tool_total_ms as f64 / 1000.0)
    } else {
        None
    };
    (gen, tool)
}

/// Extract `(time.end − time.start)` in milliseconds for a single part.
///
/// Returns `None` when either endpoint is null or missing.  The
/// opencode SQLite store routinely has `time.start` / `time.end` set
/// to null on older sessions or for parts the server crashed
/// mid-emit; treat both as "no signal" rather than zero.
fn part_duration_ms(part: &Value) -> Option<i64> {
    let time = part.get("time")?;
    let start = time.get("start").and_then(|v| v.as_i64())?;
    let end = time.get("end").and_then(|v| v.as_i64())?;
    Some(end - start)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sum_text_and_tool_parts() {
        let parts = vec![
            json!({"type":"text","time":{"start":1000,"end":1500}}),
            json!({"type":"reasoning","time":{"start":1500,"end":1800}}),
            json!({"type":"tool","time":{"start":1800,"end":2300}}),
            json!({"type":"tool","time":{"start":2300,"end":2400}}),
        ];
        let (gen, tool) = sum_part_latencies(&parts);
        // text 0.5 + reasoning 0.3 = 0.8
        assert!((gen.unwrap() - 0.8).abs() < 1e-9);
        // tool 0.5 + 0.1 = 0.6
        assert!((tool.unwrap() - 0.6).abs() < 1e-9);
    }

    #[test]
    fn no_parts_yields_both_none() {
        let (gen, tool) = sum_part_latencies(&[]);
        assert!(gen.is_none());
        assert!(tool.is_none());
    }

    #[test]
    fn parts_with_null_times_are_omitted() {
        let parts = vec![
            json!({"type":"text"}), // no time at all
            json!({"type":"tool","time":{"start":null,"end":null}}),
            json!({"type":"tool","time":{"start":1000,"end":1200}}),
        ];
        let (gen, tool) = sum_part_latencies(&parts);
        assert!(gen.is_none());
        assert_eq!(tool, Some(0.2));
    }

    #[test]
    fn control_parts_are_skipped() {
        let parts = vec![
            json!({"type":"step-start","time":{"start":1000,"end":1001}}),
            json!({"type":"step-finish","time":{"start":2000,"end":2001}}),
            json!({"type":"text","time":{"start":1500,"end":1900}}),
        ];
        let (gen, tool) = sum_part_latencies(&parts);
        assert_eq!(gen, Some(0.4));
        assert_eq!(tool, None);
    }

    #[test]
    fn negative_duration_logged_and_skipped() {
        let parts = vec![
            json!({"type":"text","time":{"start":2000,"end":1000}}), // skewed
            json!({"type":"text","time":{"start":1000,"end":1300}}),
        ];
        let (gen, _) = sum_part_latencies(&parts);
        // The clean one survives.
        assert_eq!(gen, Some(0.3));
    }

    #[test]
    fn only_tool_present_yields_tool_only() {
        let parts = vec![json!({"type":"tool","time":{"start":1000,"end":1500}})];
        let (gen, tool) = sum_part_latencies(&parts);
        assert_eq!(gen, None);
        assert_eq!(tool, Some(0.5));
    }
}

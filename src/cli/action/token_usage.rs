//! `token-usage` action — per-message LLM token consumption events.
//!
//! Aggregation strategy: for each session matching the time / project
//! / type filters, walk `provider.list_messages()` and emit one
//! [`TokenEvent`] per assistant message with non-empty token data
//! whose timestamp lies inside the requested timespan.
//!
//! Architectural relationships:
//! - **Sibling of `usage`**: same data source (`Message.tokens`)
//!   but per-message granularity instead of per-session totals.
//! - **Sibling of `activity`**: same shape (timespan-bounded event
//!   stream over sessions) but token-typed payload instead of
//!   text/permission events.
//!
//! Field selection mirrors the insight-cli `fyl-seg-list` pattern
//! (Field enum + per-row triple representation: human / NUL / JSON).

use std::io::{self, Write};
use std::path::PathBuf;

use anyhow::{anyhow, Result};
use chrono::{DateTime, Local, Utc};

use super::super::def::SessionType;
use crate::format::format_tokens;
use crate::project::{project_info_from_cwd, ProjectInfo};
use crate::provider::{provider_for_session, Message, Provider, TokenUsage};
use crate::session_filter::{list_filtered, SessionFilter};
use crate::transcript::{EntryType, Role, TranscriptEntry};
use crate::OutputFormat;

// ---------------------------------------------------------------------------
// Field enum
// ---------------------------------------------------------------------------

/// Selectable output field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Timestamp,
    SessionId,
    Provider,
    ProviderId,
    Model,
    Cwd,
    ProjectPath,
    Project,
    Subpath,
    Input,
    Output,
    CacheRead,
    CacheWrite,
    CacheCreation,
    Reasoning,
    Total,
    /// Wall-clock seconds between the most recent prior input
    /// marker (`user.text` / any-role `tool_result` / any-role
    /// `tool_error`) and this assistant message's `timestamp`.
    /// `null` after a transcript-level `error`, when no prior
    /// marker exists, or when a negative gap is detected
    /// (clock-skew sentinel — logged at `-vvv` rather than silently
    /// clamped).
    ///
    /// **Cross-harness semantics warning.** This is harness-defined
    /// wall-clock, NOT clean LLM-generation time.  The
    /// `Message.timestamp` anchor differs by harness:
    /// - pi + claudecode: `timestamp` is the moment the assistant
    ///   *finished* (model emitted its full content). So
    ///   `response_wall_clock_s` is close to LLM generation time,
    ///   since tools execute in a separate user-role turn between
    ///   messages.
    /// - opencode: `timestamp = time.created` is the moment the
    ///   assistant message *started*; tools execute *inside* that
    ///   same message via opencode's part model.  So opencode's
    ///   `response_wall_clock_s` includes ALL bundled tool runtime
    ///   for the rest of this message.
    ///
    /// Downstream consumers MUST NOT compare
    /// `response_wall_clock_s` across harnesses without knowing
    /// what they are doing.  For a harness-uniform LLM-generation
    /// signal use [`Field::LlmGenerationS`] (currently fully
    /// populated for pi + claudecode; null on opencode pending
    /// part-walking implementation — see admin.org TODO).
    ResponseWallClockS,
    /// Cross-harness-uniform seconds the LLM spent generating this
    /// assistant message's output.  Always means the same thing
    /// (tool time excluded) regardless of harness:
    /// - pi + claudecode: equal to `response_wall_clock_s` (their
    ///   `Message.timestamp` is the *end* of the assistant turn and
    ///   tools execute between turns, so the wall-clock IS the
    ///   generation time).
    /// - opencode: currently `null` — requires per-harness
    ///   derivation from part-level `time.start` / `time.end` data
    ///   in the opencode SQLite store.  Tracked as the next step in
    ///   the token-warden TODO (admin.org).
    ///
    /// This is the field downstream KPIs ("seconds per generated
    /// token", "provider latency drift") should use.  It is `null`
    /// when not derivable rather than approximated.
    LlmGenerationS,
    /// Sum of `tool_result.timestamp − tool_use.timestamp` for tool
    /// pairs whose `tool_use` lands in `[prev_message.timestamp,
    /// this_message.timestamp)` and whose matching
    /// `tool_result|tool_error` lands at-or-before
    /// `this_message.timestamp`.  `null` when no tool calls
    /// intervened or all intervening pairs were zero-width (see
    /// below).  `ToolError` closes a gap identically to `ToolResult`.
    ///
    /// **Cross-harness semantics caveat:**
    /// - pi + claudecode: `tool_use` lands on the prior assistant
    ///   message, `tool_result` lands in the next user-role message
    ///   between turns — the gap is a clean tool-runtime measurement.
    /// - opencode: when `part.time.start`/`part.time.end` are null in
    ///   the source data, opencode's transcript parser collapses
    ///   both `tool_use` and `tool_result` to `msg.time.created`,
    ///   producing zero-width pairs that we deliberately *skip*
    ///   (treated as "no signal" rather than "0-second tool").  When
    ///   ALL of a message's intervening pairs are zero-width, the
    ///   field is `null`.  Opencode-side part-walking (planned with
    ///   `llm_generation_s`) will populate this field with the sum
    ///   of intra-message tool-part durations.
    ToolLatencyBeforeS,
}

impl Field {
    /// Canonical field name as it appears in `--fields`, NUL output,
    /// and JSON output (snake_case).
    pub fn as_str(&self) -> &'static str {
        match self {
            Field::Timestamp => "timestamp",
            Field::SessionId => "session_id",
            Field::Provider => "provider",
            Field::ProviderId => "provider_id",
            Field::Model => "model",
            Field::Cwd => "cwd",
            Field::ProjectPath => "project_path",
            Field::Project => "project",
            Field::Subpath => "subpath",
            Field::Input => "input",
            Field::Output => "output",
            Field::CacheRead => "cache_read",
            Field::CacheWrite => "cache_write",
            Field::CacheCreation => "cache_creation",
            Field::Reasoning => "reasoning",
            Field::Total => "total",
            Field::ResponseWallClockS => "response_wall_clock_s",
            Field::LlmGenerationS => "llm_generation_s",
            Field::ToolLatencyBeforeS => "tool_latency_s_before",
        }
    }

    /// Human-mode column header (uppercase).
    pub fn header(&self) -> &'static str {
        match self {
            Field::Timestamp => "TIMESTAMP",
            Field::SessionId => "SESSION_ID",
            Field::Provider => "PROVIDER",
            Field::ProviderId => "PROVIDER_ID",
            Field::Model => "MODEL",
            Field::Cwd => "CWD",
            Field::ProjectPath => "PROJECT_PATH",
            Field::Project => "PROJECT",
            Field::Subpath => "SUBPATH",
            Field::Input => "INPUT",
            Field::Output => "OUTPUT",
            Field::CacheRead => "CACHE_READ",
            Field::CacheWrite => "CACHE_WRITE",
            Field::CacheCreation => "CACHE_CREATION",
            Field::Reasoning => "REASONING",
            Field::Total => "TOTAL",
            Field::ResponseWallClockS => "RESPONSE_WALL_CLOCK_S",
            Field::LlmGenerationS => "LLM_GENERATION_S",
            Field::ToolLatencyBeforeS => "TOOL_LATENCY_S_BEFORE",
        }
    }

    /// Whether this field is a token count (right-aligned, humanized
    /// in human mode).
    pub fn is_token_count(&self) -> bool {
        matches!(
            self,
            Field::Input
                | Field::Output
                | Field::CacheRead
                | Field::CacheWrite
                | Field::CacheCreation
                | Field::Reasoning
                | Field::Total
        )
    }

    /// Parse a `--fields` token.  Returns `None` for unknown names.
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "timestamp" => Field::Timestamp,
            "session_id" => Field::SessionId,
            "provider" => Field::Provider,
            "provider_id" => Field::ProviderId,
            "model" => Field::Model,
            "cwd" => Field::Cwd,
            "project_path" => Field::ProjectPath,
            "project" => Field::Project,
            "subpath" => Field::Subpath,
            "input" => Field::Input,
            "output" => Field::Output,
            "cache_read" => Field::CacheRead,
            "cache_write" => Field::CacheWrite,
            "cache_creation" => Field::CacheCreation,
            "reasoning" => Field::Reasoning,
            "total" => Field::Total,
            "response_wall_clock_s" => Field::ResponseWallClockS,
            "llm_generation_s" => Field::LlmGenerationS,
            "tool_latency_s_before" => Field::ToolLatencyBeforeS,
            _ => return None,
        })
    }
}

/// Canonical full ordering of fields (used when no `--fields` is given
/// for NUL/JSON output, so the wire format includes complete data per
/// the cli-guidelines).
pub const ALL_FIELDS: &[Field] = &[
    Field::Timestamp,
    Field::SessionId,
    Field::Provider,
    Field::ProviderId,
    Field::Model,
    Field::Cwd,
    Field::ProjectPath,
    Field::Project,
    Field::Subpath,
    Field::Input,
    Field::Output,
    Field::CacheRead,
    Field::CacheWrite,
    Field::CacheCreation,
    Field::Reasoning,
    Field::Total,
    Field::ResponseWallClockS,
    Field::LlmGenerationS,
    Field::ToolLatencyBeforeS,
];

/// Default field set for human display (compact, fits a terminal).
///
/// The latency fields (`response_wall_clock_s`, `llm_generation_s`,
/// `tool_latency_s_before`) are *intentionally* omitted from the
/// default human view — the column count is already high.  They are
/// present in `ALL_FIELDS`, so the JSON / NUL wire formats include
/// them by default per the cli-guidelines "complete data on the
/// wire" rule.  Human users opt in via
/// `--fields response_wall_clock_s,llm_generation_s,tool_latency_s_before`.
pub const DEFAULT_FIELDS_HUMAN: &[Field] = &[
    Field::Timestamp,
    Field::Project,
    Field::SessionId,
    Field::Model,
    Field::Input,
    Field::Output,
    Field::CacheRead,
    Field::Total,
];

// ---------------------------------------------------------------------------
// Event payload
// ---------------------------------------------------------------------------

/// A single token-consuming event (one assistant message).
struct TokenEvent {
    timestamp: DateTime<chrono::Utc>,
    session_id: String,
    provider: &'static str,
    provider_id: Option<String>,
    model: Option<String>,
    project: ProjectInfo,
    tokens: TokenUsage,
    /// Harness-defined wall-clock seconds from prior input marker to
    /// this assistant message's `timestamp`.  Meaning varies by
    /// harness — see [`Field::ResponseWallClockS`] rustdoc.
    response_wall_clock_s: Option<f64>,
    /// Cross-harness-uniform LLM generation seconds.  Same meaning
    /// (tool time excluded) regardless of harness.  See
    /// [`Field::LlmGenerationS`] rustdoc.  Currently equal to
    /// `response_wall_clock_s` for pi + claudecode, `None` for
    /// opencode (pending part-walking).
    llm_generation_s: Option<f64>,
    /// Sum of `tool_result − tool_use` gaps for tool calls between
    /// the previous assistant turn and this one.  See
    /// [`derive_latencies`] for the windowing rules.
    tool_latency_s_before: Option<f64>,
}

impl TokenEvent {
    /// Compute the value of a given field as a JSON value (used by
    /// the JSON renderer and for typed access from the NUL/Human
    /// renderers).
    fn json_value(&self, field: Field) -> serde_json::Value {
        use serde_json::Value;
        match field {
            Field::Timestamp => {
                let secs = self.timestamp.timestamp() as f64
                    + self.timestamp.timestamp_subsec_nanos() as f64 / 1_000_000_000.0;
                Value::from(secs)
            }
            Field::SessionId => Value::String(self.session_id.clone()),
            Field::Provider => Value::String(self.provider.to_string()),
            Field::ProviderId => match &self.provider_id {
                Some(s) => Value::String(s.clone()),
                None => Value::Null,
            },
            Field::Model => match &self.model {
                Some(s) => Value::String(s.clone()),
                None => Value::Null,
            },
            Field::Cwd => Value::String(self.project.cwd.to_string_lossy().into_owned()),
            Field::ProjectPath => {
                Value::String(self.project.project_path.to_string_lossy().into_owned())
            }
            Field::Project => Value::String(self.project.project.clone()),
            Field::Subpath => Value::String(self.project.subpath.to_string_lossy().into_owned()),
            Field::Input => Value::from(self.tokens.input),
            Field::Output => Value::from(self.tokens.output),
            Field::CacheRead => Value::from(self.tokens.cache_read),
            Field::CacheWrite => Value::from(self.tokens.cache_write),
            Field::CacheCreation => Value::from(self.tokens.cache_creation),
            Field::Reasoning => Value::from(self.tokens.reasoning),
            Field::Total => Value::from(self.tokens.total()),
            Field::ResponseWallClockS => match self.response_wall_clock_s {
                Some(v) => Value::from(v),
                None => Value::Null,
            },
            Field::LlmGenerationS => match self.llm_generation_s {
                Some(v) => Value::from(v),
                None => Value::Null,
            },
            Field::ToolLatencyBeforeS => match self.tool_latency_s_before {
                Some(v) => Value::from(v),
                None => Value::Null,
            },
        }
    }

    /// String representation for NUL output.
    ///
    /// Conventions:
    /// - `timestamp` → UTC float seconds since epoch (per cli-guidelines).
    /// - Token counts → raw integer string.
    /// - Optional fields (`provider_id`, `model`) → empty string when
    ///   missing (the field separator `\0` already disambiguates).
    /// - Path fields → lossy UTF-8 string (paths are filesystem
    ///   metadata; non-UTF-8 bytes degrade to U+FFFD).
    fn nul_value(&self, field: Field) -> String {
        match field {
            Field::Timestamp => {
                let secs = self.timestamp.timestamp() as f64
                    + self.timestamp.timestamp_subsec_nanos() as f64 / 1_000_000_000.0;
                format!("{}", secs)
            }
            Field::SessionId => self.session_id.clone(),
            Field::Provider => self.provider.to_string(),
            Field::ProviderId => self.provider_id.clone().unwrap_or_default(),
            Field::Model => self.model.clone().unwrap_or_default(),
            Field::Cwd => self.project.cwd.to_string_lossy().into_owned(),
            Field::ProjectPath => self.project.project_path.to_string_lossy().into_owned(),
            Field::Project => self.project.project.clone(),
            Field::Subpath => self.project.subpath.to_string_lossy().into_owned(),
            Field::Input => self.tokens.input.to_string(),
            Field::Output => self.tokens.output.to_string(),
            Field::CacheRead => self.tokens.cache_read.to_string(),
            Field::CacheWrite => self.tokens.cache_write.to_string(),
            Field::CacheCreation => self.tokens.cache_creation.to_string(),
            Field::Reasoning => self.tokens.reasoning.to_string(),
            Field::Total => self.tokens.total().to_string(),
            // NUL: empty string when missing (consistent with how
            // `provider_id` / `model` render `None`; the `\0`
            // separator still disambiguates the field boundary).
            Field::ResponseWallClockS => self
                .response_wall_clock_s
                .map(|v| format!("{}", v))
                .unwrap_or_default(),
            Field::LlmGenerationS => self
                .llm_generation_s
                .map(|v| format!("{}", v))
                .unwrap_or_default(),
            Field::ToolLatencyBeforeS => self
                .tool_latency_s_before
                .map(|v| format!("{}", v))
                .unwrap_or_default(),
        }
    }

    /// String representation for human display.
    ///
    /// - Timestamp: `%Y-%m-%dT%H:%M:%S` in local timezone.
    /// - Token counts: humanized via `format_tokens` (K/M/G).
    /// - Missing optional fields: `-`.
    fn human_value(&self, field: Field) -> String {
        match field {
            Field::Timestamp => {
                let local: DateTime<Local> = self.timestamp.with_timezone(&Local);
                local.format("%Y-%m-%dT%H:%M:%S").to_string()
            }
            Field::SessionId => self.session_id.clone(),
            Field::Provider => self.provider.to_string(),
            Field::ProviderId => self
                .provider_id
                .clone()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "-".to_string()),
            Field::Model => self
                .model
                .clone()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "-".to_string()),
            Field::Cwd => self.project.cwd.to_string_lossy().into_owned(),
            Field::ProjectPath => self.project.project_path.to_string_lossy().into_owned(),
            Field::Project => {
                let s = self.project.project.clone();
                if s.is_empty() {
                    "-".to_string()
                } else {
                    s
                }
            }
            Field::Subpath => {
                let s = self.project.subpath.to_string_lossy().into_owned();
                if s.is_empty() {
                    "-".to_string()
                } else {
                    s
                }
            }
            Field::Input => format_tokens(self.tokens.input),
            Field::Output => format_tokens(self.tokens.output),
            Field::CacheRead => format_tokens(self.tokens.cache_read),
            Field::CacheWrite => format_tokens(self.tokens.cache_write),
            Field::CacheCreation => format_tokens(self.tokens.cache_creation),
            Field::Reasoning => format_tokens(self.tokens.reasoning),
            Field::Total => format_tokens(self.tokens.total()),
            // Human: 3-decimal seconds; `-` when not derivable.
            Field::ResponseWallClockS => self
                .response_wall_clock_s
                .map(|v| format!("{:.3}", v))
                .unwrap_or_else(|| "-".to_string()),
            Field::LlmGenerationS => self
                .llm_generation_s
                .map(|v| format!("{:.3}", v))
                .unwrap_or_else(|| "-".to_string()),
            Field::ToolLatencyBeforeS => self
                .tool_latency_s_before
                .map(|v| format!("{:.3}", v))
                .unwrap_or_else(|| "-".to_string()),
        }
    }
}

// ---------------------------------------------------------------------------
// Latency derivation
// ---------------------------------------------------------------------------

/// For each assistant-message timestamp in `assistant_ts` (sorted
/// ascending, as returned by `list_messages()`), derive
/// `(response_wall_clock_s, tool_latency_s_before)` by walking
/// `entries` (sorted ascending by `timestamp`).
///
/// The first tuple member is the HARNESS-DEFINED wall-clock from
/// prior input marker to `target_ts`.  Whether this approximates
/// LLM-generation time is up to the caller: pi + claudecode put
/// `target_ts` at the *end* of the assistant turn (so it does),
/// opencode puts it at the *start* (so it does not — it includes
/// bundled tool runtime).  Computing the clean per-harness
/// `llm_generation_s` is the call site's responsibility, not this
/// helper's.
///
/// The window for message `M[i]` is `(M[i-1].ts, M[i].ts]` for tool
/// gaps, and `(-∞, M[i].ts)` for the LLM-latency anchor.  Using the
/// message-timestamp list (rather than per-entry assistant-role
/// scanning) avoids a cross-harness bear-trap: opencode emits multiple
/// transcript entries with DIFFERENT timestamps for the SAME logical
/// assistant message (text parts use `part.time.start`, tool parts
/// fall back to `msg.time.created` when `part.time.start` is null),
/// so per-entry turn-boundary detection would shatter one logical
/// turn into several false turns.
///
/// Pairing rules (locked here so future harness divergence has one
/// place to bite):
/// - **Input marker** = nearest prior `user.text` *or* `user.tool_result`
///   *or* `user.tool_error`.  `Thinking` is transparent.
/// - **Tool-gap accumulation** = within the window *(previous assistant
///   turn, this assistant turn⁻¹]* sum `result.ts − use.ts` for each
///   `assistant.tool_use` paired by **sequence order** with the next
///   `user.tool_result|tool_error` (the harnesses we ship today do not
///   all expose a stable `tool_use_id` on every entry).
/// - **`EntryType::ToolError`** closes a tool gap identically to
///   `ToolResult` — the tool ran and returned, success or not.
/// - **`EntryType::Thinking`** is transparent: skipped both when
///   locating the input marker and when accumulating tool gaps.
/// - **`EntryType::Error`** (message-level / API-level failure) clips
///   both latencies on the *next* assistant message to `None`: a failed
///   retry's wall-clock cost is not a clean LLM-latency signal.
/// - **Negative computed gap** (transcript timestamps non-monotonic):
///   emit `None` + `log::warn!`.  Do NOT clamp to zero — a clamp would
///   hide real harness-schema drift.
///
/// The two returned values are independent: a None on one does not
/// imply a None on the other (except in the `Error` case, which clips
/// both).
fn derive_latencies(
    entries: &[TranscriptEntry],
    assistant_ts: &[DateTime<Utc>],
) -> Vec<(Option<f64>, Option<f64>)> {
    let mut out: Vec<(Option<f64>, Option<f64>)> = Vec::with_capacity(assistant_ts.len());
    for (i, &ts) in assistant_ts.iter().enumerate() {
        // Previous assistant-message ts (or None for the first
        // message of the session) bounds the tool-gap search.
        let prev_ts: Option<DateTime<Utc>> = if i == 0 {
            None
        } else {
            Some(assistant_ts[i - 1])
        };
        out.push(derive_for_assistant_ts(entries, prev_ts, ts));
    }
    out
}

/// Derive `(response_wall_clock_s, tool_latency_s_before)` for the
/// assistant message at `target_ts`, with `prev_ts` bounding the
/// tool-gap search (the previous assistant message's
/// `Message.timestamp` from `list_messages()`, or `None` for the
/// first assistant message of a session).
///
/// The semantics, in plain language:
/// - `response_wall_clock_s` = `target_ts - last_input_marker_ts`,
///   where the last input marker is the most recent `User-Text` /
///   any-role `ToolResult` / any-role `ToolError` whose
///   `entry.timestamp < target_ts`.  `Thinking` entries are skipped.
/// - `tool_latency_s_before` = sum of `result.ts - use.ts` for
///   tool pairs where `tool_use.ts ∈ [prev_ts, target_ts)`
///   (inclusive on `prev_ts`, exclusive on `target_ts`) AND the
///   matching `tool_result|tool_error` lands at-or-before
///   `target_ts`.  When `prev_ts` is `None` (first assistant
///   message of the session), the lower bound is `-∞`.  Pairing
///   is FIFO by sequence order.  The window-shape is calibrated
///   for the cross-harness contract: pi + claudecode emit
///   `tool_use` ON the assistant message that issued it
///   (= `prev_ts` for the next message), so `[prev_ts, target_ts)`
///   includes that `tool_use`; opencode bundles `tool_use` AND
///   `tool_result` inside the SAME assistant message (both at
///   `target_ts`), so `[prev_ts, target_ts)` excludes them and no
///   inter-message tool latency is attributed — correct, because
///   the tool ran *during* this message's turn, not *before* it.
/// - `EntryType::Error` (message-level) anywhere in
///   `(prev_ts_inclusive, target_ts]` clips both fields to `None`.
fn derive_for_assistant_ts(
    entries: &[TranscriptEntry],
    prev_ts: Option<DateTime<Utc>>,
    target_ts: DateTime<Utc>,
) -> (Option<f64>, Option<f64>) {
    use std::collections::VecDeque;

    let mut last_input_marker_ts: Option<DateTime<Utc>> = None;
    let mut tool_use_queue: VecDeque<DateTime<Utc>> = VecDeque::new();
    let mut tool_gap_sum_s: f64 = 0.0;
    let mut tool_gap_count: usize = 0;
    let mut error_in_window: bool = false;

    // Lower-bound predicate for the per-message before-window.
    // `prev_ts = None` (first assistant message) → no lower bound.
    // INCLUSIVE on `prev_ts`: pi/claudecode emit tool_use at
    // `prev_ts` exactly (it's part of the prior assistant message),
    // and that tool's completion belongs to THIS message's window.
    let in_tool_window = |ts: DateTime<Utc>| -> bool {
        // Exclusive upper bound (target_ts itself belongs to the
        // current message, not its "before" window).
        if ts >= target_ts {
            return false;
        }
        match prev_ts {
            Some(p) => ts >= p,
            None => true,
        }
    };

    for entry in entries {
        // Everything past the target is irrelevant; transcripts are
        // sorted ascending by timestamp.
        if entry.timestamp > target_ts {
            break;
        }

        // Thinking is transparent everywhere.
        if matches!(entry.entry_type, EntryType::Thinking) {
            continue;
        }

        match entry.entry_type {
            // Message-level error — clips both fields when it lands
            // in (prev_ts_inclusive, target_ts].  We use a slightly
            // wider error window than the tool window (target_ts
            // included) to clip when this very assistant message
            // IS the error response.
            EntryType::Error => {
                let lower_ok = match prev_ts {
                    Some(p) => entry.timestamp >= p,
                    None => true,
                };
                if lower_ok {
                    error_in_window = true;
                }
            }

            // Assistant tool_use — push onto the FIFO if it lands in
            // the target's tool window `[prev_ts, target_ts)`.
            EntryType::ToolUse if matches!(entry.role, Role::Assistant) => {
                if in_tool_window(entry.timestamp) {
                    tool_use_queue.push_back(entry.timestamp);
                }
            }

            // Tool result / tool error — role-agnostic input marker
            // and FIFO tool-gap closer.  pi + claudecode emit these
            // with `Role::User`; opencode with `Role::Assistant`.  We
            // accept either; the entry-type is authoritative.
            EntryType::ToolResult | EntryType::ToolError => {
                if entry.timestamp < target_ts {
                    last_input_marker_ts = Some(entry.timestamp);
                }
                // Tool gap: result must be at-or-before target_ts and
                // strictly after prev_ts (or no lower bound when
                // prev_ts is None).  Use the same lower-bound rule
                // as the use side, but include results landing
                // exactly at target_ts (the message's `before`
                // window's upper edge is inclusive for results that
                // are themselves the input marker for this message).
                let result_lower_ok = match prev_ts {
                    Some(p) => entry.timestamp >= p,
                    None => true,
                };
                if result_lower_ok {
                    if let Some(use_ts) = tool_use_queue.pop_front() {
                        // Skip tool pairs whose use and result fall
                        // at the EXACT same timestamp.  This is the
                        // opencode-fallback signature: when
                        // `part.time.start` / `time.end` are null,
                        // the transcript parser collapses both entries
                        // to `msg.time.created`, producing a
                        // mathematically-zero gap that carries no
                        // real "tool ran for N seconds" signal.
                        // Genuine sub-millisecond tools would still
                        // differ by ≥1 chrono microsecond.
                        if entry.timestamp != use_ts {
                            tool_gap_sum_s += duration_secs(entry.timestamp - use_ts);
                            tool_gap_count += 1;
                        }
                    }
                }
            }

            // User text — always an input marker (only if strictly
            // earlier than the target).
            EntryType::Text if matches!(entry.role, Role::User) && entry.timestamp < target_ts => {
                last_input_marker_ts = Some(entry.timestamp);
            }

            _ => {}
        }
    }

    if error_in_window {
        return (None, None);
    }

    let llm = match last_input_marker_ts {
        None => None,
        Some(prev) => {
            let s = duration_secs(target_ts - prev);
            if s < 0.0 {
                log::warn!(
                    "derive_latencies: negative response_wall_clock_s ({} s) at {} — \
                     transcript timestamps non-monotonic; emitting null",
                    s,
                    target_ts
                );
                None
            } else {
                Some(s)
            }
        }
    };
    let tool = if tool_gap_count == 0 {
        None
    } else if tool_gap_sum_s < 0.0 {
        log::warn!(
            "derive_latencies: negative tool_latency_s_before ({} s) at {} — \
             transcript timestamps non-monotonic; emitting null",
            tool_gap_sum_s,
            target_ts
        );
        None
    } else {
        Some(tool_gap_sum_s)
    };
    (llm, tool)
}

/// Convert a `chrono::Duration` to fractional seconds without losing
/// sub-second precision when the duration fits in microseconds.
fn duration_secs(d: chrono::Duration) -> f64 {
    match d.num_microseconds() {
        Some(us) => us as f64 / 1_000_000.0,
        None => d.num_milliseconds() as f64 / 1_000.0,
    }
}

// ---------------------------------------------------------------------------
// Run entry
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub fn run(
    timespan: &str,
    sessions: Vec<String>,
    projects: Vec<String>,
    session_type: Option<SessionType>,
    provider_ids: Vec<String>,
    models: Vec<String>,
    fields: Option<Vec<String>>,
    header: bool,
    format: OutputFormat,
) -> Result<()> {
    // Resolve fields.
    let user_supplied_fields = fields.is_some();
    let selected_fields: Vec<Field> = match fields {
        Some(names) => parse_fields(&names)?,
        None => match format {
            OutputFormat::Human => DEFAULT_FIELDS_HUMAN.to_vec(),
            OutputFormat::Nul | OutputFormat::Json => ALL_FIELDS.to_vec(),
        },
    };

    // Resolve timespan to (start, end) UTC instants.
    let (start, end) = kal_time::parse_timespan(timespan)
        .map_err(|e| anyhow!("Failed to parse timespan '{}': {}", timespan, e))?;
    let start_utc = start.with_timezone(&chrono::Utc);
    let end_utc = end.with_timezone(&chrono::Utc);
    let start_secs = start.timestamp();
    let end_secs = end.timestamp();

    // Walk sessions matching primary filters.
    //
    // Note: we do NOT pass `--project` to `SessionFilter` because that
    // filter matches against `Session::project_dir` (raw cwd), whereas
    // our `project` filter matches against the *derived* project name
    // (basename of the .git ancestor).  We apply that filter manually
    // below.
    let session_id_filter: Option<String> = if sessions.len() == 1 {
        Some(sessions[0].clone())
    } else {
        None
    };
    let multi_session_filter: Option<Vec<String>> = if sessions.len() > 1 {
        Some(sessions)
    } else {
        None
    };
    let session_filter = SessionFilter {
        session_type,
        session_id: session_id_filter,
        project: None,
        search: Vec::new(),
        file: None,
        timespan: Some((start_secs, end_secs)),
        last_message_in: None,
        all: true,
        children_of: None,
        static_enrich: None,
        static_predicate: None,
        live_enrich: None,
        live_predicate: None,
    };
    let candidate_sessions = list_filtered(&session_filter)?;

    // Walk messages, building events.
    let mut events: Vec<TokenEvent> = Vec::new();
    for session in candidate_sessions {
        // Multi-session OR filter (single-session is already handled
        // by SessionFilter::session_id).
        if let Some(ids) = &multi_session_filter {
            if !ids.iter().any(|id| id == &session.base.session_id) {
                continue;
            }
        }

        let cwd = PathBuf::from(&session.base.project_dir);
        let proj = project_info_from_cwd(&cwd);

        // Project filter: exact match on derived `project` field.
        // Empty string matches sessions outside any git repo.
        if !projects.is_empty() && !projects.iter().any(|p| p == &proj.project) {
            continue;
        }

        let provider_adapter = match provider_for_session(&session.base.session_id) {
            Ok(p) => p,
            Err(e) => {
                log::warn!(
                    "Skipping session {}: provider resolution failed: {}",
                    session.base.session_id,
                    e
                );
                continue;
            }
        };
        let messages = provider_adapter.list_messages(&session.base.session_id)?;

        // Per-session transcript walk for latency derivation.  One
        // extra `parse_transcript` per session in the result window
        // — already amortised across many records per session and
        // bounded by the same `session_filter` the command enforces.
        // On parse failure we proceed with `None` latency fields
        // rather than dropping the whole session's token events.
        let transcript: Vec<TranscriptEntry> =
            match provider_adapter.parse_transcript(&session.base.session_id) {
                Ok(t) => t,
                Err(e) => {
                    log::warn!(
                        "Latency derivation skipped for session {}: parse_transcript failed: {}",
                        session.base.session_id,
                        e
                    );
                    Vec::new()
                }
            };
        // Collect ALL assistant-message timestamps for the session
        // (not just filtered ones): derivation needs the full window
        // context to detect turn boundaries and prior input markers,
        // even when the per-record `--from/--to` or `--model` filter
        // would exclude an earlier message.
        let assistant_ts: Vec<DateTime<chrono::Utc>> = messages
            .iter()
            .filter(|m| m.role == "assistant" && m.tokens.as_ref().is_some_and(|t| !t.is_empty()))
            .map(|m| m.timestamp)
            .collect();
        let latencies: Vec<(Option<f64>, Option<f64>)> = if transcript.is_empty() {
            vec![(None, None); assistant_ts.len()]
        } else {
            derive_latencies(&transcript, &assistant_ts)
        };
        // Map: assistant-message ts → (response_wall_clock_s,
        // tool_latency_s_before).  Using a Vec<(ts, lat)> rather than
        // a HashMap because the list is tiny per session and ordered
        // scans are cheaper than hashing chrono types.
        type LatencyPair = (Option<f64>, Option<f64>);
        let latency_by_ts: Vec<(DateTime<chrono::Utc>, LatencyPair)> =
            assistant_ts.into_iter().zip(latencies).collect();

        for msg in messages {
            if !message_passes_filters(&msg, start_utc, end_utc, &provider_ids, &models) {
                continue;
            }
            // Only assistant messages with non-empty token data.
            let Some(tokens) = msg.tokens.clone() else {
                continue;
            };
            if tokens.is_empty() {
                continue;
            }
            let (response_wall_clock_s, tool_latency_s_before) = latency_by_ts
                .iter()
                .find(|(ts, _)| *ts == msg.timestamp)
                .map(|(_, lat)| *lat)
                .unwrap_or((None, None));
            // Cross-harness `llm_generation_s` mapping.  Lives here
            // (the call site) rather than inside `derive_latencies`
            // so the derivation stays pure and per-harness policy
            // is one switch:
            // - pi + claudecode: `Message.timestamp` = end of
            //   assistant turn, tools execute in a separate
            //   user-role turn between messages.  So wall-clock IS
            //   generation time.
            // - opencode: `Message.timestamp = time.created` (start),
            //   tools execute as parts INSIDE the message.  The
            //   clean generation signal requires walking opencode's
            //   part-level `time.start`/`time.end` data — not yet
            //   implemented.  Emit `None` rather than a misleading
            //   approximation.  Tracked in admin.org.
            let llm_generation_s = match msg.provider {
                Provider::Pi | Provider::ClaudeCode => response_wall_clock_s,
                Provider::OpenCode => None,
            };
            events.push(TokenEvent {
                timestamp: msg.timestamp,
                session_id: msg.session_id,
                provider: msg.provider.as_str(),
                provider_id: msg.provider_id,
                model: msg.model,
                project: proj.clone(),
                tokens,
                response_wall_clock_s,
                llm_generation_s,
                tool_latency_s_before,
            });
        }
    }

    events.sort_by_key(|e| e.timestamp);

    match format {
        OutputFormat::Human => {
            render_human(&events, &selected_fields, header, !user_supplied_fields)
        }
        OutputFormat::Nul => render_nul(&events, &selected_fields),
        OutputFormat::Json => render_json(&events, &selected_fields),
    }
}

/// Parse a list of field-name strings into [`Field`] values, erroring
/// on unknown names.  Empty values (e.g. from a trailing comma) are
/// rejected.
fn parse_fields(names: &[String]) -> Result<Vec<Field>> {
    let mut out = Vec::with_capacity(names.len());
    for name in names {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return Err(anyhow!("empty field name in --fields"));
        }
        match Field::parse(trimmed) {
            Some(f) => out.push(f),
            None => {
                return Err(anyhow!(
                    "unknown field '{}'.  Available: {}",
                    trimmed,
                    ALL_FIELDS
                        .iter()
                        .map(|f| f.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            }
        }
    }
    Ok(out)
}

fn message_passes_filters(
    msg: &Message,
    start: DateTime<chrono::Utc>,
    end: DateTime<chrono::Utc>,
    provider_ids: &[String],
    models: &[String],
) -> bool {
    // Timestamp range: [start, end).  Matches `kal_time::parse_timespan`
    // semantics where `end` is exclusive.
    if msg.timestamp < start || msg.timestamp >= end {
        return false;
    }
    if !provider_ids.is_empty() {
        let m = msg.provider_id.as_deref();
        if !provider_ids.iter().any(|id| Some(id.as_str()) == m) {
            return false;
        }
    }
    if !models.is_empty() {
        let model = msg.model.as_deref().unwrap_or("");
        let model_lower = model.to_lowercase();
        if !models
            .iter()
            .any(|needle| model_lower.contains(&needle.to_lowercase()))
        {
            return false;
        }
    }
    true
}

// ---------------------------------------------------------------------------
// Renderers
// ---------------------------------------------------------------------------

fn render_human(
    events: &[TokenEvent],
    fields: &[Field],
    header: bool,
    show_summary: bool,
) -> Result<()> {
    let stdout = io::stdout();
    let mut out = stdout.lock();

    // Compute column widths (max of header width and value widths).
    let mut widths: Vec<usize> = fields.iter().map(|f| f.header().len()).collect();
    let mut rows: Vec<Vec<String>> = Vec::with_capacity(events.len());
    for event in events {
        let row: Vec<String> = fields.iter().map(|f| event.human_value(*f)).collect();
        for (i, value) in row.iter().enumerate() {
            if value.len() > widths[i] {
                widths[i] = value.len();
            }
        }
        rows.push(row);
    }

    let last_idx = fields.len().saturating_sub(1);
    let pad_field = |out: &mut dyn Write,
                     field: Field,
                     value: &str,
                     width: usize,
                     is_last: bool|
     -> io::Result<()> {
        if is_last {
            // Last column: no padding.
            write!(out, "{}", value)
        } else if field.is_token_count() {
            // Right-align token counts.
            write!(out, "{:>width$}", value, width = width)
        } else {
            // Left-align everything else.
            write!(out, "{:<width$}", value, width = width)
        }
    };

    if header {
        for (i, field) in fields.iter().enumerate() {
            pad_field(&mut out, *field, field.header(), widths[i], i == last_idx)?;
            if i != last_idx {
                write!(out, " ")?;
            }
        }
        writeln!(out)?;
    }

    for row in &rows {
        for (i, value) in row.iter().enumerate() {
            pad_field(&mut out, fields[i], value, widths[i], i == last_idx)?;
            if i != last_idx {
                write!(out, " ")?;
            }
        }
        writeln!(out)?;
    }

    if show_summary {
        let total_input: u64 = events.iter().map(|e| e.tokens.input).sum();
        let total_output: u64 = events.iter().map(|e| e.tokens.output).sum();
        let grand_total: u64 = events.iter().map(|e| e.tokens.total()).sum();
        let session_count = events
            .iter()
            .map(|e| e.session_id.clone())
            .collect::<std::collections::HashSet<_>>()
            .len();
        writeln!(
            out,
            "Total: {} events across {} sessions, {} input, {} output, {} total tokens",
            events.len(),
            session_count,
            format_tokens(total_input),
            format_tokens(total_output),
            format_tokens(grand_total),
        )?;
    }

    Ok(())
}

fn render_nul(events: &[TokenEvent], fields: &[Field]) -> Result<()> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    for event in events {
        for field in fields {
            write!(out, "{}\0", event.nul_value(*field))?;
        }
    }
    Ok(())
}

fn render_json(events: &[TokenEvent], fields: &[Field]) -> Result<()> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    for event in events {
        let mut map = serde_json::Map::with_capacity(fields.len());
        for field in fields {
            map.insert(field.as_str().to_string(), event.json_value(*field));
        }
        let line = serde_json::to_string(&serde_json::Value::Object(map))?;
        writeln!(out, "{}", line)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_parse_round_trip_all() {
        for field in ALL_FIELDS {
            let name = field.as_str();
            let parsed = Field::parse(name).unwrap_or_else(|| panic!("failed to parse '{}'", name));
            assert_eq!(parsed, *field);
        }
    }

    #[test]
    fn field_parse_unknown_returns_none() {
        assert!(Field::parse("not_a_field").is_none());
        assert!(Field::parse("").is_none());
        assert!(Field::parse("TIMESTAMP").is_none()); // case-sensitive
    }

    #[test]
    fn field_headers_uppercase() {
        for field in ALL_FIELDS {
            assert_eq!(field.header(), field.header().to_uppercase());
        }
    }

    #[test]
    fn field_token_count_classification() {
        assert!(Field::Input.is_token_count());
        assert!(Field::Total.is_token_count());
        assert!(Field::Reasoning.is_token_count());
        assert!(!Field::Timestamp.is_token_count());
        assert!(!Field::Project.is_token_count());
        assert!(!Field::Cwd.is_token_count());
    }

    #[test]
    fn parse_fields_rejects_empty() {
        assert!(parse_fields(&["".to_string()]).is_err());
        assert!(parse_fields(&["   ".to_string()]).is_err());
    }

    #[test]
    fn parse_fields_rejects_unknown() {
        let err = parse_fields(&["not_a_field".to_string()]).unwrap_err();
        assert!(err.to_string().contains("unknown field"));
        assert!(err.to_string().contains("not_a_field"));
    }

    #[test]
    fn parse_fields_accepts_valid() {
        let result = parse_fields(&["timestamp".to_string(), "session_id".to_string()]).unwrap();
        assert_eq!(result, vec![Field::Timestamp, Field::SessionId]);
    }

    #[test]
    fn default_fields_human_subset_of_all() {
        for f in DEFAULT_FIELDS_HUMAN {
            assert!(ALL_FIELDS.contains(f));
        }
    }

    #[test]
    fn latency_fields_are_in_all_fields_but_not_default_human() {
        // Per the TODO's "Open questions" resolution: latency fields
        // ship on the wire by default (JSON / NUL via ALL_FIELDS) but
        // stay off the default human view (opt-in via --fields).
        assert!(ALL_FIELDS.contains(&Field::ResponseWallClockS));
        assert!(ALL_FIELDS.contains(&Field::LlmGenerationS));
        assert!(ALL_FIELDS.contains(&Field::ToolLatencyBeforeS));
        assert!(!DEFAULT_FIELDS_HUMAN.contains(&Field::ResponseWallClockS));
        assert!(!DEFAULT_FIELDS_HUMAN.contains(&Field::LlmGenerationS));
        assert!(!DEFAULT_FIELDS_HUMAN.contains(&Field::ToolLatencyBeforeS));
    }

    #[test]
    fn latency_field_parse_round_trip() {
        assert_eq!(
            Field::parse("response_wall_clock_s"),
            Some(Field::ResponseWallClockS)
        );
        assert_eq!(
            Field::parse("llm_generation_s"),
            Some(Field::LlmGenerationS)
        );
        assert_eq!(
            Field::parse("tool_latency_s_before"),
            Some(Field::ToolLatencyBeforeS)
        );
    }

    // -----------------------------------------------------------------
    // derive_latencies
    // -----------------------------------------------------------------

    use chrono::TimeZone;

    fn ts(secs: i64) -> chrono::DateTime<chrono::Utc> {
        chrono::Utc.timestamp_opt(secs, 0).single().unwrap()
    }

    fn entry(
        sec: i64,
        role: Role,
        entry_type: EntryType,
        tool_name: Option<&str>,
    ) -> TranscriptEntry {
        TranscriptEntry {
            timestamp: ts(sec),
            role,
            entry_type,
            content: String::new(),
            tool_name: tool_name.map(|s| s.to_string()),
            tool_input: None,
        }
    }

    #[test]
    fn derive_basic_user_text_to_assistant() {
        // user.text @ 100, assistant.text @ 105 — latency = 5.0 s,
        // no tools intervened so tool_latency_s_before = None.
        let entries = vec![
            entry(100, Role::User, EntryType::Text, None),
            entry(105, Role::Assistant, EntryType::Text, None),
        ];
        let out = derive_latencies(&entries, &[ts(105)]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, Some(5.0));
        assert_eq!(out[0].1, None);
    }

    #[test]
    fn derive_first_message_null_both_fields() {
        // First assistant message of a session: no prior input
        // marker, so both fields are None.
        let entries = vec![entry(50, Role::Assistant, EntryType::Text, None)];
        let out = derive_latencies(&entries, &[ts(50)]);
        assert_eq!(out, vec![(None, None)]);
    }

    #[test]
    fn derive_pi_claudecode_two_tools_in_one_before_window() {
        // pi/claudecode shape: tool_use lands ON the assistant
        // message that issued it; tool_result lands in a separate
        // user-role message between turns.  Four assistant messages:
        //
        //   0  user.text
        //   1  assistant.text          [M0]
        //  10  user.text
        //  11  assistant.tool_use(A)   [M1]
        //  13  user.tool_result(A)
        //  14  assistant.tool_use(B)   [M2]
        //  18  user.tool_result(B)
        //  20  assistant.text          [M3, TARGET]
        //
        // For M3, prev_ts = M2.ts = 14, tool window = [14, 20).
        //   * tool_use A @ 11 → 11 < 14, OUT.
        //   * tool_use B @ 14 → IN (lower bound inclusive).
        //   * tool_result A @ 13 → 13 < 14, OUT.
        //   * tool_result B @ 18 → 13 ≤ 18 < 20, IN → closes B's use.
        // tool_latency_s_before(M3) = 18 − 14 = 4.0.
        // response_wall_clock_s(M3) = 20 − 18 (last tool_result < 20) = 2.0.
        let entries = vec![
            entry(0, Role::User, EntryType::Text, None),
            entry(1, Role::Assistant, EntryType::Text, None),
            entry(10, Role::User, EntryType::Text, None),
            entry(11, Role::Assistant, EntryType::ToolUse, Some("A")),
            entry(13, Role::User, EntryType::ToolResult, Some("A")),
            entry(14, Role::Assistant, EntryType::ToolUse, Some("B")),
            entry(18, Role::User, EntryType::ToolResult, Some("B")),
            entry(20, Role::Assistant, EntryType::Text, None),
        ];
        let out = derive_latencies(&entries, &[ts(1), ts(11), ts(14), ts(20)]);
        assert_eq!(out.len(), 4);
        // M0: input marker = user.text @ 0. llm = 1 − 0 = 1.0.
        // No tool calls before → tool = None.
        assert_eq!(out[0], (Some(1.0), None), "M0");
        // M1: prev=1, window=[1, 11).  No completed tool gap in that
        // window (A's result comes at 13).  Input marker = user.text
        // @ 10. llm = 11 − 10 = 1.0.
        assert_eq!(out[1].0, Some(1.0), "M1 llm");
        assert_eq!(out[1].1, None, "M1 tool (A still running)");
        // M2: prev=11, window=[11, 14).  tool_use A @ 11 IN,
        // tool_result A @ 13 IN → pair closes, gap = 2.
        // Input marker before 14: tool_result A @ 13. llm = 14 − 13 = 1.
        assert_eq!(out[2].0, Some(1.0), "M2 llm");
        assert_eq!(out[2].1, Some(2.0), "M2 tool");
        // M3 (target): see comment above.
        assert_eq!(out[3].0, Some(2.0), "M3 llm");
        assert_eq!(out[3].1, Some(4.0), "M3 tool");
    }

    #[test]
    fn derive_multiple_tools_in_single_window() {
        // Distinct case: two tool calls land in the SAME
        // before-window (i.e. the prior assistant message is the one
        // that issued both tool calls, before any intervening
        // assistant text/tool_use).  Here Pi-style: a single
        // assistant message at ts=11 emits TWO toolCall blocks at the
        // same timestamp; their results return separately.
        //
        // Timeline:
        //  10  user.text
        //  11  assistant.tool_use(A)        [prior msg, shared ts]
        //  11  assistant.tool_use(B)        [prior msg, shared ts]
        //  13  user.tool_result(A)          (gap A = 2)
        //  18  user.tool_result(B)          (gap B = 7)
        //  20  assistant.text               [TARGET]
        //
        // tool_latency_s_before = 2 + 7 = 9.0
        // response_wall_clock_s = 20 − 18 = 2.0
        let entries = vec![
            entry(10, Role::User, EntryType::Text, None),
            entry(11, Role::Assistant, EntryType::ToolUse, Some("A")),
            entry(11, Role::Assistant, EntryType::ToolUse, Some("B")),
            entry(13, Role::User, EntryType::ToolResult, Some("A")),
            entry(18, Role::User, EntryType::ToolResult, Some("B")),
            entry(20, Role::Assistant, EntryType::Text, None),
        ];
        let out = derive_latencies(&entries, &[ts(20)]);
        assert_eq!(out[0].0, Some(2.0), "response_wall_clock_s");
        assert_eq!(out[0].1, Some(9.0), "tool_latency_s_before");
    }

    #[test]
    fn derive_thinking_entries_transparent() {
        // Thinking entries between user and assistant don't reset the
        // anchor and don't count toward tool-time.
        let entries = vec![
            entry(100, Role::User, EntryType::Text, None),
            entry(102, Role::Assistant, EntryType::Thinking, None),
            entry(103, Role::Assistant, EntryType::Thinking, None),
            entry(105, Role::Assistant, EntryType::Text, None),
        ];
        let out = derive_latencies(&entries, &[ts(105)]);
        // Anchor stays at the user.text @ 100 (Thinking transparent).
        assert_eq!(out[0].0, Some(5.0));
        assert_eq!(out[0].1, None);
    }

    #[test]
    fn derive_after_message_error_clips_to_none() {
        // user.text @ 100, error @ 102, assistant.text @ 105
        //   → both fields None (don't conflate failed-retry wall
        //     clock with LLM latency).
        let entries = vec![
            entry(100, Role::User, EntryType::Text, None),
            entry(102, Role::Assistant, EntryType::Error, None),
            entry(105, Role::Assistant, EntryType::Text, None),
        ];
        let out = derive_latencies(&entries, &[ts(105)]);
        assert_eq!(out, vec![(None, None)]);
    }

    #[test]
    fn derive_tool_error_closes_gap_like_tool_result() {
        // assistant.tool_use @ 11, user.tool_error @ 14 — the failed
        // tool still ran for 3.0 s and that counts toward
        // tool_latency_s_before of the next assistant turn.
        let entries = vec![
            entry(0, Role::User, EntryType::Text, None),
            entry(1, Role::Assistant, EntryType::Text, None),
            entry(10, Role::User, EntryType::Text, None),
            entry(11, Role::Assistant, EntryType::ToolUse, Some("A")),
            entry(14, Role::User, EntryType::ToolError, Some("A")),
            entry(15, Role::Assistant, EntryType::Text, None),
        ];
        let out = derive_latencies(&entries, &[ts(15)]);
        assert_eq!(out[0].1, Some(3.0), "tool_latency_s_before");
        // Last input marker = tool_error @ 14, so llm = 15 − 14 = 1.0.
        assert_eq!(out[0].0, Some(1.0), "response_wall_clock_s");
    }

    #[test]
    fn derive_negative_gap_returns_none() {
        // Non-monotonic timestamps (clock skew / schema drift):
        // emit None rather than clamp to zero.
        let entries = vec![
            entry(200, Role::User, EntryType::Text, None),
            entry(100, Role::Assistant, EntryType::Text, None), // earlier!
        ];
        let out = derive_latencies(&entries, &[ts(100)]);
        assert_eq!(out[0].0, None);
        assert_eq!(out[0].1, None);
    }

    #[test]
    fn derive_two_turns_no_bleed_between_windows() {
        // Two assistant turns, each with a tool call.  The second
        // turn's `tool_latency_s_before` must NOT include the first
        // turn's tool gap.
        let entries = vec![
            entry(0, Role::User, EntryType::Text, None),
            entry(1, Role::Assistant, EntryType::ToolUse, Some("A")),
            entry(5, Role::User, EntryType::ToolResult, Some("A")),
            entry(6, Role::Assistant, EntryType::Text, None),
            entry(10, Role::User, EntryType::Text, None),
            entry(11, Role::Assistant, EntryType::ToolUse, Some("B")),
            entry(13, Role::User, EntryType::ToolResult, Some("B")),
            entry(14, Role::Assistant, EntryType::Text, None),
        ];
        let out = derive_latencies(&entries, &[ts(6), ts(14)]);
        // Turn 1: tool A ran 4 s; llm = 6 − 5 = 1.
        assert_eq!(out[0].0, Some(1.0));
        assert_eq!(out[0].1, Some(4.0));
        // Turn 2: tool B ran 2 s (NOT 4+2=6); llm = 14 − 13 = 1.
        assert_eq!(out[1].0, Some(1.0));
        assert_eq!(
            out[1].1,
            Some(2.0),
            "prior turn's tool gap must not bleed in"
        );
    }

    #[test]
    fn derive_skips_tool_pair_with_identical_timestamps() {
        // OpenCode's transcript parser falls back to msg.time.created
        // when part.time.start/end are null, which collapses both the
        // tool_use and tool_result entries to the same timestamp.
        // That zero-width pair carries no real "tool ran for N s"
        // signal — skip it rather than emitting a spurious 0.0.
        let entries = vec![
            entry(10, Role::User, EntryType::Text, None),
            entry(11, Role::Assistant, EntryType::ToolUse, Some("A")),
            entry(11, Role::Assistant, EntryType::ToolResult, Some("A")),
            entry(20, Role::Assistant, EntryType::Text, None),
        ];
        let out = derive_latencies(&entries, &[ts(11), ts(20)]);
        // M_target (20): the only tool pair has use_ts==result_ts==11,
        // so it is skipped; tool_latency_s_before = None.
        assert_eq!(out[1].1, None);
        // response_wall_clock_s still computed normally: last input marker
        // (tool_result @ 11) is the most recent marker < 20.
        assert_eq!(out[1].0, Some(9.0));
    }

    #[test]
    fn derive_opencode_style_assistant_role_tool_result_closes_gap() {
        // OpenCode's transcript parser emits tool_result / tool_error
        // entries with `Role::Assistant` (its part model nests tool
        // parts inside the assistant message that owns them), whereas
        // pi + claudecode emit them with `Role::User`.  Latency
        // derivation must accept BOTH — the entry_type is the
        // authoritative signal.  This test pins that cross-harness
        // invariant.
        //
        // Timeline:
        //  10  user.text
        //  11  assistant.tool_use(A)
        //  13  assistant.tool_result(A)    [role=Assistant, opencode]
        //  20  assistant.text               [TARGET]
        //
        // tool_latency_s_before = 13 − 11 = 2.0
        // response_wall_clock_s         = 20 − 13 = 7.0
        let entries = vec![
            entry(10, Role::User, EntryType::Text, None),
            entry(11, Role::Assistant, EntryType::ToolUse, Some("A")),
            entry(13, Role::Assistant, EntryType::ToolResult, Some("A")),
            entry(20, Role::Assistant, EntryType::Text, None),
        ];
        let out = derive_latencies(&entries, &[ts(20)]);
        assert_eq!(out[0].0, Some(7.0), "response_wall_clock_s");
        assert_eq!(out[0].1, Some(2.0), "tool_latency_s_before");
    }

    #[test]
    fn derive_assistant_split_into_text_plus_tool_use_same_ts() {
        // Pi can emit an assistant turn as multiple TranscriptEntries
        // with the same timestamp (text + toolCall blocks of the
        // same message).  Both share a single ts; we should match
        // exactly once, not double-count, and not treat the toolUse
        // as opening a new turn.
        let entries = vec![
            entry(100, Role::User, EntryType::Text, None),
            entry(105, Role::Assistant, EntryType::Text, None),
            entry(105, Role::Assistant, EntryType::ToolUse, Some("A")),
        ];
        let out = derive_latencies(&entries, &[ts(105)]);
        assert_eq!(out[0].0, Some(5.0));
        // Tool A hasn't returned yet — no completed tool gap before
        // this assistant message.
        assert_eq!(out[0].1, None);
    }
}

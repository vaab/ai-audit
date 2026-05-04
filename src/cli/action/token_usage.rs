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
use chrono::{DateTime, Local};

use super::super::def::SessionType;
use crate::format::format_tokens;
use crate::project::{project_info_from_cwd, ProjectInfo};
use crate::provider::{provider_for_session, Message, TokenUsage};
use crate::session_filter::{list_filtered, SessionFilter};
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
];

/// Default field set for human display (compact, fits a terminal).
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
        }
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
        search: None,
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

        let messages = match provider_for_session(&session.base.session_id) {
            Ok(p) => p.list_messages(&session.base.session_id)?,
            Err(e) => {
                log::warn!(
                    "Skipping session {}: provider resolution failed: {}",
                    session.base.session_id,
                    e
                );
                continue;
            }
        };

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
            events.push(TokenEvent {
                timestamp: msg.timestamp,
                session_id: msg.session_id,
                provider: msg.provider.as_str(),
                provider_id: msg.provider_id,
                model: msg.model,
                project: proj.clone(),
                tokens,
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
}

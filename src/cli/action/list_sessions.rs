//! List sessions command handler.

use anyhow::Result;
use colored::Colorize;
use serde::Serialize;

use super::super::def::{LiveStatusArg, SessionStatusOpts, SessionType, StaticStatusArg};
use super::require_opencode_for;
use crate::config::Config;
use crate::opencode::enrich::{
    extract_live, extract_static, live_status_predicate, make_live_enricher, make_static_enricher,
    static_status_predicate,
};
use crate::opencode::server_client::{resolve_server_credentials, LiveStatus};
use crate::opencode::status::StaticStatus;
use crate::provider::detect_provider;
use crate::session_filter::{canonicalize_filter_path, list_filtered, SessionFilter};
use crate::OutputFormat;

#[derive(Debug, Serialize)]
struct SessionRecord {
    timestamp: f64,
    session_id: String,
    #[serde(rename = "type")]
    session_type: &'static str,
    project_dir: String,
    title: String,
    static_status: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    live_status: Option<&'static str>,
}

// The dispatch layer (cli/action/mod.rs) calls this with many parameters
// that mirror the clap Command::ListSessions variant fields. Refactoring
// to a struct would require changing the dispatcher contract; defer.
#[allow(clippy::too_many_arguments)]
pub fn run(
    session_type: Option<SessionType>,
    session_id: Option<&str>,
    search: Option<&str>,
    timespan: Option<&str>,
    project: Option<&str>,
    file: Option<&str>,
    all: bool,
    children_of: Option<&str>,
    status: &SessionStatusOpts,
    format: OutputFormat,
    _quiet: bool,
) -> Result<()> {
    let config = Config::load()?;
    let static_statuses = static_statuses(status);
    let live_statuses = live_statuses(status);
    let wants_status_features = static_statuses.is_some()
        || status.resumable
        || status.last_message_in.is_some()
        || status.output_live_status
        || live_statuses.is_some();
    let resolved_type = resolve_session_type(session_type, session_id, wants_status_features)?;
    let wants_live = status.output_live_status || live_statuses.is_some();
    let filter = SessionFilter {
        session_type: resolved_type,
        session_id: session_id.map(str::to_string),
        project: project.map(canonicalize_filter_path),
        search: search.map(str::to_string),
        file: file.map(canonicalize_filter_path),
        timespan: parse_timespan(timespan)?,
        last_message_in: parse_timespan(status.last_message_in.as_deref())?,
        all,
        children_of: children_of.map(str::to_string),
        static_enrich: wants_status_features.then(make_static_enricher),
        static_predicate: static_statuses.map(static_status_predicate),
        live_enrich: wants_live.then(|| {
            make_live_enricher(resolve_server_credentials(
                status.server_url.as_deref(),
                status.server_password.as_deref(),
                &config,
            ))
        }),
        live_predicate: live_statuses.map(live_status_predicate),
    };
    let sessions = list_filtered(&filter)?;
    let records = sessions
        .iter()
        .map(|session| SessionRecord {
            timestamp: session.base.started_at.timestamp() as f64
                + session.base.started_at.timestamp_subsec_nanos() as f64 / 1_000_000_000.0,
            session_id: session.base.session_id.clone(),
            session_type: session.base.provider.as_str(),
            project_dir: session.base.project_dir.clone(),
            title: session.base.title.clone(),
            static_status: extract_static(session).map(|extension| extension.status.as_str()),
            live_status: if wants_live {
                extract_live(session).map(|extension| extension.status.as_str())
            } else {
                None
            },
        })
        .collect::<Vec<_>>();

    output_records(&records, resolved_type, project, wants_live, format)
}

fn resolve_session_type(
    session_type: Option<SessionType>,
    session_id: Option<&str>,
    wants_status_features: bool,
) -> Result<Option<SessionType>> {
    if !wants_status_features {
        return Ok(session_type);
    }
    if session_type == Some(SessionType::ClaudeCode) {
        require_opencode_for("status")?;
    }
    if session_id.is_some_and(|id| detect_provider(id) == crate::provider::Provider::ClaudeCode) {
        require_opencode_for("status")?;
    }
    Ok(Some(SessionType::OpenCode))
}

fn static_statuses(status: &SessionStatusOpts) -> Option<Vec<StaticStatus>> {
    let statuses = status.status.as_ref().map(|values| {
        values
            .iter()
            .copied()
            .map(map_static_status)
            .collect::<Vec<_>>()
    });
    if status.resumable {
        return Some(StaticStatus::resumable_set());
    }
    statuses
}

fn live_statuses(status: &SessionStatusOpts) -> Option<Vec<LiveStatus>> {
    status
        .live_status
        .as_ref()
        .map(|values| values.iter().copied().map(map_live_status).collect())
}

fn map_static_status(value: StaticStatusArg) -> StaticStatus {
    match value {
        StaticStatusArg::Completed => StaticStatus::Completed,
        StaticStatusArg::UserPending => StaticStatus::UserPending,
        StaticStatusArg::AssistantEmpty => StaticStatus::AssistantEmpty,
        StaticStatusArg::AssistantPartial => StaticStatus::AssistantPartial,
        StaticStatusArg::AssistantToolStuck => StaticStatus::AssistantToolStuck,
    }
}

fn map_live_status(value: LiveStatusArg) -> LiveStatus {
    match value {
        LiveStatusArg::Running => LiveStatus::Running,
        LiveStatusArg::Idle => LiveStatus::Idle,
        LiveStatusArg::ServerUnreachable => LiveStatus::ServerUnreachable,
    }
}

fn parse_timespan(input: Option<&str>) -> Result<Option<(i64, i64)>> {
    input
        .map(|value| {
            kal_time::parse_timespan(value)
                .map(|(start, end)| (start.timestamp(), end.timestamp()))
                .map_err(|error| anyhow::anyhow!("Failed to parse timespan '{}': {}", value, error))
        })
        .transpose()
}

fn output_records(
    records: &[SessionRecord],
    session_type: Option<SessionType>,
    project: Option<&str>,
    wants_live: bool,
    format: OutputFormat,
) -> Result<()> {
    match format {
        OutputFormat::Json => {
            for record in records {
                println!("{}", serde_json::to_string(record)?);
            }
        }
        OutputFormat::Nul => {
            use std::io::{self, Write};
            let stdout = io::stdout();
            let mut handle = stdout.lock();
            for record in records {
                if wants_live {
                    write!(
                        handle,
                        "{}\0{}\0{}\0{}\0{}\0{}\0{}\0",
                        record.timestamp,
                        record.session_id,
                        record.session_type,
                        record.project_dir,
                        record.title,
                        record.static_status.unwrap_or(""),
                        record.live_status.unwrap_or(""),
                    )?;
                } else {
                    write!(
                        handle,
                        "{}\0{}\0{}\0{}\0{}\0{}\0",
                        record.timestamp,
                        record.session_id,
                        record.session_type,
                        record.project_dir,
                        record.title,
                        record.static_status.unwrap_or(""),
                    )?;
                }
            }
        }
        OutputFormat::Human => {
            if records.is_empty() {
                return Ok(());
            }
            let home_dir = dirs::home_dir().unwrap_or_default();
            let home_prefix = format!("{}/", home_dir.display());
            let to_local = |ts: f64| -> chrono::DateTime<chrono::Local> {
                chrono::DateTime::from_timestamp(ts as i64, 0)
                    .unwrap_or_default()
                    .with_timezone(&chrono::Local)
            };
            let same_day = if records.len() > 1 {
                to_local(records[0].timestamp).date_naive()
                    == to_local(records[records.len() - 1].timestamp).date_naive()
            } else {
                true
            };
            let ts_fmt = if same_day {
                "%H:%M:%S"
            } else {
                "%Y-%m-%dT%H:%M:%S"
            };
            let show_type = session_type.is_none()
                && records
                    .iter()
                    .any(|record| record.session_type != records[0].session_type);
            let show_dir = project.is_none()
                && records
                    .iter()
                    .any(|record| record.project_dir != records[0].project_dir);
            let show_static = records
                .iter()
                .filter_map(|record| record.static_status)
                .collect::<std::collections::HashSet<_>>()
                .len()
                > 1;
            for record in records {
                let mut parts = vec![
                    to_local(record.timestamp)
                        .format(ts_fmt)
                        .to_string()
                        .cyan()
                        .to_string(),
                    record.session_id.yellow().to_string(),
                ];
                if show_type {
                    parts.push(record.session_type.purple().to_string());
                }
                if show_static {
                    parts.push(record.static_status.unwrap_or("-").green().to_string());
                }
                if wants_live {
                    parts.push(record.live_status.unwrap_or("-").magenta().to_string());
                }
                if show_dir {
                    let dir = if record.project_dir.starts_with(&home_prefix) {
                        format!("~/{}", &record.project_dir[home_prefix.len()..])
                    } else if record.project_dir == home_dir.to_string_lossy() {
                        "~".to_string()
                    } else {
                        record.project_dir.clone()
                    };
                    parts.push(dir.blue().to_string());
                }
                parts.push(record.title.white().bold().to_string());
                println!("{}", parts.join(" "));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use crate::cli::def::{Args, Commands};

    #[test]
    fn cli_accepts_static_status_csv() {
        let args = Args::try_parse_from([
            "ai-audit",
            "list-sessions",
            "--status",
            "user-pending,assistant-empty",
        ])
        .unwrap();
        match args.command {
            Commands::ListSessions { status, .. } => {
                assert_eq!(status.status.unwrap().len(), 2);
            }
            _ => panic!("expected list-sessions"),
        }
    }

    #[test]
    fn cli_rejects_live_value_in_static_status() {
        assert!(
            Args::try_parse_from(["ai-audit", "list-sessions", "--status", "running"]).is_err()
        );
    }

    #[test]
    fn cli_accepts_live_status_csv() {
        let args = Args::try_parse_from([
            "ai-audit",
            "list-sessions",
            "--filter-by-live-status",
            "running,idle",
        ])
        .unwrap();
        match args.command {
            Commands::ListSessions { status, .. } => {
                assert_eq!(status.live_status.unwrap().len(), 2);
            }
            _ => panic!("expected list-sessions"),
        }
    }

    #[test]
    fn cli_rejects_static_value_in_live_status() {
        assert!(Args::try_parse_from([
            "ai-audit",
            "list-sessions",
            "--filter-by-live-status",
            "user-pending",
        ])
        .is_err());
    }
}

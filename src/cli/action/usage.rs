//! Token usage command handler.

use anyhow::Result;
use serde::Serialize;

use super::super::def::{LiveStatusArg, SessionStatusOpts, SessionType, StaticStatusArg};
use super::require_opencode_for;
use crate::config::Config;
use crate::format::format_tokens;
use crate::opencode::enrich::{
    extract_live, extract_static, live_status_predicate, make_live_enricher, make_static_enricher,
    static_status_predicate,
};
use crate::opencode::server_client::{resolve_server_credentials, LiveStatus};
use crate::opencode::status::StaticStatus;
use crate::provider::{detect_provider, provider_for_session, TokenUsage};
use crate::session_filter::{canonicalize_filter_path, list_filtered, SessionFilter};
use crate::OutputFormat;

#[derive(Debug, Serialize)]
struct SessionUsage {
    timestamp: f64,
    session_id: String,
    provider: &'static str,
    #[serde(flatten)]
    tokens: TokenUsage,
    #[serde(rename = "total")]
    total_tokens: u64,
    #[serde(rename = "messages")]
    message_count: usize,
    is_sub_agent: bool,
    static_status: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    live_status: Option<&'static str>,
}

pub fn run(
    session: Option<String>,
    session_type: Option<SessionType>,
    timespan: Option<&str>,
    project: Option<&str>,
    status: &SessionStatusOpts,
    format: OutputFormat,
    quiet: bool,
) -> Result<()> {
    if let Some(session_id) = session {
        return run_single(&session_id, status, format);
    }
    run_aggregated(session_type, timespan, project, status, format, quiet)
}

fn run_single(session_id: &str, status: &SessionStatusOpts, format: OutputFormat) -> Result<()> {
    let wants_status_features = status.status.is_some()
        || status.resumable
        || status.last_message_in.is_some()
        || status.output_live_status
        || status.live_status.is_some();
    if wants_status_features {
        match detect_provider(session_id)? {
            crate::provider::Provider::OpenCode => {}
            crate::provider::Provider::ClaudeCode | crate::provider::Provider::Pi => {
                return require_opencode_for("status");
            }
        }
    }
    let provider = provider_for_session(session_id)?;
    let messages = provider.list_messages(session_id)?;
    let total_tokens: TokenUsage = messages
        .iter()
        .filter_map(|message| message.tokens.clone())
        .sum();
    let user_count = messages
        .iter()
        .filter(|message| message.role == "user")
        .count();
    let assistant_count = messages
        .iter()
        .filter(|message| message.role == "assistant")
        .count();
    match format {
        OutputFormat::Json => {
            println!(
                "{}",
                serde_json::to_string(&serde_json::json!({
                    "session_id": session_id,
                    "provider": provider.provider().as_str(),
                    "messages": messages.len(),
                    "user_messages": user_count,
                    "assistant_messages": assistant_count,
                    "input": total_tokens.input,
                    "output": total_tokens.output,
                    "cache_read": total_tokens.cache_read,
                    "cache_write": total_tokens.cache_write,
                    "cache_creation": total_tokens.cache_creation,
                    "reasoning": total_tokens.reasoning,
                    "total": total_tokens.total(),
                }))?
            );
        }
        OutputFormat::Nul => {
            print!(
                "{}\0{}\0{}\0{}\0{}\0",
                session_id,
                provider.provider().as_str(),
                total_tokens.input,
                total_tokens.output,
                total_tokens.total(),
            );
        }
        OutputFormat::Human => {
            println!("Session: {} ({})", session_id, provider.provider());
            println!(
                "Messages: {} (user: {}, assistant: {})",
                messages.len(),
                user_count,
                assistant_count
            );
            println!();
            println!("Token Usage:");
            println!(
                "  Input:          {:>12}",
                format_tokens(total_tokens.input)
            );
            println!(
                "  Output:         {:>12}",
                format_tokens(total_tokens.output)
            );
            println!(
                "  Cache read:     {:>12}",
                format_tokens(total_tokens.cache_read)
            );
            println!(
                "  Cache write:    {:>12}",
                format_tokens(total_tokens.cache_write)
            );
            println!(
                "  Cache creation: {:>12}",
                format_tokens(total_tokens.cache_creation)
            );
            println!(
                "  Reasoning:      {:>12}",
                format_tokens(total_tokens.reasoning)
            );
            println!("  {}", "\u{2500}".repeat(17));
            println!(
                "  Total:          {:>12}",
                format_tokens(total_tokens.total())
            );
        }
    }
    Ok(())
}

fn run_aggregated(
    session_type: Option<SessionType>,
    timespan: Option<&str>,
    project: Option<&str>,
    status: &SessionStatusOpts,
    format: OutputFormat,
    _quiet: bool,
) -> Result<()> {
    let config = Config::load()?;
    let static_statuses = status.status.as_ref().map(|values| {
        values
            .iter()
            .copied()
            .map(|value| match value {
                StaticStatusArg::Completed => StaticStatus::Completed,
                StaticStatusArg::UserPending => StaticStatus::UserPending,
                StaticStatusArg::AssistantEmpty => StaticStatus::AssistantEmpty,
                StaticStatusArg::AssistantPartial => StaticStatus::AssistantPartial,
                StaticStatusArg::AssistantToolStuck => StaticStatus::AssistantToolStuck,
            })
            .collect::<Vec<_>>()
    });
    let static_statuses = if status.resumable {
        Some(StaticStatus::resumable_set())
    } else {
        static_statuses
    };
    let live_statuses = status.live_status.as_ref().map(|values| {
        values
            .iter()
            .copied()
            .map(|value| match value {
                LiveStatusArg::Running => LiveStatus::Running,
                LiveStatusArg::Idle => LiveStatus::Idle,
                LiveStatusArg::ServerUnreachable => LiveStatus::ServerUnreachable,
            })
            .collect::<Vec<_>>()
    });
    let wants_status_features = static_statuses.is_some()
        || status.last_message_in.is_some()
        || status.output_live_status
        || live_statuses.is_some();
    if (session_type == Some(SessionType::ClaudeCode) || session_type == Some(SessionType::Pi))
        && wants_status_features
    {
        return require_opencode_for("status");
    }
    let wants_live = status.output_live_status || live_statuses.is_some();
    let sessions = list_filtered(&SessionFilter {
        session_type: if wants_status_features {
            Some(SessionType::OpenCode)
        } else {
            session_type
        },
        session_id: None,
        project: project.map(canonicalize_filter_path),
        search: None,
        file: None,
        timespan: parse_timespan(timespan)?,
        last_message_in: parse_timespan(status.last_message_in.as_deref())?,
        all: true,
        children_of: None,
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
    })?;

    let mut usage_records = Vec::new();
    for session in sessions {
        let provider = provider_for_session(&session.base.session_id)?;
        let messages = provider.list_messages(&session.base.session_id)?;
        let tokens: TokenUsage = messages
            .iter()
            .filter_map(|message| message.tokens.clone())
            .sum();
        if tokens.is_empty() {
            continue;
        }
        let static_status = extract_static(&session).map(|extension| extension.status.as_str());
        let live_status = if wants_live {
            extract_live(&session).map(|extension| extension.status.as_str())
        } else {
            None
        };
        usage_records.push(SessionUsage {
            timestamp: session.base.started_at.timestamp() as f64
                + session.base.started_at.timestamp_subsec_nanos() as f64 / 1_000_000_000.0,
            session_id: session.base.session_id.clone(),
            provider: session.base.provider.as_str(),
            total_tokens: tokens.total(),
            tokens,
            message_count: messages.len(),
            is_sub_agent: session.base.parent_id.is_some(),
            static_status,
            live_status,
        });
    }

    usage_records.sort_by(|left, right| left.timestamp.partial_cmp(&right.timestamp).unwrap());
    output_records(&usage_records, wants_live, format)
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

fn output_records(records: &[SessionUsage], wants_live: bool, format: OutputFormat) -> Result<()> {
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
                        "{}\0{}\0{}\0{}\0{}\0{}\0{}\0{}\0",
                        record.timestamp,
                        record.session_id,
                        record.provider,
                        record.tokens.input,
                        record.tokens.output,
                        record.total_tokens,
                        record.static_status.unwrap_or(""),
                        record.live_status.unwrap_or(""),
                    )?;
                } else {
                    write!(
                        handle,
                        "{}\0{}\0{}\0{}\0{}\0{}\0{}\0",
                        record.timestamp,
                        record.session_id,
                        record.provider,
                        record.tokens.input,
                        record.tokens.output,
                        record.total_tokens,
                        record.static_status.unwrap_or(""),
                    )?;
                }
            }
        }
        OutputFormat::Human => {
            for record in records {
                if record.is_sub_agent {
                    continue;
                }
                let dt: chrono::DateTime<chrono::Local> =
                    chrono::DateTime::from_timestamp(record.timestamp as i64, 0)
                        .unwrap_or_default()
                        .with_timezone(&chrono::Local);
                if wants_live {
                    println!(
                        "{} {} {} {} {:>10} {:>10} {:>10}",
                        dt.format("%Y-%m-%dT%H:%M:%S"),
                        record.session_id,
                        record.static_status.unwrap_or("-"),
                        record.live_status.unwrap_or("-"),
                        format_tokens(record.tokens.input),
                        format_tokens(record.tokens.output),
                        format_tokens(record.total_tokens),
                    );
                } else {
                    println!(
                        "{} {} {} {:>10} {:>10} {:>10}",
                        dt.format("%Y-%m-%dT%H:%M:%S"),
                        record.session_id,
                        record.static_status.unwrap_or("-"),
                        format_tokens(record.tokens.input),
                        format_tokens(record.tokens.output),
                        format_tokens(record.total_tokens),
                    );
                }
            }
            let total_input: u64 = records.iter().map(|record| record.tokens.input).sum();
            let total_output: u64 = records.iter().map(|record| record.tokens.output).sum();
            let grand_total: u64 = records.iter().map(|record| record.total_tokens).sum();
            let user_count = records.iter().filter(|record| !record.is_sub_agent).count();
            let sub_agent_count = records.iter().filter(|record| record.is_sub_agent).count();
            if sub_agent_count > 0 {
                println!(
                    "Total: {} sessions ({} user + {} sub-agent), {} input, {} output, {} total tokens",
                    records.len(),
                    user_count,
                    sub_agent_count,
                    format_tokens(total_input),
                    format_tokens(total_output),
                    format_tokens(grand_total),
                );
            } else {
                println!(
                    "Total: {} sessions, {} input, {} output, {} total tokens",
                    records.len(),
                    format_tokens(total_input),
                    format_tokens(total_output),
                    format_tokens(grand_total),
                );
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
    fn cli_usage_live_status_csv() {
        let args = Args::try_parse_from([
            "ai-audit",
            "usage",
            "--filter-by-live-status",
            "running,idle",
        ])
        .unwrap();
        match args.command {
            Commands::Usage(a) => assert_eq!(a.status.live_status.unwrap().len(), 2),
            _ => panic!("expected usage"),
        }
    }

    #[test]
    fn cli_usage_resumable_flag() {
        let args = Args::try_parse_from(["ai-audit", "usage", "--resumable"]).unwrap();
        match args.command {
            Commands::Usage(a) => assert!(a.status.resumable),
            _ => panic!("expected usage"),
        }
    }

    #[test]
    fn format_tokens_billions() {
        assert_eq!(super::format_tokens(1_000_000_000), "1.0G");
    }
}

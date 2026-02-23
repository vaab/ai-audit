//! Token usage command handler.

use anyhow::Result;
use serde::Serialize;
use std::path::PathBuf;

use super::super::def::SessionType;
use crate::provider::{self, Provider, TokenUsage};
use crate::OutputFormat;

/// Per-session token usage record.
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
}

/// Parsed timespan bounds as UTC epoch seconds.
struct TimespanFilter {
    start: i64,
    end: i64,
}

impl TimespanFilter {
    fn overlaps(&self, started_secs: f64, updated_secs: f64) -> bool {
        let started = started_secs as i64;
        let updated = updated_secs as i64;
        started <= self.end && updated >= self.start
    }
}

pub fn run(
    session: Option<String>,
    session_type: Option<SessionType>,
    timespan: Option<&str>,
    project: Option<&str>,
    format: OutputFormat,
    quiet: bool,
) -> Result<()> {
    if let Some(ref session_id) = session {
        return run_single(session_id, format);
    }
    run_aggregated(session_type, timespan, project, format, quiet)
}

/// Show detailed token usage for a single session.
fn run_single(session_id: &str, format: OutputFormat) -> Result<()> {
    let p = provider::provider_for_session(session_id);
    let messages = p.list_messages(session_id)?;

    let total_tokens: TokenUsage = messages.iter().filter_map(|m| m.tokens.clone()).sum();

    let user_count = messages.iter().filter(|m| m.role == "user").count();
    let assistant_count = messages.iter().filter(|m| m.role == "assistant").count();

    match format {
        OutputFormat::Json => {
            let record = SessionUsage {
                timestamp: messages
                    .first()
                    .map(|m| {
                        m.timestamp.timestamp() as f64
                            + m.timestamp.timestamp_subsec_nanos() as f64 / 1_000_000_000.0
                    })
                    .unwrap_or(0.0),
                session_id: session_id.to_string(),
                provider: p.provider().as_str(),
                total_tokens: total_tokens.total(),
                tokens: total_tokens,
                message_count: messages.len(),
            };
            println!("{}", serde_json::to_string(&record)?);
        }
        OutputFormat::Nul => {
            use std::io::{self, Write};
            let stdout = io::stdout();
            let mut handle = stdout.lock();
            write!(
                handle,
                "{}\0{}\0{}\0{}\0{}\0{}\0",
                messages
                    .first()
                    .map(|m| m.timestamp.timestamp() as f64
                        + m.timestamp.timestamp_subsec_nanos() as f64 / 1_000_000_000.0)
                    .unwrap_or(0.0),
                session_id,
                p.provider().as_str(),
                total_tokens.input,
                total_tokens.output,
                total_tokens.total(),
            )?;
        }
        OutputFormat::Human => {
            println!("Session: {} ({})", session_id, p.provider());
            println!(
                "Messages: {} (user: {}, assistant: {})",
                messages.len(),
                user_count,
                assistant_count
            );
            println!();
            println!("Token Usage:");
            println!("  Input:          {:>12}", total_tokens.input);
            println!("  Output:         {:>12}", total_tokens.output);
            println!("  Cache read:     {:>12}", total_tokens.cache_read);
            println!("  Cache write:    {:>12}", total_tokens.cache_write);
            println!("  Cache creation: {:>12}", total_tokens.cache_creation);
            println!("  Reasoning:      {:>12}", total_tokens.reasoning);
            println!("  {}", "\u{2500}".repeat(17));
            println!("  Total:          {:>12}", total_tokens.total());
        }
    }

    Ok(())
}

/// Show aggregated token usage across sessions.
fn run_aggregated(
    session_type: Option<SessionType>,
    timespan: Option<&str>,
    project: Option<&str>,
    format: OutputFormat,
    quiet: bool,
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

    let project_path: Option<String> = match project {
        Some(p) => {
            let path = PathBuf::from(p);
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

    let providers: Vec<Box<dyn provider::SessionProvider>> = match session_type {
        Some(SessionType::ClaudeCode) => vec![provider::provider_for(Provider::ClaudeCode)],
        Some(SessionType::OpenCode) => vec![provider::provider_for(Provider::OpenCode)],
        None => provider::all_providers(),
    };

    let mut usage_records: Vec<SessionUsage> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    for p in &providers {
        match p.list_sessions() {
            Ok(sessions) => {
                for s in sessions {
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

                    match p.list_messages(&s.session_id) {
                        Ok(messages) => {
                            let tokens: TokenUsage =
                                messages.iter().filter_map(|m| m.tokens.clone()).sum();
                            if tokens.is_empty() {
                                continue;
                            }
                            usage_records.push(SessionUsage {
                                timestamp: started,
                                session_id: s.session_id,
                                provider: s.provider.as_str(),
                                total_tokens: tokens.total(),
                                tokens,
                                message_count: messages.len(),
                            });
                        }
                        Err(e) => {
                            if !quiet {
                                eprintln!(
                                    "Warning: failed to read messages for {}: {}",
                                    s.session_id, e
                                );
                            }
                        }
                    }
                }
            }
            Err(e) => {
                errors.push(format!("{}: {}", p.provider(), e));
            }
        }
    }

    // Sort by timestamp (oldest first)
    usage_records.sort_by(|a, b| a.timestamp.partial_cmp(&b.timestamp).unwrap());

    match format {
        OutputFormat::Json => {
            for r in &usage_records {
                println!("{}", serde_json::to_string(r)?);
            }
        }
        OutputFormat::Nul => {
            use std::io::{self, Write};
            let stdout = io::stdout();
            let mut handle = stdout.lock();
            for r in &usage_records {
                write!(
                    handle,
                    "{}\0{}\0{}\0{}\0{}\0{}\0",
                    r.timestamp,
                    r.session_id,
                    r.provider,
                    r.tokens.input,
                    r.tokens.output,
                    r.total_tokens,
                )?;
            }
        }
        OutputFormat::Human => {
            for r in &usage_records {
                let dt: chrono::DateTime<chrono::Local> =
                    chrono::DateTime::from_timestamp(r.timestamp as i64, 0)
                        .unwrap_or_default()
                        .with_timezone(&chrono::Local);
                println!(
                    "{} {} {} {:>10} {:>10} {:>10}",
                    dt.format("%Y-%m-%dT%H:%M:%S"),
                    r.session_id,
                    r.provider,
                    r.tokens.input,
                    r.tokens.output,
                    r.total_tokens,
                );
            }

            // Summary line
            let total_input: u64 = usage_records.iter().map(|r| r.tokens.input).sum();
            let total_output: u64 = usage_records.iter().map(|r| r.tokens.output).sum();
            let grand_total: u64 = usage_records.iter().map(|r| r.total_tokens).sum();
            println!(
                "Total: {} sessions, {} input, {} output, {} total tokens",
                usage_records.len(),
                total_input,
                total_output,
                grand_total,
            );
        }
    }

    if !errors.is_empty() && !quiet {
        for e in &errors {
            eprintln!("Warning: failed to list sessions from {}", e);
        }
    }

    if usage_records.is_empty() && !errors.is_empty() {
        anyhow::bail!("Failed to get usage data: {}", errors.join("; "));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use crate::cli::def::Args;

    #[test]
    fn cli_usage_no_args() {
        let args = Args::try_parse_from(["ai-audit", "usage"]).expect("bare usage works");
        match args.command {
            crate::cli::def::Commands::Usage { session, .. } => {
                assert!(session.is_none());
            }
            _ => panic!("expected Usage command"),
        }
    }

    #[test]
    fn cli_usage_session_id() {
        let args =
            Args::try_parse_from(["ai-audit", "usage", "ses_abc123"]).expect("session id works");
        match args.command {
            crate::cli::def::Commands::Usage { session, .. } => {
                assert_eq!(session.as_deref(), Some("ses_abc123"));
            }
            _ => panic!("expected Usage command"),
        }
    }

    #[test]
    fn cli_usage_type_claudecode() {
        let args = Args::try_parse_from(["ai-audit", "usage", "--type", "claudecode"])
            .expect("--type claudecode works");
        match args.command {
            crate::cli::def::Commands::Usage { session_type, .. } => {
                assert!(matches!(
                    session_type,
                    Some(crate::cli::def::SessionType::ClaudeCode)
                ));
            }
            _ => panic!("expected Usage command"),
        }
    }

    #[test]
    fn cli_usage_type_opencode() {
        let args = Args::try_parse_from(["ai-audit", "usage", "--type", "opencode"])
            .expect("--type opencode works");
        match args.command {
            crate::cli::def::Commands::Usage { session_type, .. } => {
                assert!(matches!(session_type, Some(crate::cli::def::SessionType::OpenCode)));
            }
            _ => panic!("expected Usage command"),
        }
    }

    #[test]
    fn cli_usage_timespan() {
        let args = Args::try_parse_from(["ai-audit", "usage", "--timespan", "today"])
            .expect("--timespan today works");
        match args.command {
            crate::cli::def::Commands::Usage { timespan, .. } => {
                assert_eq!(timespan.as_deref(), Some("today"));
            }
            _ => panic!("expected Usage command"),
        }
    }

    #[test]
    fn cli_usage_project_filter() {
        let args =
            Args::try_parse_from(["ai-audit", "usage", "-p", "/tmp"]).expect("-p /tmp works");
        match args.command {
            crate::cli::def::Commands::Usage { project, .. } => {
                assert_eq!(project.as_deref(), Some("/tmp"));
            }
            _ => panic!("expected Usage command"),
        }
    }

    #[test]
    fn cli_usage_json_flag() {
        let args =
            Args::try_parse_from(["ai-audit", "usage", "--json"]).expect("--json works");
        match args.command {
            crate::cli::def::Commands::Usage { output, .. } => {
                assert!(output.json);
                assert!(!output.nul);
            }
            _ => panic!("expected Usage command"),
        }
    }

    #[test]
    fn cli_usage_nul_flag() {
        let args = Args::try_parse_from(["ai-audit", "usage", "-0"]).expect("-0 works");
        match args.command {
            crate::cli::def::Commands::Usage { output, .. } => {
                assert!(output.nul);
                assert!(!output.json);
            }
            _ => panic!("expected Usage command"),
        }
    }

    #[test]
    fn cli_usage_json_and_nul_mutually_exclusive() {
        let result = Args::try_parse_from(["ai-audit", "usage", "--json", "-0"]);
        assert!(result.is_err(), "--json and -0 should be mutually exclusive");
    }
}

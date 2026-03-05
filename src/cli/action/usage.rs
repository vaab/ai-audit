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
    is_sub_agent: bool,
}

/// Format a token count for human display using SI prefixes (K/M/G).
///
/// Values under 1000 are shown as-is with trailing padding so that
/// bare numbers align with suffixed ones: `"8   "` aligns with
/// `"8.0K"` when right-justified in a column.
fn format_tokens(n: u64) -> String {
    if n >= 1_000_000_000 {
        format!("{:.1}G", n as f64 / 1_000_000_000.0)
    } else if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        format!("{}   ", n)
    }
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
                is_sub_agent: false,
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

/// Show aggregated token usage across sessions.
fn run_aggregated(
    session_type: Option<SessionType>,
    timespan: Option<&str>,
    project: Option<&str>,
    format: OutputFormat,
    _quiet: bool,
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
                            let is_sub_agent = s.parent_id.is_some();
                            usage_records.push(SessionUsage {
                                timestamp: started,
                                session_id: s.session_id,
                                provider: s.provider.as_str(),
                                total_tokens: tokens.total(),
                                tokens,
                                message_count: messages.len(),
                                is_sub_agent,
                            });
                        }
                        Err(e) => {
                            log::warn!("Failed to read messages for {}: {}", s.session_id, e);
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
                if r.is_sub_agent {
                    continue;
                }
                let dt: chrono::DateTime<chrono::Local> =
                    chrono::DateTime::from_timestamp(r.timestamp as i64, 0)
                        .unwrap_or_default()
                        .with_timezone(&chrono::Local);
                println!(
                    "{} {} {} {:>10} {:>10} {:>10}",
                    dt.format("%Y-%m-%dT%H:%M:%S"),
                    r.session_id,
                    r.provider,
                    format_tokens(r.tokens.input),
                    format_tokens(r.tokens.output),
                    format_tokens(r.total_tokens),
                );
            }

            // Summary line with sub-agent breakdown
            let total_input: u64 = usage_records.iter().map(|r| r.tokens.input).sum();
            let total_output: u64 = usage_records.iter().map(|r| r.tokens.output).sum();
            let grand_total: u64 = usage_records.iter().map(|r| r.total_tokens).sum();
            let user_count = usage_records.iter().filter(|r| !r.is_sub_agent).count();
            let sub_agent_count = usage_records.iter().filter(|r| r.is_sub_agent).count();
            if sub_agent_count > 0 {
                println!(
                    "Total: {} sessions ({} user + {} sub-agent), {} input, {} output, {} total tokens",
                    usage_records.len(),
                    user_count,
                    sub_agent_count,
                    format_tokens(total_input),
                    format_tokens(total_output),
                    format_tokens(grand_total),
                );
            } else {
                println!(
                    "Total: {} sessions, {} input, {} output, {} total tokens",
                    usage_records.len(),
                    format_tokens(total_input),
                    format_tokens(total_output),
                    format_tokens(grand_total),
                );
            }
        }
    }

    for e in &errors {
        log::warn!("Failed to list sessions from {}", e);
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
                assert!(matches!(
                    session_type,
                    Some(crate::cli::def::SessionType::OpenCode)
                ));
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
        let args = Args::try_parse_from(["ai-audit", "usage", "--json"]).expect("--json works");
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
        assert!(
            result.is_err(),
            "--json and -0 should be mutually exclusive"
        );
    }

    #[test]
    fn format_tokens_below_thousand() {
        assert_eq!(super::format_tokens(0), "0   ");
        assert_eq!(super::format_tokens(1), "1   ");
        assert_eq!(super::format_tokens(999), "999   ");
    }

    #[test]
    fn format_tokens_thousands() {
        assert_eq!(super::format_tokens(1_000), "1.0K");
        assert_eq!(super::format_tokens(1_500), "1.5K");
        assert_eq!(super::format_tokens(52_301), "52.3K");
        assert_eq!(super::format_tokens(999_999), "1000.0K");
    }

    #[test]
    fn format_tokens_millions() {
        assert_eq!(super::format_tokens(1_000_000), "1.0M");
        assert_eq!(super::format_tokens(5_622_054), "5.6M");
        assert_eq!(super::format_tokens(215_015_370), "215.0M");
    }

    #[test]
    fn format_tokens_billions() {
        assert_eq!(super::format_tokens(1_000_000_000), "1.0G");
        assert_eq!(super::format_tokens(1_400_000_000), "1.4G");
    }
}

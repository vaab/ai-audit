use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use std::fs;
use std::io::{self, Write};

use ai_audit::{activity, claudecode, config, opencode, OutputFormat};

#[cfg(unix)]
fn reset_sigpipe() {
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

#[derive(Parser)]
#[command(name = "ai-audit")]
#[command(version)]
#[command(about = "Audit tool for AI assistant sessions")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Clone, Copy, ValueEnum)]
enum SessionType {
    Claudecode,
    Opencode,
}

#[derive(Subcommand)]
enum Commands {
    /// List permission events for a session
    Permissions {
        /// Session ID (UUID)
        session: String,

        /// Output NUL-separated records for piping
        #[arg(short = '0', long = "null")]
        nul: bool,

        /// Output as newline-delimited JSON
        #[arg(short, long)]
        json: bool,
    },
    /// List available sessions
    ListSessions {
        /// Filter by session type
        #[arg(short = 't', long = "type")]
        session_type: Option<SessionType>,

        /// Output as newline-delimited JSON
        #[arg(short, long)]
        json: bool,
    },
    /// User activity tracking (messages and permission grants)
    Activity {
        #[command(subcommand)]
        action: ActivityAction,
    },
}

#[derive(Subcommand)]
enum ActivityAction {
    /// List available activity identifiers
    List,
    /// Get activity events for a timespan
    Get {
        /// Timespan to query (e.g., "today", "2025-01-01..2025-01-02")
        timespan: String,

        /// Identifier(s) to filter by (e.g., "claude-msg@rs/ai-audit")
        #[arg(name = "IDENT")]
        identifiers: Vec<String>,

        /// Output raw data separated by NUL char
        #[arg(short, long)]
        raw: bool,
    },
}

#[derive(Debug)]
struct UnifiedSession {
    timestamp: chrono::DateTime<chrono::Utc>,
    session_id: String,
    session_type: &'static str,
}

fn cmd_permissions(session: &str, format: OutputFormat) -> Result<()> {
    if session.starts_with("ses_") {
        let events = opencode::permissions::parse_events(session)?;
        opencode::permissions::display_events(&events, format);
    } else {
        let debug_file = claudecode::resolve_debug_file(session);

        if !debug_file.exists() {
            anyhow::bail!("Debug file not found: {}", debug_file.display());
        }

        let content = fs::read_to_string(&debug_file).context("Failed to read debug file")?;

        let mut events = claudecode::permissions::parse_events(&content)?;

        if let Ok(tool_uses) = claudecode::session::load_tool_uses(session) {
            claudecode::permissions::enrich_with_session(&mut events, &tool_uses);
        }

        claudecode::permissions::display_events(&events, format);
    }

    Ok(())
}

fn cmd_list_sessions(session_type: Option<SessionType>, json: bool) -> Result<()> {
    let mut sessions: Vec<UnifiedSession> = Vec::new();

    let include_claudecode = session_type.map_or(true, |t| matches!(t, SessionType::Claudecode));
    let include_opencode = session_type.map_or(true, |t| matches!(t, SessionType::Opencode));

    if include_claudecode {
        if let Ok(cc_sessions) = claudecode::session::list_sessions() {
            for s in cc_sessions {
                sessions.push(UnifiedSession {
                    timestamp: s.timestamp,
                    session_id: s.session_id,
                    session_type: "claudecode",
                });
            }
        }
    }

    if include_opencode {
        if let Ok(oc_sessions) = opencode::list_sessions() {
            for s in oc_sessions {
                sessions.push(UnifiedSession {
                    timestamp: s.timestamp,
                    session_id: s.session_id,
                    session_type: "opencode",
                });
            }
        }
    }

    sessions.sort_by_key(|s| s.timestamp);

    if json {
        for s in &sessions {
            println!(
                r#"{{"timestamp":"{}","session_id":"{}","type":"{}"}}"#,
                s.timestamp.to_rfc3339(),
                s.session_id,
                s.session_type
            );
        }
    } else {
        for s in &sessions {
            println!(
                "{}\t{}\t{}",
                s.timestamp.to_rfc3339(),
                s.session_id,
                s.session_type
            );
        }
    }

    Ok(())
}

fn main() -> Result<()> {
    #[cfg(unix)]
    reset_sigpipe();

    let cli = Cli::parse();

    match cli.command {
        Commands::Permissions { session, nul, json } => {
            let format = if json {
                OutputFormat::Json
            } else if nul {
                OutputFormat::Nul
            } else {
                OutputFormat::Human
            };
            cmd_permissions(&session, format)?;
        }
        Commands::ListSessions { session_type, json } => {
            cmd_list_sessions(session_type, json)?;
        }
        Commands::Activity { action } => {
            cmd_activity(action)?;
        }
    }

    Ok(())
}

fn cmd_activity(action: ActivityAction) -> Result<()> {
    let config = config::Config::load().context("Failed to load configuration")?;

    match action {
        ActivityAction::List => {
            let identifiers = activity::list_identifiers(&config)?;
            for ident in identifiers {
                println!("{}", ident);
            }
        }
        ActivityAction::Get {
            timespan,
            identifiers,
            raw,
        } => {
            let (start, end) = kal_time::parse_timespan(&timespan)
                .map_err(|e| anyhow::anyhow!("Failed to parse timespan '{}': {}", timespan, e))?;

            let events = activity::fetch_activities(&config, start, end, &identifiers)?;

            let stdout = io::stdout();
            let mut handle = stdout.lock();

            if raw {
                // Raw mode: NUL-separated records
                // Format: timestamp\0ident\0json_activity\0
                for event in events {
                    let json = serde_json::to_string(&event.data)?;
                    write!(handle, "{}\0{}\0{}\0", event.timestamp, event.ident, json)?;
                }
            } else {
                // Human readable mode
                for event in events {
                    let timestamp_str = activity::format_timestamp_display(event.timestamp);
                    let summary = activity::activity_summary(&event);
                    writeln!(handle, "{} {} {}", timestamp_str, event.ident, summary)?;
                }
            }
        }
    }

    Ok(())
}

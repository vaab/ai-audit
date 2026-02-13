//! CLI argument definitions.

use clap::{ArgGroup, Args as ClapArgs, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

use crate::OutputFormat;

/// Audit tool for AI assistant sessions
#[derive(Parser)]
#[command(name = "ai-audit", version, about)]
pub struct Args {
    /// Suppress non-error output
    #[arg(short, long, global = true)]
    pub quiet: bool,

    /// Increase verbosity (-v, -vv, -vvv)
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    pub verbose: u8,

    #[command(subcommand)]
    pub command: Commands,
}

/// Session type filter
#[derive(Clone, Copy, ValueEnum)]
pub enum SessionType {
    /// Claude Code sessions
    #[value(name = "claudecode")]
    ClaudeCode,
    /// OpenCode sessions
    #[value(name = "opencode")]
    OpenCode,
}

/// Output format options (mutually exclusive)
#[derive(ClapArgs)]
#[command(group = ArgGroup::new("output-format").multiple(false))]
pub struct OutputOpts {
    /// Output NUL-separated records for piping
    #[arg(short = '0', long = "null", group = "output-format")]
    pub nul: bool,

    /// Output as newline-delimited JSON
    #[arg(short, long, group = "output-format")]
    pub json: bool,
}

impl OutputOpts {
    /// Convert flags to OutputFormat enum
    pub fn format(&self) -> OutputFormat {
        if self.json {
            OutputFormat::Json
        } else if self.nul {
            OutputFormat::Nul
        } else {
            OutputFormat::Human
        }
    }
}

#[derive(Subcommand)]
pub enum Commands {
    /// List permission events for a session
    Permissions {
        /// Session ID (UUID or ses_* for OpenCode)
        session: String,

        #[command(flatten)]
        output: OutputOpts,
    },
    /// List available sessions
    ListSessions {
        /// Filter by session type
        #[arg(short = 't', long = "type")]
        session_type: Option<SessionType>,

        /// Only list sessions containing a message matching this string
        #[arg(short, long)]
        search: Option<String>,

        /// Filter by timespan (e.g., "today", "2025-01-01..2025-01-02")
        #[arg(long)]
        timespan: Option<String>,

        #[command(flatten)]
        output: OutputOpts,
    },
    /// User activity tracking (messages and permission grants)
    Activity {
        #[command(subcommand)]
        action: ActivityAction,
    },
    /// Rate agent instructions against test cases
    Rate {
        /// Path to agent instruction file (system prompt)
        instruction: PathBuf,

        /// Test file or directory containing test cases
        #[arg(long)]
        test: PathBuf,

        /// Agent models (comma-separated)
        #[arg(long)]
        agent_models: Option<String>,

        /// Judge models (comma-separated)
        #[arg(long)]
        judge_models: Option<String>,

        /// Agent name for opencode (uses opencode default if not specified)
        #[arg(long)]
        agent_name: Option<String>,

        /// Timeout in seconds
        #[arg(long)]
        timeout: Option<u64>,

        /// Force recomputation (ignore cache)
        #[arg(long)]
        no_cache: bool,

        /// Judge prompt path
        #[arg(long)]
        judge_prompt: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
pub enum ActivityAction {
    /// List available activity identifiers
    List {
        #[command(flatten)]
        output: OutputOpts,
    },
    /// Get activity events for a timespan
    Get {
        /// Timespan to query (e.g., "today", "2025-01-01..2025-01-02")
        timespan: String,

        /// Identifier(s) to filter by (e.g., "claude-msg@rs/ai-audit")
        #[arg(name = "IDENT")]
        identifiers: Vec<String>,

        #[command(flatten)]
        output: OutputOpts,
    },
}

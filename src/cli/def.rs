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
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum SessionType {
    /// Claude Code sessions
    #[value(name = "claudecode")]
    ClaudeCode,
    /// OpenCode sessions
    #[value(name = "opencode")]
    OpenCode,
    /// pi (badlogic/pi-mono) sessions
    #[value(name = "pi")]
    Pi,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum StaticStatusArg {
    #[value(name = "completed")]
    Completed,
    #[value(name = "user-pending")]
    UserPending,
    #[value(name = "assistant-empty")]
    AssistantEmpty,
    #[value(name = "assistant-partial")]
    AssistantPartial,
    #[value(name = "assistant-tool-stuck")]
    AssistantToolStuck,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum LiveStatusArg {
    #[value(name = "running")]
    Running,
    #[value(name = "idle")]
    Idle,
    #[value(name = "server-unreachable")]
    ServerUnreachable,
}

/// Output format options (mutually exclusive)
#[derive(ClapArgs)]
#[command(group = ArgGroup::new("output-format").multiple(false))]
pub struct OutputOpts {
    /// Output NUL-separated records for piping
    #[arg(short = '0', long = "null", alias = "raw", group = "output-format")]
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

#[derive(ClapArgs, Debug, Default, Clone)]
pub struct SessionStatusOpts {
    /// Filter by static status values (comma-separated)
    #[arg(
        long = "status",
        alias = "filter-by-static-status",
        value_enum,
        value_delimiter = ','
    )]
    pub status: Option<Vec<StaticStatusArg>>,

    /// Shorthand for the resumable static status set
    #[arg(long)]
    pub resumable: bool,

    /// Filter by last message timestamp timespan
    #[arg(long = "last-message-in")]
    pub last_message_in: Option<String>,

    /// Include live status in output
    #[arg(long = "output-live-status")]
    pub output_live_status: bool,

    /// Filter by live status values (comma-separated)
    #[arg(long = "filter-by-live-status", value_enum, value_delimiter = ',')]
    pub live_status: Option<Vec<LiveStatusArg>>,

    /// OpenCode server URL
    #[arg(long = "server-url")]
    pub server_url: Option<String>,

    /// OpenCode server password
    #[arg(long = "server-password", hide = true)]
    pub server_password: Option<String>,
}

#[derive(ClapArgs, Debug, Clone)]
#[command(group = ArgGroup::new("session-target").required(true).args(["session", "all"]))]
pub struct SessionNudgeArgs {
    /// Session ID to nudge
    pub session: Option<String>,

    /// Filter by project path
    #[arg(short, long)]
    pub project: Option<String>,

    /// Only nudge sessions containing a message matching this string
    #[arg(short, long)]
    pub search: Option<String>,

    /// Filter by last message timestamp timespan
    #[arg(long = "last-message-in")]
    pub last_message_in: Option<String>,

    /// Filter by static status values (comma-separated)
    #[arg(
        long = "status",
        alias = "filter-by-static-status",
        value_enum,
        value_delimiter = ','
    )]
    pub status: Option<Vec<StaticStatusArg>>,

    /// Nudge all sessions matching filters
    #[arg(long)]
    pub all: bool,

    /// Show the nudge plan without posting prompts
    #[arg(long)]
    pub dry_run: bool,

    /// OpenCode server URL
    #[arg(long = "server-url")]
    pub server_url: Option<String>,

    /// OpenCode server password
    #[arg(long = "server-password", hide = true)]
    pub server_password: Option<String>,

    /// Prompt to post as the nudge user message
    #[arg(long = "continue-prompt", default_value = "continue")]
    pub continue_prompt: String,

    /// Allow nudging sessions whose project directories no longer exist
    #[arg(long = "allow-revive-orphan-sessions")]
    pub allow_revive_orphan_sessions: bool,

    /// Also nudge sessions that are already running
    #[arg(long = "force-nudge-already-running")]
    pub force_nudge_already_running: bool,

    /// Maximum concurrent prompt_async requests
    #[arg(long, default_value_t = 10)]
    pub concurrency: usize,
}

#[derive(Subcommand)]
pub enum SessionAction {
    /// Nudge resumable OpenCode sessions
    Nudge(SessionNudgeArgs),
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

        /// Filter by session ID (exact match)
        #[arg(long)]
        session_id: Option<String>,

        /// Only list sessions containing a message matching this string
        #[arg(short, long)]
        search: Option<String>,

        /// Filter by timespan (e.g., "today", "2025-01-01..2025-01-02")
        #[arg(long)]
        timespan: Option<String>,

        /// Filter by project path (exact match; can be relative, e.g., "." or "../fyl")
        #[arg(short, long)]
        project: Option<String>,

        /// Only list sessions where this file was written or edited
        #[arg(short, long)]
        file: Option<String>,

        /// Include sub-agent sessions (hidden by default)
        #[arg(short, long)]
        all: bool,

        /// List only sub-sessions of this parent session ID
        #[arg(long)]
        children_of: Option<String>,

        #[command(flatten)]
        status: SessionStatusOpts,

        #[command(flatten)]
        output: OutputOpts,
    },
    /// User activity tracking (messages and permission grants)
    Activity {
        #[command(subcommand)]
        action: ActivityAction,
    },
    /// Display full session transcript
    Transcript {
        /// Session ID (UUID for Claude Code, ses_* for OpenCode).
        /// If omitted, auto-detects the current session.
        session: Option<String>,

        /// Show only the last N entries
        #[arg(short = 'n', long)]
        last: Option<usize>,

        /// Show only tool_use entries that wrote or edited this file
        #[arg(short, long)]
        file: Option<String>,

        #[command(flatten)]
        output: OutputOpts,
    },
    /// Detect and print the current AI session ID
    CurrentSession {
        /// Text to match against the last messages of session transcripts.
        /// When provided, sessions are identified by searching for this string
        /// in recent messages instead of using process-tree detection.
        #[arg(short, long)]
        r#match: Option<String>,

        /// Filter by session type (claudecode or opencode)
        #[arg(short = 't', long = "type")]
        session_type: Option<SessionType>,

        /// Number of recent messages to search when using --match (default: 5)
        #[arg(short = 'n', long, default_value = "5")]
        last_messages: usize,

        /// Filter by project path (default: current directory)
        #[arg(short, long)]
        project: Option<String>,

        #[command(flatten)]
        output: OutputOpts,
    },
    /// Detect the last AI session used in the current tmux pane
    LastSession {
        /// Filter by session type (claudecode or opencode)
        #[arg(short = 't', long = "type")]
        session_type: Option<SessionType>,

        /// Read scrollback from file instead of capturing from tmux pane
        #[arg(long)]
        scrollback_file: Option<PathBuf>,

        #[command(flatten)]
        output: OutputOpts,
    },
    /// Manage sessions
    Session {
        #[command(subcommand)]
        action: SessionAction,
    },
    /// Show token usage for a session or across all sessions
    Usage {
        /// Session ID. If omitted, shows aggregated usage across all sessions.
        session: Option<String>,

        /// Filter by session type (claudecode or opencode)
        #[arg(short = 't', long = "type")]
        session_type: Option<SessionType>,

        /// Filter by timespan (e.g., "today", "2025-01-01..2025-01-02")
        #[arg(long)]
        timespan: Option<String>,

        /// Filter by project path
        #[arg(short, long)]
        project: Option<String>,

        #[command(flatten)]
        status: SessionStatusOpts,

        #[command(flatten)]
        output: OutputOpts,
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

impl Commands {
    /// Extract the output format from any command variant.
    pub fn output_format(&self) -> OutputFormat {
        match self {
            Commands::Permissions { output, .. }
            | Commands::ListSessions { output, .. }
            | Commands::Transcript { output, .. }
            | Commands::CurrentSession { output, .. }
            | Commands::LastSession { output, .. }
            | Commands::Usage { output, .. } => output.format(),
            Commands::Session { .. } | Commands::Rate { .. } => OutputFormat::Human,
            Commands::Activity { action } => match action {
                ActivityAction::List { output, .. } | ActivityAction::Get { output, .. } => {
                    output.format()
                }
            },
        }
    }
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

        /// Filter by session ID(s) (can be repeated)
        #[arg(short, long = "session")]
        sessions: Vec<String>,

        #[command(flatten)]
        output: OutputOpts,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn session_nudge_requires_target() {
        assert!(Args::try_parse_from(["ai-audit", "session", "nudge"]).is_err());
    }

    #[test]
    fn session_nudge_rejects_both_target_modes() {
        assert!(Args::try_parse_from(["ai-audit", "session", "nudge", "ses_1", "--all"]).is_err());
    }

    #[test]
    fn session_nudge_accepts_all_and_project() {
        assert!(
            Args::try_parse_from(["ai-audit", "session", "nudge", "--all", "-p", "/tmp/foo"])
                .is_ok()
        );
    }

    #[test]
    fn session_nudge_accepts_force_running() {
        assert!(Args::try_parse_from([
            "ai-audit",
            "session",
            "nudge",
            "--all",
            "--force-nudge-already-running",
        ])
        .is_ok());
    }
}

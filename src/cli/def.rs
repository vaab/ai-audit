//! CLI argument definitions.

use clap::{ArgGroup, Args as ClapArgs, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

use crate::OutputFormat;

/// Audit tool for AI assistant sessions
#[derive(Parser)]
#[command(name = "ai-audit", version, about)]
#[command(infer_subcommands = true)]
pub struct Args {
    /// Suppress non-error output
    #[arg(short, long, global = true)]
    pub quiet: bool,

    /// Increase verbosity (-v, -vv, -vvv)
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    pub verbose: u8,

    /// Force color output (defaults to TTY auto-detection, honoring NO_COLOR).
    #[arg(long, global = true)]
    pub color: bool,

    /// Force no-color output (defaults to TTY auto-detection).
    #[arg(long, global = true)]
    pub no_color: bool,

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
#[derive(ClapArgs, Debug, Clone)]
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

    /// Only nudge sessions whose transcript contains this string.
    /// Repeatable — when given multiple times, every needle must be
    /// present (AND semantics).
    #[arg(short, long = "search")]
    pub search: Vec<String>,

    /// Filter by timespan (e.g., "today", "2025-01-01..2025-01-02")
    #[arg(long)]
    pub timespan: Option<String>,

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

    /// Fork the session and apply the resume strategy to the fork,
    /// leaving the original session untouched.
    ///
    /// The fork is created at the same revert cutoff that CleanResume
    /// would use (i.e. just before the user message being replayed),
    /// or at the head for ContinuePrompt shapes.  The new fork's
    /// session id is printed.
    ///
    /// Useful as an escape hatch when you want to experiment with a
    /// nudge without risking the real session.  Note: each fork
    /// duplicates the entire message history on disk.
    #[arg(long = "fork")]
    pub fork: bool,
}

/// Arguments for `session delete` — wipe one or more sessions across
/// every storage location ai-audit reads (transcripts, debug logs,
/// SQLite rows, legacy file-tree, session-index cache).
///
/// Filter shape mirrors `SessionListArgs` / `SessionNudgeArgs`: a
/// positional `<session-id>`, batch via `--all` + filter flags, or
/// `--ids-file` for piped IDs.  Exactly one of the three target
/// modes must be supplied.
///
/// Safety:
///   * `--dry-run` is the only safety mechanism (no `--yes`, no
///     confirmation prompt — the filter is the contract).
///   * Self-deletion (target ID matches `$OPENCODE_SESSION_ID` /
///     `$CLAUDE_SESSION_ID` / `$PI_SESSION_ID` or
///     `session-detect::detect_current_session()`) is rejected with
///     a clear error.  No override flag is provided in v1.
///   * Child sessions (`parent_id == target`) are rejected unless
///     `--cascade` is passed; the error lists the child IDs.
#[derive(ClapArgs, Debug, Clone)]
#[command(group = ArgGroup::new("delete-target").required(true).args(["session", "all", "ids_file"]))]
pub struct SessionDeleteArgs {
    /// Session ID to delete (positional).  Mutually exclusive with
    /// `--all`, `--ids-file`, and every filter flag.
    pub session: Option<String>,

    /// Filter by session type
    #[arg(short = 't', long = "type")]
    pub session_type: Option<SessionType>,

    /// Filter by session ID (exact match — useful with `--type`
    /// for unambiguous targeting from a script).
    #[arg(long = "session-id")]
    pub session_id: Option<String>,

    /// Only delete sessions whose transcript contains this string.
    /// Repeatable — every needle must be present (AND semantics).
    #[arg(short, long = "search")]
    pub search: Vec<String>,

    /// Filter by timespan (e.g., "today", "..2026-01-01")
    #[arg(long)]
    pub timespan: Option<String>,

    /// Filter by last message timestamp timespan
    #[arg(long = "last-message-in")]
    pub last_message_in: Option<String>,

    /// Filter by project path (canonicalized; supports `.`, relative
    /// paths)
    #[arg(short, long)]
    pub project: Option<String>,

    /// Only delete sessions where this file was written or edited
    #[arg(short, long)]
    pub file: Option<String>,

    /// Filter by static status values (comma-separated)
    #[arg(
        long = "status",
        alias = "filter-by-static-status",
        value_enum,
        value_delimiter = ','
    )]
    pub status: Option<Vec<StaticStatusArg>>,

    /// Read session IDs from a file (or `-` for stdin).  Format is
    /// auto-detected: if the first non-whitespace byte is `{`, the
    /// content is parsed as newline-delimited JSON (`session_id` or
    /// `id` field per line, matching `session list -j`).  Otherwise
    /// NUL-separated plain IDs are expected (matching
    /// `session list -0` and `activity get --categs-file`).
    #[arg(long = "ids-file", value_name = "PATH")]
    pub ids_file: Option<PathBuf>,

    /// Delete all sessions matching the filter flags.  Required when
    /// using filters without a positional `<session-id>` or
    /// `--ids-file` (safety: prevents accidental "delete everything"
    /// from an empty-filter typo).
    #[arg(long)]
    pub all: bool,

    /// Also delete child sessions whose `parent_id` matches a target.
    /// Without this flag, the command refuses to delete a parent
    /// session and lists the child IDs in the error message.
    #[arg(long)]
    pub cascade: bool,

    /// Print what would be deleted (count + per-session path
    /// summary) without performing any writes.  Always exits 0.
    #[arg(long)]
    pub dry_run: bool,

    #[command(flatten)]
    pub output: OutputOpts,
}

#[derive(ClapArgs, Debug, Clone)]
pub struct SessionInfoArgs {
    /// Session ID. If omitted, auto-detects the current session.
    pub session: Option<String>,

    /// Skip the live-status HTTP probe (OpenCode only).
    /// Useful in offline contexts (e.g. commit hooks).
    #[arg(long = "no-live")]
    pub no_live: bool,

    /// OpenCode server URL (only consulted when probing live status).
    #[arg(long = "server-url")]
    pub server_url: Option<String>,

    /// OpenCode server password (only consulted when probing live status).
    #[arg(long = "server-password", hide = true)]
    pub server_password: Option<String>,

    #[command(flatten)]
    pub output: OutputOpts,
}

// ---------------------------------------------------------------------------
// Payload structs reused by both new `session <verb>` subcommands and the
// hidden top-level legacy commands.  This keeps the two surfaces in lockstep
// without duplicating field definitions.
// ---------------------------------------------------------------------------

#[derive(ClapArgs, Debug, Clone)]
pub struct SessionPermissionsArgs {
    /// Session ID (UUID or ses_* for OpenCode)
    pub session: String,

    #[command(flatten)]
    pub output: OutputOpts,
}

#[derive(ClapArgs, Debug, Clone)]
pub struct SessionListArgs {
    /// Filter by session type
    #[arg(short = 't', long = "type")]
    pub session_type: Option<SessionType>,

    /// Filter by session ID (exact match)
    #[arg(long)]
    pub session_id: Option<String>,

    /// Only list sessions whose transcript contains this string.
    /// Repeatable — when given multiple times, every needle must be
    /// present (AND semantics).
    #[arg(short, long = "search")]
    pub search: Vec<String>,

    /// Filter by timespan (e.g., "today", "2025-01-01..2025-01-02")
    #[arg(long)]
    pub timespan: Option<String>,

    /// Filter by project path (exact match; can be relative, e.g., "." or "../fyl")
    #[arg(short, long)]
    pub project: Option<String>,

    /// Only list sessions where this file was written or edited
    #[arg(short, long)]
    pub file: Option<String>,

    /// Include sub-agent sessions (hidden by default)
    #[arg(short, long)]
    pub all: bool,

    /// List only sub-sessions of this parent session ID
    #[arg(long)]
    pub children_of: Option<String>,

    #[command(flatten)]
    pub status: SessionStatusOpts,

    #[command(flatten)]
    pub output: OutputOpts,
}

#[derive(ClapArgs, Debug, Clone)]
pub struct SessionTranscriptArgs {
    /// Session ID (UUID for Claude Code, ses_* for OpenCode).
    /// If omitted, auto-detects the current session.
    pub session: Option<String>,

    /// Show only the last N entries
    #[arg(short = 'n', long)]
    pub last: Option<usize>,

    /// Show only tool_use entries that wrote or edited this file
    #[arg(short, long)]
    pub file: Option<String>,

    #[command(flatten)]
    pub output: OutputOpts,
}

#[derive(ClapArgs, Debug, Clone)]
pub struct SessionCurrentArgs {
    /// Text to match against the last messages of session transcripts.
    /// When provided, sessions are identified by searching for this string
    /// in recent messages instead of using process-tree detection.
    #[arg(short, long)]
    pub r#match: Option<String>,

    /// Filter by session type (claudecode or opencode)
    #[arg(short = 't', long = "type")]
    pub session_type: Option<SessionType>,

    /// Number of recent messages to search when using --match (default: 5)
    #[arg(short = 'n', long, default_value = "5")]
    pub last_messages: usize,

    /// Filter by project path (default: current directory)
    #[arg(short, long)]
    pub project: Option<String>,

    #[command(flatten)]
    pub output: OutputOpts,
}

#[derive(ClapArgs, Debug, Clone)]
pub struct SessionPreviousArgs {
    /// Filter by session type (claudecode or opencode)
    #[arg(short = 't', long = "type")]
    pub session_type: Option<SessionType>,

    /// Read scrollback from file instead of capturing from tmux pane
    #[arg(long)]
    pub scrollback_file: Option<PathBuf>,

    #[command(flatten)]
    pub output: OutputOpts,
}

#[derive(ClapArgs, Debug, Clone)]
pub struct SessionUsageArgs {
    /// Session ID. If omitted, shows aggregated usage across all sessions.
    pub session: Option<String>,

    /// Filter by session type (claudecode or opencode)
    #[arg(short = 't', long = "type")]
    pub session_type: Option<SessionType>,

    /// Filter by timespan (e.g., "today", "2025-01-01..2025-01-02")
    #[arg(long)]
    pub timespan: Option<String>,

    /// Filter by project path
    #[arg(short, long)]
    pub project: Option<String>,

    #[command(flatten)]
    pub status: SessionStatusOpts,

    #[command(flatten)]
    pub output: OutputOpts,
}

#[derive(ClapArgs, Debug, Clone)]
pub struct SessionAssistedByArgs {
    /// Session ID to resolve.  If omitted, auto-detects.
    #[arg(long)]
    pub session: Option<String>,

    /// Exit 0 silently when no current session can be detected.
    /// Useful for `commit-msg` hooks running in human shells where
    /// missing AI context should not block the commit.
    #[arg(long = "quiet-if-no-session")]
    pub quiet_if_no_session: bool,

    #[command(flatten)]
    pub output: OutputOpts,
}

#[derive(Subcommand)]
#[command(infer_subcommands = true)]
pub enum SessionAction {
    /// List available sessions
    #[command(visible_alias = "ls")]
    List(SessionListArgs),

    /// Detect and print the current AI session ID
    #[command(visible_alias = "cur")]
    Current(SessionCurrentArgs),

    /// Detect the last AI session used in the current tmux pane
    #[command(visible_alias = "prev")]
    Previous(SessionPreviousArgs),

    /// Display full session transcript
    #[command(visible_alias = "tr")]
    Transcript(SessionTranscriptArgs),

    /// List permission events for a session
    #[command(visible_alias = "perms")]
    Permissions(SessionPermissionsArgs),

    /// Show token usage for a session or across all sessions
    #[command(visible_alias = "tokens")]
    Usage(SessionUsageArgs),

    /// Resolve the kernel-canonical Assisted-by trailer for a session
    AssistedBy(SessionAssistedByArgs),

    /// Show metadata for a single session
    Info(SessionInfoArgs),

    /// Nudge resumable OpenCode sessions
    Nudge(SessionNudgeArgs),

    /// Delete sessions (wipes transcripts, DB rows, debug logs, and cache).
    ///
    /// Composes with the same filter flags as `session list`; pipe
    /// `session list -j` into `session delete --ids-file -` for
    /// pipeline composition.  `--dry-run` is the only safety
    /// mechanism — no confirmation prompt, no `--yes`.
    Delete(SessionDeleteArgs),
}

#[derive(Subcommand)]
pub enum Commands {
    /// List permission events for a session
    #[command(hide = true)]
    Permissions(SessionPermissionsArgs),
    /// List available sessions
    #[command(hide = true)]
    ListSessions(SessionListArgs),
    /// User activity tracking (messages and permission grants)
    Activity {
        #[command(subcommand)]
        action: ActivityAction,
    },
    /// Display full session transcript
    #[command(hide = true)]
    Transcript(SessionTranscriptArgs),
    /// Detect and print the current AI session ID
    #[command(hide = true)]
    CurrentSession(SessionCurrentArgs),
    /// Detect the last AI session used in the current tmux pane
    #[command(hide = true)]
    LastSession(SessionPreviousArgs),
    /// Manage sessions
    Session {
        #[command(subcommand)]
        action: SessionAction,
    },
    /// Show token usage for a session or across all sessions
    #[command(hide = true)]
    Usage(SessionUsageArgs),
    /// Resolve the kernel-canonical Assisted-by trailer for a session
    ///
    /// Defaults to the auto-detected current session (same detection
    /// path as `session current`).  Use `--session` to bypass detection.
    #[command(hide = true)]
    AssistedBy(SessionAssistedByArgs),
    /// Per-message LLM token consumption events over a timespan
    ///
    /// Emits one record per assistant message that consumed tokens,
    /// across all providers (claudecode, opencode, pi).  Filterable
    /// by session, project (basename of `.git` ancestor), provider
    /// harness, LLM provider id, and model substring.
    TokenUsage {
        /// Timespan to query (e.g., "today", "30m", "2025-01-01..2025-01-02")
        timespan: String,

        /// Filter by session ID(s) (can be repeated)
        #[arg(short, long = "session")]
        sessions: Vec<String>,

        /// Filter by project name (basename of `.git` ancestor; can be repeated).
        /// Use `--project=""` to match sessions outside any git repo.
        #[arg(short = 'p', long = "project")]
        projects: Vec<String>,

        /// Filter by harness type (claudecode, opencode, pi)
        #[arg(short = 't', long = "type")]
        session_type: Option<SessionType>,

        /// Filter by LLM provider id (e.g. "anthropic", "openai-codex"; can be repeated)
        #[arg(long = "provider-id")]
        provider_ids: Vec<String>,

        /// Filter by model substring (case-insensitive; can be repeated)
        #[arg(long = "model")]
        models: Vec<String>,

        /// Comma-separated list of fields to display (also repeatable).
        /// Available: timestamp, session_id, provider, provider_id, model,
        /// cwd, project_path, project, subpath,
        /// input, output, cache_read, cache_write, cache_creation, reasoning, total
        #[arg(long = "fields", short = 'f', value_delimiter = ',', action = clap::ArgAction::Append)]
        fields: Option<Vec<String>>,

        /// Print a header row (human mode only)
        #[arg(long)]
        header: bool,

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
            Commands::Permissions(a) => a.output.format(),
            Commands::ListSessions(a) => a.output.format(),
            Commands::Transcript(a) => a.output.format(),
            Commands::CurrentSession(a) => a.output.format(),
            Commands::LastSession(a) => a.output.format(),
            Commands::Usage(a) => a.output.format(),
            Commands::AssistedBy(a) => a.output.format(),
            Commands::TokenUsage { output, .. } => output.format(),
            Commands::Session { action } => action.output_format(),
            Commands::Rate { .. } => OutputFormat::Human,
            Commands::Activity { action } => match action {
                ActivityAction::List { output, .. } | ActivityAction::Get { output, .. } => {
                    output.format()
                }
            },
        }
    }
}

impl SessionAction {
    /// Extract the output format from any session subcommand variant.
    pub fn output_format(&self) -> OutputFormat {
        match self {
            SessionAction::List(a) => a.output.format(),
            SessionAction::Current(a) => a.output.format(),
            SessionAction::Previous(a) => a.output.format(),
            SessionAction::Transcript(a) => a.output.format(),
            SessionAction::Permissions(a) => a.output.format(),
            SessionAction::Usage(a) => a.output.format(),
            SessionAction::AssistedBy(a) => a.output.format(),
            SessionAction::Info(a) => a.output.format(),
            SessionAction::Nudge(_) => OutputFormat::Human,
            SessionAction::Delete(a) => a.output.format(),
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

        /// Read additional identifiers from a NUL-separated file (or `-` for stdin).
        ///
        /// Merged with positional IDENT arguments. Use this when the
        /// identifier list would exceed the command-line ARG_MAX limit.
        #[arg(long = "categs-file", value_name = "PATH")]
        categs_file: Option<PathBuf>,

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

    #[test]
    fn activity_get_accepts_categs_file() {
        let args = Args::try_parse_from([
            "ai-audit",
            "activity",
            "get",
            "today",
            "--categs-file",
            "/tmp/categs.nul",
        ])
        .expect("parse");
        match args.command {
            Commands::Activity {
                action:
                    ActivityAction::Get {
                        categs_file,
                        identifiers,
                        ..
                    },
            } => {
                assert_eq!(
                    categs_file.as_deref(),
                    Some(std::path::Path::new("/tmp/categs.nul"))
                );
                assert!(identifiers.is_empty());
            }
            _ => panic!("expected activity get"),
        }
    }

    #[test]
    fn activity_get_accepts_stdin_dash() {
        let args =
            Args::try_parse_from(["ai-audit", "activity", "get", "today", "--categs-file", "-"])
                .expect("parse");
        match args.command {
            Commands::Activity {
                action: ActivityAction::Get { categs_file, .. },
            } => {
                assert_eq!(categs_file.as_deref(), Some(std::path::Path::new("-")));
            }
            _ => panic!("expected activity get"),
        }
    }

    #[test]
    fn color_flag_parses_globally_before_subcommand() {
        let args = Args::try_parse_from(["ai-audit", "--color", "list-sessions"]).expect("parse");
        assert!(args.color);
        assert!(!args.no_color);
    }

    #[test]
    fn color_flag_parses_globally_after_subcommand() {
        // `global = true` makes the flag available after the subcommand too.
        let args =
            Args::try_parse_from(["ai-audit", "list-sessions", "--no-color"]).expect("parse");
        assert!(!args.color);
        assert!(args.no_color);
    }

    #[test]
    fn color_and_no_color_both_present_parse_ok() {
        // Clap accepts both flags together; the runtime conflict check
        // in `cli::run()` is what rejects the combination.
        let args = Args::try_parse_from(["ai-audit", "--color", "--no-color", "list-sessions"])
            .expect("parse");
        assert!(args.color);
        assert!(args.no_color);
    }

    #[test]
    fn activity_get_merges_positional_and_categs_file() {
        // Both forms can coexist. The action-side merges them.
        let args = Args::try_parse_from([
            "ai-audit",
            "activity",
            "get",
            "today",
            "claude-msg@p",
            "--categs-file",
            "/tmp/x",
        ])
        .expect("parse");
        match args.command {
            Commands::Activity {
                action:
                    ActivityAction::Get {
                        identifiers,
                        categs_file,
                        ..
                    },
            } => {
                assert_eq!(identifiers, vec!["claude-msg@p".to_string()]);
                assert!(categs_file.is_some());
            }
            _ => panic!("expected activity get"),
        }
    }

    // ====================================================================
    // `session <verb>` restructure (formerly top-level commands)
    // ====================================================================
    //
    // The CLI moved the read-only inspectors + `usage` + `assisted-by`
    // under `session`. These tests pin:
    //
    //   1. The new canonical form parses and routes to `Commands::Session`
    //      with the right `SessionAction` variant.
    //   2. Each subcommand's visible alias works (`ls`, `cur`, `prev`,
    //      `tr`, `perms`, `tokens`).
    //   3. Clap's `infer_subcommands` resolves unambiguous prefixes
    //      (e.g. `aa session li` -> `list`).
    //   4. The ambiguous-prefix case (`s p` between `permissions` and
    //      `previous`) is correctly rejected.
    //   5. Every legacy top-level form (`list-sessions`, `current-session`,
    //      `last-session`, `transcript`, `permissions <SESSION>`, `usage`,
    //      `assisted-by`) still parses to its hidden `Commands::*` variant
    //      — backwards compat for scripts and historical session
    //      transcripts.
    //   6. The top-level `s` prefix resolves to `session` (no ambiguity
    //      since no other top-level command starts with `s`).

    fn parse(argv: &[&str]) -> Args {
        Args::try_parse_from(argv).expect("parse should succeed")
    }

    #[test]
    fn session_list_canonical() {
        let args = parse(&["ai-audit", "session", "list"]);
        match args.command {
            Commands::Session {
                action: SessionAction::List(_),
            } => {}
            _ => panic!("expected session list"),
        }
    }

    #[test]
    fn session_list_alias_ls() {
        let args = parse(&["ai-audit", "session", "ls"]);
        assert!(matches!(
            args.command,
            Commands::Session {
                action: SessionAction::List(_)
            }
        ));
    }

    #[test]
    fn session_list_inferred_from_prefix() {
        // `li` is unambiguous because no other subcommand starts with `li`.
        let args = parse(&["ai-audit", "session", "li"]);
        assert!(matches!(
            args.command,
            Commands::Session {
                action: SessionAction::List(_)
            }
        ));
    }

    #[test]
    fn session_l_resolves_to_list() {
        // `l` is unambiguous: no other `session` subcommand starts with `l`
        // (`previous` replaced `last`, so the ambiguity is gone).
        let args = parse(&["ai-audit", "session", "l"]);
        assert!(matches!(
            args.command,
            Commands::Session {
                action: SessionAction::List(_)
            }
        ));
    }

    #[test]
    fn session_current_canonical_and_alias() {
        for argv in [
            &["ai-audit", "session", "current"][..],
            &["ai-audit", "session", "cur"][..],
        ] {
            let args = parse(argv);
            assert!(
                matches!(
                    args.command,
                    Commands::Session {
                        action: SessionAction::Current(_)
                    }
                ),
                "failed for {argv:?}"
            );
        }
    }

    #[test]
    fn session_previous_canonical_and_alias() {
        for argv in [
            &["ai-audit", "session", "previous"][..],
            &["ai-audit", "session", "prev"][..],
        ] {
            let args = parse(argv);
            assert!(
                matches!(
                    args.command,
                    Commands::Session {
                        action: SessionAction::Previous(_)
                    }
                ),
                "failed for {argv:?}"
            );
        }
    }

    #[test]
    fn session_transcript_canonical_and_alias() {
        for argv in [
            &["ai-audit", "session", "transcript", "ses_x"][..],
            &["ai-audit", "session", "tr", "ses_x"][..],
        ] {
            let args = parse(argv);
            assert!(
                matches!(
                    args.command,
                    Commands::Session {
                        action: SessionAction::Transcript(_)
                    }
                ),
                "failed for {argv:?}"
            );
        }
    }

    #[test]
    fn session_permissions_canonical_and_alias() {
        for argv in [
            &["ai-audit", "session", "permissions", "ses_x"][..],
            &["ai-audit", "session", "perms", "ses_x"][..],
        ] {
            let args = parse(argv);
            assert!(
                matches!(
                    args.command,
                    Commands::Session {
                        action: SessionAction::Permissions(_)
                    }
                ),
                "failed for {argv:?}"
            );
        }
    }

    #[test]
    fn session_usage_canonical_and_alias() {
        for argv in [
            &["ai-audit", "session", "usage"][..],
            &["ai-audit", "session", "tokens"][..],
        ] {
            let args = parse(argv);
            assert!(
                matches!(
                    args.command,
                    Commands::Session {
                        action: SessionAction::Usage(_)
                    }
                ),
                "failed for {argv:?}"
            );
        }
    }

    #[test]
    fn session_assisted_by_canonical() {
        let args = parse(&["ai-audit", "session", "assisted-by"]);
        assert!(matches!(
            args.command,
            Commands::Session {
                action: SessionAction::AssistedBy(_)
            }
        ));
    }

    #[test]
    fn session_p_is_ambiguous_between_permissions_and_previous() {
        // `p` matches `permissions`, `previous`, `prev`, and `perms`.
        // Clap's `infer_subcommands` refuses to silently pick a winner
        // and surfaces an error.  The exact phrasing depends on clap's
        // version (currently "unrecognized subcommand 'p'" with a
        // similar-commands tip listing several candidates).  The
        // contract we pin here is the bare minimum: parsing fails and
        // the error mentions at least one of the candidates so the
        // user can disambiguate.
        let result = Args::try_parse_from(["ai-audit", "session", "p"]);
        let err = match result {
            Ok(_) => panic!("expected ambiguous-prefix parse error, got Ok"),
            Err(e) => e,
        };
        let msg = err.to_string();
        assert!(
            msg.contains("previous") || msg.contains("permissions"),
            "expected error to mention at least one candidate, got: {msg}"
        );
    }

    #[test]
    fn session_pe_resolves_to_permissions() {
        let args = parse(&["ai-audit", "session", "pe", "ses_x"]);
        assert!(matches!(
            args.command,
            Commands::Session {
                action: SessionAction::Permissions(_)
            }
        ));
    }

    #[test]
    fn session_pr_resolves_to_previous() {
        let args = parse(&["ai-audit", "session", "pr"]);
        assert!(matches!(
            args.command,
            Commands::Session {
                action: SessionAction::Previous(_)
            }
        ));
    }

    #[test]
    fn top_level_s_resolves_to_session() {
        // No other top-level command starts with `s`, so `s l` works.
        let args = parse(&["ai-audit", "s", "l"]);
        assert!(matches!(
            args.command,
            Commands::Session {
                action: SessionAction::List(_)
            }
        ));
    }

    // ====================================================================
    // Legacy top-level commands (hidden, deprecated) still parse.
    // ====================================================================

    #[test]
    fn legacy_list_sessions_still_parses() {
        let args = parse(&["ai-audit", "list-sessions"]);
        assert!(matches!(args.command, Commands::ListSessions(_)));
    }

    #[test]
    fn legacy_current_session_still_parses() {
        let args = parse(&["ai-audit", "current-session"]);
        assert!(matches!(args.command, Commands::CurrentSession(_)));
    }

    #[test]
    fn legacy_last_session_still_parses() {
        let args = parse(&["ai-audit", "last-session"]);
        assert!(matches!(args.command, Commands::LastSession(_)));
    }

    #[test]
    fn legacy_transcript_still_parses() {
        let args = parse(&["ai-audit", "transcript", "ses_x"]);
        assert!(matches!(args.command, Commands::Transcript(_)));
    }

    #[test]
    fn legacy_permissions_still_parses() {
        let args = parse(&["ai-audit", "permissions", "ses_x"]);
        assert!(matches!(args.command, Commands::Permissions(_)));
    }

    #[test]
    fn legacy_usage_still_parses() {
        let args = parse(&["ai-audit", "usage"]);
        assert!(matches!(args.command, Commands::Usage(_)));
    }

    #[test]
    fn legacy_assisted_by_still_parses() {
        let args = parse(&["ai-audit", "assisted-by"]);
        assert!(matches!(args.command, Commands::AssistedBy(_)));
    }

    #[test]
    fn session_list_carries_filter_flags() {
        // Make sure flags flow through the payload struct correctly.
        let args = parse(&[
            "ai-audit", "session", "list", "--search", "needle", "-p", "/tmp", "-t", "opencode",
        ]);
        match args.command {
            Commands::Session {
                action: SessionAction::List(a),
            } => {
                assert_eq!(a.search, vec!["needle".to_string()]);
                assert_eq!(a.project.as_deref(), Some("/tmp"));
                assert_eq!(a.session_type, Some(SessionType::OpenCode));
            }
            _ => panic!("expected session list"),
        }
    }

    #[test]
    fn session_list_accepts_repeated_search() {
        // Multiple `-s`/`--search` flags collect into the Vec and
        // compose with AND semantics downstream (see
        // session_filter::combined_filters_intersect_multi_search).
        let args = parse(&[
            "ai-audit",
            "session",
            "list",
            "-s",
            "jwt",
            "--search",
            "middleware",
        ]);
        match args.command {
            Commands::Session {
                action: SessionAction::List(a),
            } => {
                assert_eq!(a.search, vec!["jwt".to_string(), "middleware".to_string()]);
            }
            _ => panic!("expected session list"),
        }
    }

    #[test]
    fn session_nudge_accepts_repeated_search() {
        let args = parse(&[
            "ai-audit", "session", "nudge", "--all", "-s", "stuck", "-s", "auth",
        ]);
        match args.command {
            Commands::Session {
                action: SessionAction::Nudge(a),
            } => {
                assert_eq!(a.search, vec!["stuck".to_string(), "auth".to_string()]);
            }
            _ => panic!("expected session nudge"),
        }
    }

    #[test]
    fn session_transcript_carries_session_id() {
        let args = parse(&["ai-audit", "session", "tr", "ses_abc", "-n", "5"]);
        match args.command {
            Commands::Session {
                action: SessionAction::Transcript(a),
            } => {
                assert_eq!(a.session.as_deref(), Some("ses_abc"));
                assert_eq!(a.last, Some(5));
            }
            _ => panic!("expected session transcript"),
        }
    }
}

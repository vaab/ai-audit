//! Command action handlers.

mod activity;
mod assisted_by;
mod last_session;
mod list_sessions;
mod permissions;
mod rate;
mod session;
mod token_usage;
mod transcript;
mod usage;

use anyhow::{anyhow, Result};

use super::def::{
    Commands, SessionAction, SessionAssistedByArgs, SessionCurrentArgs, SessionListArgs,
    SessionPermissionsArgs, SessionPreviousArgs, SessionTranscriptArgs, SessionType,
    SessionUsageArgs,
};

/// Resolve a session ID: use explicit value if given, else auto-detect.
fn resolve_session(explicit: Option<String>) -> Result<String> {
    match explicit {
        Some(id) => Ok(id),
        None => {
            let detected = crate::session_detect::detect_current_session()?;
            log::info!(
                "Auto-detected session: {} ({:?})",
                detected.session_id,
                detected.provider
            );
            Ok(detected.session_id)
        }
    }
}

pub(super) fn require_opencode_for(feature: &str) -> Result<()> {
    Err(anyhow!(
        "not implemented for claudecode (only opencode is supported for {} features)",
        feature
    ))
}

/// Emit a one-line deprecation notice on stderr when the user invokes a
/// legacy top-level command that has been folded under `session`.
///
/// The new and old commands route through the same payload struct and
/// the same `run_*` helper below, so the only difference between the
/// two paths is this warning.
fn deprecation_warning(old: &str, new: &str) {
    eprintln!(
        "warning: `ai-audit {old}` is deprecated; use `ai-audit {new}` instead.  \
         The old form still works but will be removed in a future release."
    );
}

// ---------------------------------------------------------------------------
// Per-action runners.  Both new (`session <verb>`) and legacy (top-level)
// command paths funnel into these to guarantee identical behaviour.
// ---------------------------------------------------------------------------

fn run_list(a: SessionListArgs, quiet: bool) -> Result<()> {
    list_sessions::run(
        a.session_type,
        a.session_id.as_deref(),
        &a.search,
        a.timespan.as_deref(),
        a.project.as_deref(),
        a.file.as_deref(),
        a.all,
        a.children_of.as_deref(),
        &a.status,
        a.output.format(),
        quiet,
    )
}

fn run_transcript(a: SessionTranscriptArgs, verbose: u8) -> Result<()> {
    let session_id = resolve_session(a.session)?;
    transcript::run(
        &session_id,
        a.last,
        a.file.as_deref(),
        a.output.format(),
        verbose,
    )
}

fn run_current(a: SessionCurrentArgs) -> Result<()> {
    let provider_filter = a.session_type.map(provider_from_arg);
    let detected = if let Some(needle) = a.r#match {
        crate::session_detect::find_session_by_match(&crate::session_detect::MatchOptions {
            needle,
            last_messages: a.last_messages,
            provider_filter,
            project_dir: a.project,
        })?
    } else {
        crate::session_detect::detect_current_session()?
    };
    match a.output.format() {
        crate::OutputFormat::Json => {
            println!(
                "{}",
                serde_json::json!({
                    "session_id": detected.session_id,
                    "provider": detected.provider.as_str(),
                })
            );
        }
        crate::OutputFormat::Nul => {
            print!("{}\0", detected.session_id);
        }
        crate::OutputFormat::Human => {
            println!("{}", detected.session_id);
        }
    }
    Ok(())
}

fn run_previous(a: SessionPreviousArgs) -> Result<()> {
    last_session::run(a.session_type, a.scrollback_file, a.output.format())
}

fn run_permissions(a: SessionPermissionsArgs) -> Result<()> {
    permissions::run(&a.session, a.output.format())
}

fn run_usage(a: SessionUsageArgs, quiet: bool) -> Result<()> {
    usage::run(
        a.session,
        a.session_type,
        a.timespan.as_deref(),
        a.project.as_deref(),
        &a.status,
        a.output.format(),
        quiet,
    )
}

fn run_assisted_by(a: SessionAssistedByArgs) -> Result<()> {
    assisted_by::run(a.session, a.quiet_if_no_session, a.output.format())
}

fn provider_from_arg(t: SessionType) -> crate::provider::Provider {
    match t {
        SessionType::OpenCode => crate::provider::Provider::OpenCode,
        SessionType::ClaudeCode => crate::provider::Provider::ClaudeCode,
        SessionType::Pi => crate::provider::Provider::Pi,
    }
}

pub fn dispatch(cmd: Commands, quiet: bool, verbose: u8) -> Result<()> {
    match cmd {
        // ---- New canonical `session <verb>` surface --------------------
        Commands::Session { action } => match action {
            SessionAction::List(a) => run_list(a, quiet),
            SessionAction::Current(a) => run_current(a),
            SessionAction::Previous(a) => run_previous(a),
            SessionAction::Transcript(a) => run_transcript(a, verbose),
            SessionAction::Permissions(a) => run_permissions(a),
            SessionAction::Usage(a) => run_usage(a, quiet),
            SessionAction::AssistedBy(a) => run_assisted_by(a),
            SessionAction::Info(args) => session::run_info(args),
            SessionAction::Nudge(args) => session::run_nudge(args),
            SessionAction::Delete(args) => session::run_delete(args),
        },

        // ---- Legacy top-level commands (hidden, deprecated) ------------
        Commands::Permissions(a) => {
            deprecation_warning("permissions <SESSION>", "session permissions <SESSION>");
            run_permissions(a)
        }
        Commands::ListSessions(a) => {
            deprecation_warning("list-sessions", "session list");
            run_list(a, quiet)
        }
        Commands::Transcript(a) => {
            deprecation_warning("transcript", "session transcript");
            run_transcript(a, verbose)
        }
        Commands::CurrentSession(a) => {
            deprecation_warning("current-session", "session current");
            run_current(a)
        }
        Commands::LastSession(a) => {
            deprecation_warning("last-session", "session previous");
            run_previous(a)
        }
        Commands::Usage(a) => {
            deprecation_warning("usage", "session usage");
            run_usage(a, quiet)
        }
        Commands::AssistedBy(a) => {
            deprecation_warning("assisted-by", "session assisted-by");
            run_assisted_by(a)
        }

        // ---- Unchanged top-level commands -------------------------------
        Commands::Activity { action } => activity::run(action),
        Commands::Rate {
            instruction,
            test,
            agent_models,
            judge_models,
            timeout,
            no_cache,
            judge_prompt,
        } => rate::run(
            &instruction,
            &test,
            agent_models.as_deref(),
            judge_models.as_deref(),
            timeout,
            no_cache,
            judge_prompt.as_deref(),
            quiet,
        ),
        Commands::TokenUsage {
            timespan,
            sessions,
            projects,
            session_type,
            provider_ids,
            models,
            fields,
            header,
            output,
        } => token_usage::run(
            &timespan,
            sessions,
            projects,
            session_type,
            provider_ids,
            models,
            fields,
            header,
            output.format(),
        ),
    }
}

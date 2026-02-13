//! Command action handlers.

mod activity;
mod list_sessions;
mod permissions;
mod rate;
mod transcript;

use anyhow::Result;

use super::def::Commands;

/// Resolve a session ID: use explicit value if given, else auto-detect.
fn resolve_session(explicit: Option<String>) -> Result<String> {
    match explicit {
        Some(id) => Ok(id),
        None => {
            let detected = crate::session_detect::detect_current_session()?;
            eprintln!(
                "Auto-detected session: {} ({:?})",
                detected.session_id, detected.provider
            );
            Ok(detected.session_id)
        }
    }
}

pub fn dispatch(cmd: Commands, quiet: bool, _verbose: u8) -> Result<()> {
    match cmd {
        Commands::Permissions { session, output } => permissions::run(&session, output.format()),
        Commands::ListSessions {
            session_type,
            search,
            timespan,
            project,
            output,
        } => list_sessions::run(
            session_type,
            search.as_deref(),
            timespan.as_deref(),
            project.as_deref(),
            output.format(),
            quiet,
        ),
        Commands::Transcript {
            session,
            last,
            output,
        } => {
            let session_id = resolve_session(session)?;
            transcript::run(&session_id, last, output.format(), _verbose)
        }
        Commands::Activity { action } => activity::run(action),
        Commands::Rate {
            instruction,
            test,
            agent_models,
            judge_models,
            agent_name,
            timeout,
            no_cache,
            judge_prompt,
        } => rate::run(
            &instruction,
            &test,
            agent_models.as_deref(),
            judge_models.as_deref(),
            agent_name.as_deref(),
            timeout,
            no_cache,
            judge_prompt.as_deref(),
            quiet,
        ),
    }
}

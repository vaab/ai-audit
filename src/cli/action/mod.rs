//! Command action handlers.

mod activity;
mod list_sessions;
mod permissions;
mod rate;

use anyhow::Result;

use super::def::Commands;

pub fn dispatch(cmd: Commands, quiet: bool, _verbose: u8) -> Result<()> {
    match cmd {
        Commands::Permissions { session, output } => permissions::run(&session, output.format()),
        Commands::ListSessions {
            session_type,
            search,
            timespan,
            output,
        } => list_sessions::run(
            session_type,
            search.as_deref(),
            timespan.as_deref(),
            output.format(),
            quiet,
        ),
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

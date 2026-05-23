mod delete;
mod info;
mod nudge;

use anyhow::Result;

use crate::cli::def::{SessionDeleteArgs, SessionInfoArgs, SessionNudgeArgs};

/// Run the `session info` action.
pub fn run_info(args: SessionInfoArgs) -> Result<()> {
    info::run(args)
}

/// Run the `session nudge` action.
pub fn run_nudge(args: SessionNudgeArgs) -> Result<()> {
    nudge::run(args)
}

/// Run the `session delete` action.
pub fn run_delete(args: SessionDeleteArgs) -> Result<()> {
    delete::run(args)
}

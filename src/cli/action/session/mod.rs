mod info;
mod nudge;

use anyhow::Result;

use crate::cli::def::{SessionInfoArgs, SessionNudgeArgs};

/// Run the `session info` action.
pub fn run_info(args: SessionInfoArgs) -> Result<()> {
    info::run(args)
}

/// Run the `session nudge` action.
pub fn run_nudge(args: SessionNudgeArgs) -> Result<()> {
    nudge::run(args)
}

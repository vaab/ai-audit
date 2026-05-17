mod info;
mod nudge;

use anyhow::Result;

use crate::cli::def::SessionAction;

pub fn run(action: SessionAction) -> Result<()> {
    match action {
        SessionAction::Nudge(args) => nudge::run(args),
        SessionAction::Info(args) => info::run(args),
    }
}

//! CLI module - argument parsing and command dispatch.

mod action;
mod def;

pub use def::{Args, Commands};

use anyhow::Result;
use clap::Parser;

#[cfg(unix)]
fn reset_sigpipe() {
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

/// CLI entry point.
pub fn run() -> Result<()> {
    #[cfg(unix)]
    reset_sigpipe();

    // Respect NO_COLOR environment variable
    if std::env::var("NO_COLOR").is_ok() {
        // Future: disable colors when color support is added
        // colored::control::set_override(false);
    }

    let args = Args::parse();
    action::dispatch(args.command, args.quiet, args.verbose)
}

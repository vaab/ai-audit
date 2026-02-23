//! CLI module - argument parsing and command dispatch.

mod action;
mod def;

pub use def::{Args, Commands};

use anyhow::Result;
use clap::Parser;

use crate::OutputFormat;

#[cfg(unix)]
fn reset_sigpipe() {
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

/// Set up automatic paging through `less` for human-readable output.
///
/// Activates only when stdout is a TTY and the output format is `Human`.
/// Uses `less -FRX`: quit-if-one-screen, raw control chars, no init/deinit.
/// Respects the `PAGER` environment variable if set.
fn setup_pager(format: OutputFormat) {
    if format == OutputFormat::Human {
        pager::Pager::with_pager("less -FRX").setup();
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
    setup_pager(args.command.output_format());
    action::dispatch(args.command, args.quiet, args.verbose)
}

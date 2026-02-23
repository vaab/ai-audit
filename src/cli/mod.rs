//! CLI module - argument parsing and command dispatch.

mod action;
pub(crate) mod color;
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

    let args = Args::parse();
    // Detect TTY before pager fork (pager turns stdout into a pipe).
    color::init();
    setup_pager(args.command.output_format());
    action::dispatch(args.command, args.quiet, args.verbose)
}

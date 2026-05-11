//! CLI module - argument parsing and command dispatch.

mod action;
pub(crate) mod color;
pub(crate) mod def;

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

/// Set up logging with level derived from `-q`/`-v` flags.
///
/// Level mapping: `-q` → Error, default → Warn, `-v` → Info,
/// `-vv` → Debug, `-vvv` → Trace.  The `RUST_LOG` env var
/// overrides if set.
fn setup_logging(verbose: u8, quiet: bool) {
    use env_logger::Builder;
    use log::LevelFilter;

    let level = if quiet {
        LevelFilter::Error
    } else {
        match verbose {
            0 => LevelFilter::Warn,
            1 => LevelFilter::Info,
            2 => LevelFilter::Debug,
            _ => LevelFilter::Trace,
        }
    };

    Builder::new()
        .filter_level(level)
        .parse_default_env()
        .format_timestamp(None)
        .init();
}

/// CLI entry point.
pub fn run() -> Result<()> {
    #[cfg(unix)]
    reset_sigpipe();

    let args = Args::parse();
    setup_logging(args.verbose, args.quiet);

    if args.color && args.no_color {
        anyhow::bail!("Cannot use both --color and --no-color");
    }

    // Detect TTY before pager fork (pager turns stdout into a pipe).
    color::init(args.color, args.no_color);
    setup_pager(args.command.output_format());
    action::dispatch(args.command, args.quiet, args.verbose)
}

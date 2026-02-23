//! Color initialization for human-readable output.
//!
//! Must be called before the pager fork, which turns stdout into a pipe
//! and would cause `colored` to think it's not a TTY.

use std::io::IsTerminal;

/// Initialize color support before the pager fork.
///
/// After `pager::Pager::setup()`, stdout is a pipe to `less`, so
/// `colored` would disable colors. We detect the real TTY state here
/// and force colors on when appropriate.
pub fn init() {
    if std::env::var("NO_COLOR").is_ok() {
        colored::control::set_override(false);
    } else if std::io::stdout().is_terminal() {
        colored::control::set_override(true);
    }
}

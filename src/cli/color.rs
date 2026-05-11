//! Color initialization for human-readable output.
//!
//! Must be called before the pager fork, which turns stdout into a pipe
//! and would cause `colored` to think it's not a TTY.

use std::io::IsTerminal;

/// Initialize color support before the pager fork.
///
/// Precedence (highest to lowest):
///   1. Explicit `--color` flag       → force ON
///   2. Explicit `--no-color` flag    → force OFF
///   3. `NO_COLOR` environment var    → force OFF
///   4. stdout is a TTY               → force ON
///   5. otherwise                     → leave `colored` to decide (OFF for pipes)
///
/// Steps 4-5 exist because after `pager::Pager::setup()`, stdout becomes
/// a pipe to `less`, so `colored` would disable colors. We detect the
/// real TTY state here and force colors on when appropriate.
///
/// `force_on` and `force_off` are mutually exclusive — the caller MUST
/// reject the conflict before calling this function.
pub fn init(force_on: bool, force_off: bool) {
    debug_assert!(
        !(force_on && force_off),
        "force_on and force_off are mutually exclusive; caller must reject the conflict"
    );

    if force_on {
        colored::control::set_override(true);
    } else if force_off {
        colored::control::set_override(false);
    } else if std::env::var("NO_COLOR").is_ok() {
        colored::control::set_override(false);
    } else if std::io::stdout().is_terminal() {
        colored::control::set_override(true);
    }
}

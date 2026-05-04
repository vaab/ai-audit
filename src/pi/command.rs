//! Central command builder for all `pi` subprocesses.
//!
//! Mirrors the hermetic argv discipline that
//! `insight-cli/src/pi/command.rs` enforces.  Both crates use pi as
//! a black box subprocess; both pin the same `--no-*` flags for the
//! same reason: the LLM input must be a pure function of the
//! caller-supplied arguments and not pick up arbitrary host state
//! (cwd-walked AGENTS.md, ~/.claude/CLAUDE.md, skill advertisements,
//! extension prompts, prompt templates).

use std::process::Command;

/// Build a `pi` Command pre-populated with the **hermetic flag set**.
///
/// Resulting argv:
///
/// ```text
/// pi --print --mode json --no-session
///    --no-context-files --no-skills --no-extensions
///    --no-prompt-templates --no-themes
/// ```
///
/// Callers append their own flags (`--model`, `--system-prompt`,
/// `--append-system-prompt`, `--tools`, ...) and finally the user
/// message.
pub fn build_hermetic() -> Command {
    let mut cmd = Command::new("pi");
    cmd.args([
        "--print",
        "--mode",
        "json",
        "--no-session",
        "--no-context-files",
        "--no-skills",
        "--no-extensions",
        "--no-prompt-templates",
        "--no-themes",
    ]);
    cmd
}

/// Build a `pi` Command for ancillary subcommands (e.g.
/// `pi --list-models`) where hermetic flags are not relevant.
pub fn build_plain() -> Command {
    Command::new("pi")
}

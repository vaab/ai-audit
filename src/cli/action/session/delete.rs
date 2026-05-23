//! `ai-audit session delete <session-id | --all | --ids-file>` —
//! wipe one or more sessions across every storage location ai-audit
//! reads.
//!
//! Spec: `doc/admin.org § ai-audit / session delete / wipe sessions
//! across all storage`.
//!
//! Implementation phases (see todo list):
//! - 3a: this file (handler skeleton + per-provider dispatch)
//! - 3b: load_ids_file helper with NUL + NDJSON auto-detection
//!
//! For now this is a stub that returns "not yet implemented" so the
//! CLI surface compiles end-to-end while the underlying
//! per-provider `delete_session(id)` functions are built up.

use anyhow::{bail, Result};

use crate::cli::def::SessionDeleteArgs;

pub fn run(_args: SessionDeleteArgs) -> Result<()> {
    bail!(
        "session delete: not yet implemented (Phase 2 / 3 of the \
         implementation roadmap — per-provider delete functions and \
         the main handler are coming up next)"
    )
}

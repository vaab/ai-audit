//! Permissions command handler.

use anyhow::{Context, Result};
use std::fs;

use crate::provider::{self, Provider};
use crate::{claudecode, opencode, OutputFormat};

pub fn run(session: &str, format: OutputFormat) -> Result<()> {
    match provider::detect_provider(session)? {
        Provider::OpenCode => {
            let events = opencode::permissions::parse_events(session)?;
            opencode::permissions::display_events(&events, format);
        }
        Provider::ClaudeCode => {
            let debug_file = claudecode::resolve_debug_file(session);

            if !debug_file.exists() {
                anyhow::bail!("Debug file not found: {}", debug_file.display());
            }

            let content = fs::read_to_string(&debug_file).context("Failed to read debug file")?;

            let mut events = claudecode::permissions::parse_events(&content)?;

            if let Ok(tool_uses) = claudecode::session::load_tool_uses(session) {
                claudecode::permissions::enrich_with_session(&mut events, &tool_uses);
            }

            claudecode::permissions::display_events(&events, format);
        }
        Provider::Pi => {
            anyhow::bail!("permissions: pi has no permission/approval events to display");
        }
    }

    Ok(())
}

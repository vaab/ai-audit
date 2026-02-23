//! Provider-agnostic abstractions for AI assistant sessions.
//!
//! This module defines the shared types and traits that all provider
//! backends (Claude Code, OpenCode, future providers) must implement.
//! Consumer code (CLI actions, session detection, activity tracking)
//! uses these abstractions exclusively, so adding a new provider
//! requires only implementing `SessionProvider` — no other code changes.

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::transcript::TranscriptEntry;

/// AI assistant provider identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    ClaudeCode,
    OpenCode,
}

impl Provider {
    pub fn as_str(&self) -> &'static str {
        match self {
            Provider::ClaudeCode => "claudecode",
            Provider::OpenCode => "opencode",
        }
    }
}

impl std::fmt::Display for Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Detect the provider for a session ID based on its format.
///
/// - `ses_*` prefix -> OpenCode
/// - UUID format -> Claude Code
pub fn detect_provider(session_id: &str) -> Provider {
    if session_id.starts_with("ses_") {
        Provider::OpenCode
    } else {
        Provider::ClaudeCode
    }
}

/// Provider-agnostic session metadata.
///
/// Unified representation of a session from any provider. All fields
/// common to Claude Code and OpenCode are present; provider-specific
/// extras (like `parent_id` for OpenCode sub-agents) use `Option`.
#[derive(Debug, Clone, Serialize)]
pub struct Session {
    pub session_id: String,
    pub provider: Provider,
    pub started_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub project_dir: String,
    pub title: String,
    /// Parent session ID (present for sub-agent sessions in OpenCode).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
}

/// Trait that each provider backend implements to supply session data.
///
/// Adding a new provider (e.g., Codex) requires only implementing this
/// trait. All CLI actions and cross-provider logic use `SessionProvider`
/// exclusively.
pub trait SessionProvider {
    /// Which provider this adapter serves.
    fn provider(&self) -> Provider;

    /// List all sessions available from this provider.
    fn list_sessions(&self) -> Result<Vec<Session>>;

    /// Check if a session's messages contain the given text.
    fn session_contains_text(&self, session_id: &str, needle: &str) -> bool;

    /// Check if a session includes a tool_use that wrote/edited the given file.
    fn session_edited_file(&self, session_id: &str, file_path: &str) -> bool;

    /// Check if the last N messages of a session contain the given text.
    fn session_tail_contains_text(&self, session_id: &str, needle: &str, last_n: usize) -> bool;

    /// Parse a full conversation transcript from a session.
    fn parse_transcript(&self, session_id: &str) -> Result<Vec<TranscriptEntry>>;
}

/// Get the provider adapter for a given session ID.
///
/// Returns a boxed `SessionProvider` based on the session ID format.
pub fn provider_for_session(session_id: &str) -> Box<dyn SessionProvider> {
    match detect_provider(session_id) {
        Provider::ClaudeCode => Box::new(crate::claudecode::ClaudeCodeProvider),
        Provider::OpenCode => Box::new(crate::opencode::OpenCodeProvider),
    }
}

/// Get all available provider adapters.
pub fn all_providers() -> Vec<Box<dyn SessionProvider>> {
    vec![
        Box::new(crate::claudecode::ClaudeCodeProvider),
        Box::new(crate::opencode::OpenCodeProvider),
    ]
}

/// Get the provider adapter for a specific provider.
pub fn provider_for(provider: Provider) -> Box<dyn SessionProvider> {
    match provider {
        Provider::ClaudeCode => Box::new(crate::claudecode::ClaudeCodeProvider),
        Provider::OpenCode => Box::new(crate::opencode::OpenCodeProvider),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_provider_opencode() {
        assert_eq!(detect_provider("ses_abc123"), Provider::OpenCode);
        assert_eq!(detect_provider("ses_"), Provider::OpenCode);
    }

    #[test]
    fn test_detect_provider_claudecode() {
        assert_eq!(
            detect_provider("550e8400-e29b-41d4-a716-446655440000"),
            Provider::ClaudeCode
        );
        assert_eq!(detect_provider("some-uuid"), Provider::ClaudeCode);
    }

    #[test]
    fn test_provider_as_str() {
        assert_eq!(Provider::ClaudeCode.as_str(), "claudecode");
        assert_eq!(Provider::OpenCode.as_str(), "opencode");
    }

    #[test]
    fn test_provider_display() {
        assert_eq!(format!("{}", Provider::ClaudeCode), "claudecode");
        assert_eq!(format!("{}", Provider::OpenCode), "opencode");
    }
}

pub mod permissions;
pub mod session;
pub mod transcript;

use std::path::PathBuf;

use anyhow::Result;

use crate::provider::{Provider, Session, SessionProvider};
use crate::transcript::TranscriptEntry;

pub fn debug_dir() -> PathBuf {
    crate::claudecode_data_dir().join("debug")
}

pub fn projects_dir() -> PathBuf {
    crate::claudecode_data_dir().join("projects")
}

pub fn resolve_debug_file(session_id: &str) -> PathBuf {
    debug_dir().join(format!("{}.txt", session_id))
}

/// Claude Code provider adapter.
pub struct ClaudeCodeProvider;

impl SessionProvider for ClaudeCodeProvider {
    fn provider(&self) -> Provider {
        Provider::ClaudeCode
    }

    fn list_sessions(&self) -> Result<Vec<Session>> {
        let sessions = session::list_sessions()?;
        Ok(sessions
            .into_iter()
            .map(|s| Session {
                session_id: s.session_id,
                provider: Provider::ClaudeCode,
                started_at: s.started_at,
                updated_at: s.updated_at,
                project_dir: s.project_dir,
                title: s.title,
                parent_id: None,
            })
            .collect())
    }

    fn session_contains_text(&self, session_id: &str, needle: &str) -> bool {
        session::session_contains_text(session_id, needle)
    }

    fn session_edited_file(&self, session_id: &str, file_path: &str) -> bool {
        session::session_edited_file(session_id, file_path)
    }

    fn session_tail_contains_text(&self, session_id: &str, needle: &str, last_n: usize) -> bool {
        session::session_tail_contains_text(session_id, needle, last_n)
    }

    fn parse_transcript(&self, session_id: &str) -> Result<Vec<TranscriptEntry>> {
        transcript::parse_transcript(session_id)
    }
}

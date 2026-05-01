//! Pi (badlogic/pi-mono) provider adapter.
//!
//! Pi stores sessions as JSONL files under
//! `~/.pi/agent/sessions/--<encoded-cwd>--/<iso-ts>_<uuid>.jsonl`.
//! The base directory can be overridden by the `PI_CODING_AGENT_DIR`
//! environment variable.
//!
//! Sub-agent sessions (e.g. those spawned by the `pi-subagents`
//! extension) are nested under the parent's directory:
//! `<parent-base>/<parent-uuid-dir>/<entry-id>/run-N/session.jsonl`.
//! Their parent UUID is the directory name two levels above the file.
//!
//! Pi has no permission/approval model, so this module exposes no
//! `permissions` submodule.
//!
//! Project directory is read from the `cwd` field of the JSONL
//! header line — never from the encoded directory name (the encoding
//! `/` → `-` is lossy).

pub mod session;
pub mod transcript;

use std::path::PathBuf;

use anyhow::Result;

use crate::provider::{Message, Provider, Session, SessionProvider};
use crate::transcript::TranscriptEntry;

/// Default Pi data directory, honoring the `PI_CODING_AGENT_DIR` override.
pub fn pi_data_dir() -> PathBuf {
    if let Ok(custom) = std::env::var("PI_CODING_AGENT_DIR") {
        if !custom.is_empty() {
            return PathBuf::from(custom);
        }
    }
    dirs::home_dir()
        .expect("Could not find home directory")
        .join(".pi")
        .join("agent")
}

/// Pi sessions directory.
pub fn sessions_dir() -> PathBuf {
    pi_data_dir().join("sessions")
}

/// Pi provider adapter.
pub struct PiProvider;

impl SessionProvider for PiProvider {
    fn provider(&self) -> Provider {
        Provider::Pi
    }

    fn list_sessions(&self) -> Result<Vec<Session>> {
        let sessions = session::list_sessions()?;
        Ok(sessions
            .into_iter()
            .map(|s| Session {
                session_id: s.session_id,
                provider: Provider::Pi,
                started_at: s.started_at,
                updated_at: s.updated_at,
                project_dir: s.project_dir,
                title: s.title,
                parent_id: s.parent_id,
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

    fn list_messages(&self, session_id: &str) -> Result<Vec<Message>> {
        session::list_messages(session_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pi_data_dir_default() {
        // Ensure PI_CODING_AGENT_DIR is not set for this test
        // SAFETY: Test is single-threaded for this var.
        unsafe { std::env::remove_var("PI_CODING_AGENT_DIR") };
        let dir = pi_data_dir();
        assert!(dir.ends_with(".pi/agent"));
    }

    #[test]
    fn test_pi_data_dir_override() {
        // SAFETY: Test guards by setting then unsetting.
        unsafe { std::env::set_var("PI_CODING_AGENT_DIR", "/tmp/custom-pi-dir") };
        let dir = pi_data_dir();
        assert_eq!(dir, PathBuf::from("/tmp/custom-pi-dir"));
        unsafe { std::env::remove_var("PI_CODING_AGENT_DIR") };
    }

    #[test]
    fn test_sessions_dir_under_data_dir() {
        unsafe { std::env::set_var("PI_CODING_AGENT_DIR", "/tmp/test-pi") };
        assert_eq!(sessions_dir(), PathBuf::from("/tmp/test-pi/sessions"));
        unsafe { std::env::remove_var("PI_CODING_AGENT_DIR") };
    }
}

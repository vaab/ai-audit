pub mod info;
pub mod permissions;
pub mod session;
pub mod session_index;
pub mod transcript;

use std::path::PathBuf;

use anyhow::Result;

use crate::provider::{
    brand_for_llm_provider, infer_llm_provider_from_model, Message, ModelAttribution, Provider,
    Session, SessionProvider,
};
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

    fn list_messages(&self, session_id: &str) -> Result<Vec<Message>> {
        session::list_messages(session_id)
    }

    fn resolve_attribution(&self, session_id: &str) -> Result<ModelAttribution> {
        let messages = session::list_messages(session_id)?;
        resolve_attribution_from_messages(&messages, session_id)
    }
}

/// Build a [`ModelAttribution`] from a list of Claude Code messages.
///
/// Picks the most recent assistant message that carries a `model`
/// field, then infers the LLM provider from the model-id family
/// (Claude Code does not record the provider directly).  Inference
/// always sets `llm_provider_inferred = true`.  Unknown families and
/// unmapped brands produce a hard error rather than a generic
/// substitute — see `doc/admin.org` for the policy.
pub fn resolve_attribution_from_messages(
    messages: &[Message],
    session_id: &str,
) -> Result<ModelAttribution> {
    let last_assistant = messages
        .iter()
        .rev()
        .find(|m| m.role == "assistant" && m.model.is_some())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no assistant message with a model id found in Claude Code \
                 session {}",
                session_id
            )
        })?;
    let model_id = last_assistant
        .model
        .clone()
        .expect("filtered for model.is_some() above");

    let inferred = infer_llm_provider_from_model(&model_id).ok_or_else(|| {
        anyhow::anyhow!(
            "Claude Code session {} has model={:?}, but its family is not \
             recognised by `provider::infer_llm_provider_from_model` — cannot \
             infer the LLM provider for the Assisted-by trailer",
            session_id,
            model_id
        )
    })?;
    if brand_for_llm_provider(inferred).is_none() {
        anyhow::bail!(
            "Claude Code session {} inferred llm_provider {:?} which has no \
             brand mapping; teach `provider::brand_for_llm_provider`",
            session_id,
            inferred
        );
    }

    Ok(ModelAttribution {
        harness: Provider::ClaudeCode,
        llm_provider: Some(inferred.to_string()),
        llm_provider_inferred: true,
        model_id,
        access_surface: "ClaudeCode".to_string(),
        agent: None,
        mode: None,
        variant: None,
    })
}

#[cfg(test)]
mod attribution_tests {
    use super::*;
    use crate::provider::{Message, Provider, TokenUsage};
    use chrono::TimeZone;

    fn assistant(model: &str, ts_secs: i64) -> Message {
        Message {
            message_id: format!("msg-{}", ts_secs),
            session_id: "ses".to_string(),
            provider: Provider::ClaudeCode,
            role: "assistant".to_string(),
            model: Some(model.to_string()),
            provider_id: None,
            agent: None,
            mode: None,
            timestamp: chrono::Utc.timestamp_opt(ts_secs, 0).unwrap(),
            tokens: Some(TokenUsage::default()),
        }
    }

    #[test]
    fn test_resolve_attribution_claude_infers_anthropic() {
        let msgs = vec![assistant("claude-opus-4-1", 1_000)];
        let attr = resolve_attribution_from_messages(&msgs, "ses").unwrap();
        assert_eq!(attr.llm_provider.as_deref(), Some("anthropic"));
        assert!(attr.llm_provider_inferred);
        assert_eq!(attr.model_id, "claude-opus-4-1");
        assert_eq!(
            attr.trailer().unwrap(),
            "Assisted-by: Claude:claude-opus-4-1"
        );
    }

    #[test]
    fn test_resolve_attribution_claude_picks_most_recent_assistant() {
        // Older assistant turn is ignored when a newer one exists.
        let msgs = vec![
            assistant("claude-haiku-3-5", 1_000),
            assistant("claude-sonnet-4-5", 2_000),
        ];
        let attr = resolve_attribution_from_messages(&msgs, "ses").unwrap();
        assert_eq!(attr.model_id, "claude-sonnet-4-5");
    }

    #[test]
    fn test_resolve_attribution_claude_unknown_family_errors() {
        // Non-Claude model in a Claude Code session — we must NOT
        // hand-wave attribution.  Loud failure is the contract.
        let msgs = vec![assistant("custom-fork-of-something", 1_000)];
        let err = resolve_attribution_from_messages(&msgs, "ses").expect_err("must error");
        assert!(err.to_string().contains("custom-fork-of-something"));
    }

    #[test]
    fn test_resolve_attribution_claude_no_assistant_errors() {
        let err = resolve_attribution_from_messages(&[], "ses-x").expect_err("must error");
        assert!(err.to_string().contains("ses-x"));
    }
}

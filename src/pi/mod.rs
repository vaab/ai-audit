//! Pi (badlogic/pi-mono) provider adapter.
//!
//! ## Forensic side (read)
//!
//! Pi stores sessions as JSONL files under
//! `~/.pi/agent/sessions/--<encoded-cwd>--/<iso-ts>_<uuid>.jsonl`.
//! The base directory can be overridden by the `PI_CODING_AGENT_DIR`
//! environment variable.  Sub-agent sessions (e.g. those spawned by
//! the `pi-subagents` extension) are nested under the parent's
//! directory: `<parent-base>/<parent-uuid-dir>/<entry-id>/run-N/session.jsonl`.
//! Their parent UUID is the directory name two levels above the
//! file.
//!
//! Pi has no permission/approval model, so this module exposes no
//! `permissions` submodule.
//!
//! Project directory is read from the `cwd` field of the JSONL
//! header line — never from the encoded directory name (the encoding
//! `/` → `-` is lossy).
//!
//! ## Invocation side (write)
//!
//! Use [`run::run`] to spawn `pi --print --mode json` with the
//! hermetic flag set built by [`command::build_hermetic`].  See
//! the module docs for [`run`] and [`sanity`] for the contract.

pub mod command;
pub mod run;
pub mod sanity;
pub mod session;
pub mod session_index;
pub mod transcript;

pub use run::{AiTaskResult, RunOptions};
pub use sanity::{AiTaskSpec, LlmOutputCutShort};

use std::path::PathBuf;

use anyhow::Result;

use crate::provider::{
    brand_for_llm_provider, Message, ModelAttribution, Provider, Session, SessionProvider,
};
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

    fn resolve_attribution(&self, session_id: &str) -> Result<ModelAttribution> {
        let raw = session::read_session_jsonl(session_id)?;
        resolve_attribution_from_jsonl(&raw, session_id)
    }
}

/// Resolve attribution for a Pi session given the raw JSONL content.
///
/// Strategy:
///
/// 1. Parse all assistant messages and all `model_change` events.
/// 2. Take the most recent assistant message.  If it carries both
///    `provider` and `model`, use those.
/// 3. Otherwise, look for the most recent `model_change` event whose
///    timestamp is `<=` the assistant message's timestamp, and use its
///    fields to fill any gaps left by the message.
/// 4. Missing `model` is a hard error.  Missing `provider` falls back
///    to inference (`infer_llm_provider_from_model`); when inference
///    succeeds, `llm_provider_inferred` is set to `true`.  When neither
///    a recorded nor inferable provider exists, error out.
///
/// Held to the same "no silent fallback" rule as the rest of the
/// resolver: if the brand cannot be mapped, return an error rather
/// than emitting a non-canonical trailer downstream.
pub fn resolve_attribution_from_jsonl(content: &str, session_id: &str) -> Result<ModelAttribution> {
    let messages = session::parse_messages(content, session_id)?;
    let model_changes = session::parse_model_changes(content);

    let last_assistant = messages
        .iter()
        .rev()
        .find(|m| m.role == "assistant")
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no assistant message found in Pi session {} \
                 — cannot resolve attribution",
                session_id
            )
        })?;

    // Most recent model_change event at or before the assistant turn.
    let prior_change = model_changes
        .iter()
        .filter(|ev| ev.timestamp <= last_assistant.timestamp)
        .next_back();

    let model_id = last_assistant
        .model
        .clone()
        .or_else(|| prior_change.and_then(|ev| ev.model.clone()))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Pi session {} has no model id on the last assistant message \
                 nor in any prior model_change event",
                session_id
            )
        })?;

    let mut llm_provider = last_assistant
        .provider_id
        .clone()
        .or_else(|| prior_change.and_then(|ev| ev.provider.clone()));
    let mut llm_provider_inferred = false;
    if llm_provider.is_none() {
        if let Some(inferred) = crate::provider::infer_llm_provider_from_model(&model_id) {
            llm_provider = Some(inferred.to_string());
            llm_provider_inferred = true;
        }
    }

    if let Some(ref pid) = llm_provider {
        if brand_for_llm_provider(pid).is_none() {
            anyhow::bail!(
                "Pi session {} resolved llm_provider {:?} which has no brand mapping; \
                 teach `provider::brand_for_llm_provider` if this is a real provider",
                session_id,
                pid
            );
        }
    } else {
        anyhow::bail!(
            "Pi session {} has model={:?} but no provider field on the assistant \
             message, no prior model_change event, and no inferable family — cannot \
             build a kernel-canonical Assisted-by trailer",
            session_id,
            model_id
        );
    }

    Ok(ModelAttribution {
        harness: Provider::Pi,
        llm_provider,
        llm_provider_inferred,
        model_id,
        access_surface: "Pi".to_string(),
        agent: last_assistant.agent.clone(),
        mode: last_assistant.mode.clone(),
        variant: None,
    })
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

    // === Attribution resolver acceptance tests ===

    use indoc::indoc;

    #[test]
    fn test_resolve_attribution_pi_current_shape() {
        // Live Pi shape: message.provider + message.model on assistant.
        let jsonl = indoc! {r#"
            {"type":"session","version":3,"id":"x","timestamp":"2026-04-30T09:36:43Z","cwd":"/tmp"}
            {"type":"message","id":"a1","timestamp":"2026-04-30T09:37:01Z","message":{"role":"assistant","provider":"openai-codex","model":"gpt-5.5","usage":{"input":1,"output":1}}}
        "#};
        let attr = resolve_attribution_from_jsonl(jsonl, "ses").unwrap();
        assert_eq!(attr.harness, Provider::Pi);
        assert_eq!(attr.llm_provider.as_deref(), Some("openai-codex"));
        assert!(!attr.llm_provider_inferred);
        assert_eq!(attr.model_id, "gpt-5.5");
        assert_eq!(attr.trailer().unwrap(), "Assisted-by: GPT:gpt-5.5");
    }

    #[test]
    fn test_resolve_attribution_pi_uses_model_change_when_assistant_lacks_provider() {
        // Assistant message has only `model` (or only `modelID`); the
        // resolver must fall back to the most recent model_change for
        // the LLM provider.
        let jsonl = indoc! {r#"
            {"type":"session","version":3,"id":"x","timestamp":"2026-04-30T09:36:43Z","cwd":"/tmp"}
            {"type":"model_change","id":"m1","timestamp":"2026-04-30T09:36:44Z","provider":"anthropic","modelId":"claude-opus-4-7"}
            {"type":"message","id":"a1","timestamp":"2026-04-30T09:37:01Z","message":{"role":"assistant","modelID":"claude-opus-4-7","usage":{"input":1,"output":1}}}
        "#};
        let attr = resolve_attribution_from_jsonl(jsonl, "ses").unwrap();
        assert_eq!(attr.llm_provider.as_deref(), Some("anthropic"));
        assert!(!attr.llm_provider_inferred);
        assert_eq!(attr.model_id, "claude-opus-4-7");
        assert_eq!(
            attr.trailer().unwrap(),
            "Assisted-by: Claude:claude-opus-4-7"
        );
    }

    #[test]
    fn test_resolve_attribution_pi_legacy_modelid_inferred_anthropic() {
        // Pure legacy fixture: only `modelID`, no provider anywhere.
        // Must infer `anthropic` from the `claude-` family and mark
        // the result as inferred.
        let jsonl = indoc! {r#"
            {"type":"session","version":3,"id":"x","timestamp":"2026-04-30T09:36:43Z","cwd":"/tmp"}
            {"type":"message","id":"a1","timestamp":"2026-04-30T09:37:01Z","message":{"role":"assistant","modelID":"claude-opus-4-7","usage":{"input":1,"output":1}}}
        "#};
        let attr = resolve_attribution_from_jsonl(jsonl, "ses").unwrap();
        assert_eq!(attr.llm_provider.as_deref(), Some("anthropic"));
        assert!(attr.llm_provider_inferred);
        assert_eq!(attr.model_id, "claude-opus-4-7");
    }

    #[test]
    fn test_resolve_attribution_pi_unknown_family_errors_loudly() {
        // No provider, model family the inference table cannot map:
        // must error, never invent attribution.
        let jsonl = indoc! {r#"
            {"type":"session","version":3,"id":"x","timestamp":"2026-04-30T09:36:43Z","cwd":"/tmp"}
            {"type":"message","id":"a1","timestamp":"2026-04-30T09:37:01Z","message":{"role":"assistant","modelID":"unknown-model-9000","usage":{"input":1,"output":1}}}
        "#};
        let err = resolve_attribution_from_jsonl(jsonl, "ses")
            .expect_err("must fail for unknown family with no recorded provider");
        let msg = err.to_string();
        assert!(
            msg.contains("unknown-model-9000") || msg.contains("provider"),
            "error should mention the model or missing provider: {msg}"
        );
    }

    #[test]
    fn test_resolve_attribution_pi_no_assistant_errors() {
        // Session with no assistant turn at all.
        let jsonl = indoc! {r#"
            {"type":"session","version":3,"id":"x","timestamp":"2026-04-30T09:36:43Z","cwd":"/tmp"}
            {"type":"message","id":"u1","timestamp":"2026-04-30T09:37:01Z","message":{"role":"user","content":"hello"}}
        "#};
        let err = resolve_attribution_from_jsonl(jsonl, "ses-empty").expect_err("must error");
        assert!(err.to_string().contains("ses-empty"));
    }
}

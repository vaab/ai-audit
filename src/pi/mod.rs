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
pub mod info;
pub mod run;
pub mod sanity;
pub mod session;
pub mod session_index;
pub mod transcript;

pub use run::{AiTaskResult, RunOptions};
pub use sanity::{AiTaskSpec, LlmOutputCutShort};

use std::path::PathBuf;

use anyhow::{Context, Result};

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

/// Report of what was wiped by [`delete_session`] for one Pi session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PiDeleteReport {
    /// Absolute paths of JSONL files unlinked.  When `--cascade` was
    /// used and child sessions were deleted, their JSONL files appear
    /// here too (one entry per file).
    pub paths: Vec<PathBuf>,
    /// `true` when the parent project directory was pruned because
    /// its last session was deleted.
    pub pruned_project_dir: bool,
    /// Number of child sessions cascaded.  Equal to 0 when
    /// `--cascade` wasn't passed or there were no children.
    pub cascaded: usize,
}

impl PiDeleteReport {
    pub fn count(&self) -> usize {
        self.paths.len()
    }
}

/// Discover every session whose `parent_id` is `session_id`.
///
/// Used by [`delete_session`] for the `--cascade` path.  Returns an
/// empty vec when the session has no children (the common case) or
/// when the sessions directory is missing.
pub fn list_child_sessions(session_id: &str) -> Result<Vec<String>> {
    let all = session::list_sessions()?;
    Ok(all
        .into_iter()
        .filter(|s| s.parent_id.as_deref() == Some(session_id))
        .map(|s| s.session_id)
        .collect())
}

/// Wipe one Pi session.
///
/// Pi has no permission/approval model, no SQLite database, no
/// auxiliary log files keyed by session id, no run cache that
/// references the session.  The only on-disk state is the JSONL
/// transcript plus the ai-audit session-index cache entry.
///
/// Order:
///   1. Remove the session-index cache entry (cheap, in-process).
///   2. If `cascade` is `true` AND the session has children, also
///      remove each child's JSONL file and cache entry (recursive,
///      depth-first).  If `cascade` is `false` and children exist,
///      this function returns an error — the caller decides whether
///      to retry with cascade.
///   3. Remove the session's own JSONL file (missing → silent).
///   4. Try `rmdir` on the parent project directory (silent if
///      non-empty or permission-denied — never an error).
///   5. Persist the updated cache.
///
/// `dry_run` mode performs zero writes — reports what WOULD be wiped.
pub fn delete_session(session_id: &str, cascade: bool, dry_run: bool) -> Result<PiDeleteReport> {
    use crate::session_index::{self as idx, CachedHarnessIndex};

    let mut report = PiDeleteReport::default();

    // -- Cascade gate --------------------------------------------
    let children = list_child_sessions(session_id).unwrap_or_default();
    if !children.is_empty() && !cascade {
        anyhow::bail!(
            "pi session {} has {} child session(s) ({}); pass --cascade \
             to recursively delete the subtree",
            session_id,
            children.len(),
            preview_ids(&children),
        );
    }

    // -- Locate JSONL files ---------------------------------------
    let jsonl_self = session::find_session_file(session_id);
    let mut jsonl_children: Vec<(String, PathBuf)> = Vec::new();
    for child_id in &children {
        if let Some(path) = session::find_session_file(child_id) {
            jsonl_children.push((child_id.clone(), path));
        }
    }

    // -- Dry-run ---------------------------------------------------
    if dry_run {
        for (_, path) in &jsonl_children {
            report.paths.push(path.clone());
        }
        if let Some(path) = &jsonl_self {
            report.paths.push(path.clone());
            if let Some(parent) = path.parent() {
                if would_become_empty_after(parent, std::iter::once(path.as_path())) {
                    report.paths.push(parent.to_path_buf());
                    report.pruned_project_dir = true;
                }
            }
        }
        report.cascaded = children.len();
        return Ok(report);
    }

    // -- Real wipe ------------------------------------------------
    let mut cache = idx::load_harness_index(session_index::CACHE_FILE)
        .unwrap_or_else(CachedHarnessIndex::empty);
    cache.remove_session(session_id);

    // 2. Children (cascade).
    for (child_id, path) in jsonl_children {
        if path.exists() {
            std::fs::remove_file(&path).with_context(|| {
                format!(
                    "Failed to remove pi child session {} at {}",
                    child_id,
                    path.display()
                )
            })?;
            log::debug!(
                "pi delete {} (cascade child {}): removed {}",
                session_id,
                child_id,
                path.display()
            );
            report.paths.push(path);
        }
        cache.remove_session(&child_id);
        report.cascaded += 1;
    }

    // 3. Self.
    if let Some(path) = jsonl_self {
        if path.exists() {
            std::fs::remove_file(&path)
                .with_context(|| format!("Failed to remove pi session file {}", path.display()))?;
            log::debug!("pi delete {}: removed {}", session_id, path.display());
            report.paths.push(path.clone());

            // 4. Prune empty project dir (silent failure on non-empty).
            if let Some(parent) = path.parent() {
                if std::fs::remove_dir(parent).is_ok() {
                    log::debug!(
                        "pi delete {}: pruned empty project dir {}",
                        session_id,
                        parent.display()
                    );
                    report.paths.push(parent.to_path_buf());
                    report.pruned_project_dir = true;
                }
            }
        }
    }

    // 5. Persist cache.
    let cache_dir = idx::cache_dir().ok_or_else(|| {
        anyhow::anyhow!("Unable to resolve XDG cache dir for session-index cache")
    })?;
    std::fs::create_dir_all(&cache_dir)
        .with_context(|| format!("Failed to create cache dir {}", cache_dir.display()))?;
    idx::save_harness_index(session_index::CACHE_FILE, &cache)?;

    Ok(report)
}

/// Preview a slice of IDs for inclusion in an error message —
/// truncate at 5 with `... and N more` overflow.
fn preview_ids(ids: &[String]) -> String {
    if ids.len() <= 5 {
        return ids.join(", ");
    }
    let head = ids.iter().take(5).cloned().collect::<Vec<_>>().join(", ");
    format!("{}, ... and {} more", head, ids.len() - 5)
}

/// Predicate: would `parent` be empty after the iterator of children
/// is removed?  Used for dry-run prune prediction (Pi).
fn would_become_empty_after<'a, I: IntoIterator<Item = &'a std::path::Path>>(
    parent: &std::path::Path,
    children: I,
) -> bool {
    let to_remove: std::collections::HashSet<&std::path::Path> = children.into_iter().collect();
    let entries = match std::fs::read_dir(parent) {
        Ok(it) => it,
        Err(_) => return false,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !to_remove.contains(path.as_path()) {
            return false;
        }
    }
    true
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
mod delete_tests {
    use super::*;
    use std::path::Path;
    use tempfile::TempDir;

    // Shared crate-level lock — see ``crate::TEST_ENV_LOCK``.
    use crate::TEST_ENV_LOCK as ENV_LOCK;

    struct EnvGuard {
        pi_dir: Option<String>,
        home: Option<String>,
        xdg_cache_home: Option<String>,
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.pi_dir {
                    Some(v) => std::env::set_var("PI_CODING_AGENT_DIR", v),
                    None => std::env::remove_var("PI_CODING_AGENT_DIR"),
                }
                match &self.home {
                    Some(v) => std::env::set_var("HOME", v),
                    None => std::env::remove_var("HOME"),
                }
                match &self.xdg_cache_home {
                    Some(v) => std::env::set_var("XDG_CACHE_HOME", v),
                    None => std::env::remove_var("XDG_CACHE_HOME"),
                }
            }
        }
    }

    fn setup_env(home: &Path, pi_dir: &Path) -> EnvGuard {
        let guard = EnvGuard {
            pi_dir: std::env::var("PI_CODING_AGENT_DIR").ok(),
            home: std::env::var("HOME").ok(),
            xdg_cache_home: std::env::var("XDG_CACHE_HOME").ok(),
        };
        unsafe {
            std::env::set_var("PI_CODING_AGENT_DIR", pi_dir);
            std::env::set_var("HOME", home);
            std::env::set_var("XDG_CACHE_HOME", home.join(".cache"));
        }
        guard
    }

    /// Write a top-level pi session JSONL.  Returns the file path.
    fn seed_top_level(
        pi_dir: &Path,
        encoded_cwd: &str,
        session_uuid: &str,
        cwd: &str,
    ) -> std::path::PathBuf {
        let project_dir = pi_dir.join("sessions").join(encoded_cwd);
        std::fs::create_dir_all(&project_dir).unwrap();
        let path = project_dir.join(format!("2026-01-01T00-00-00_{}.jsonl", session_uuid));
        let body = format!(
            r#"{{"type":"session","version":3,"id":"{}","timestamp":"2026-01-01T00:00:00Z","cwd":"{}"}}"#,
            session_uuid, cwd
        );
        std::fs::write(&path, body).unwrap();
        path
    }

    /// Write a sub-agent pi session JSONL whose directory layout
    /// makes `derive_parent_id` recover the parent UUID from the
    /// ancestor dir name.  Returns the file path.
    fn seed_subagent(
        pi_dir: &Path,
        encoded_cwd: &str,
        parent_uuid: &str,
        entry_id: &str,
        run_n: usize,
        child_uuid: &str,
        cwd: &str,
    ) -> std::path::PathBuf {
        let dir = pi_dir
            .join("sessions")
            .join(encoded_cwd)
            .join(format!("2026-01-01T00-00-00_{}", parent_uuid))
            .join(entry_id)
            .join(format!("run-{}", run_n));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.jsonl");
        let body = format!(
            r#"{{"type":"session","version":3,"id":"{}","timestamp":"2026-01-01T00:00:01Z","cwd":"{}"}}"#,
            child_uuid, cwd
        );
        std::fs::write(&path, body).unwrap();
        path
    }

    // Hand-built UUIDv7 examples — version nibble is `7` at offset
    // 14, variant nibble is `8` at offset 19.  Hyphens at 8, 13, 18, 23.
    const UUID_TOP: &str = "019191cd-7be0-7000-8000-000000000001";
    const UUID_CHILD_1: &str = "019191cd-7be0-7000-8000-000000000002";
    const UUID_CHILD_2: &str = "019191cd-7be0-7000-8000-000000000003";
    const UUID_UNRELATED: &str = "019191cd-7be0-7000-8000-00000000000a";

    #[test]
    fn delete_session_removes_top_level_jsonl_and_prunes_dir() {
        let _lock = ENV_LOCK.lock().unwrap();
        let tmp_home = TempDir::new().unwrap();
        let tmp_pi = TempDir::new().unwrap();
        let _guard = setup_env(tmp_home.path(), tmp_pi.path());

        let path = seed_top_level(tmp_pi.path(), "-tmp-proj", UUID_TOP, "/tmp/proj");
        let project_dir = path.parent().unwrap().to_path_buf();

        let report = delete_session(UUID_TOP, false, false).unwrap();

        assert!(!path.exists());
        assert!(!project_dir.exists(), "empty project dir should be pruned");
        assert!(report.pruned_project_dir);
        assert_eq!(report.cascaded, 0);
    }

    #[test]
    fn delete_session_keeps_non_empty_project_dir() {
        let _lock = ENV_LOCK.lock().unwrap();
        let tmp_home = TempDir::new().unwrap();
        let tmp_pi = TempDir::new().unwrap();
        let _guard = setup_env(tmp_home.path(), tmp_pi.path());

        let target = seed_top_level(tmp_pi.path(), "-tmp-proj", UUID_TOP, "/tmp/proj");
        let sibling = seed_top_level(tmp_pi.path(), "-tmp-proj", UUID_UNRELATED, "/tmp/proj");
        let project_dir = target.parent().unwrap().to_path_buf();

        let report = delete_session(UUID_TOP, false, false).unwrap();

        assert!(!target.exists());
        assert!(sibling.exists(), "sibling session must survive");
        assert!(project_dir.exists(), "project dir kept (has sibling)");
        assert!(!report.pruned_project_dir);
    }

    #[test]
    fn delete_session_refuses_without_cascade_when_children_exist() {
        let _lock = ENV_LOCK.lock().unwrap();
        let tmp_home = TempDir::new().unwrap();
        let tmp_pi = TempDir::new().unwrap();
        let _guard = setup_env(tmp_home.path(), tmp_pi.path());

        let parent = seed_top_level(tmp_pi.path(), "-tmp-proj", UUID_TOP, "/tmp/proj");
        let child = seed_subagent(
            tmp_pi.path(),
            "-tmp-proj",
            UUID_TOP,
            "entry_1",
            0,
            UUID_CHILD_1,
            "/tmp/proj",
        );

        let err = delete_session(UUID_TOP, false, false).expect_err("must refuse");
        let msg = err.to_string();
        assert!(msg.contains("--cascade"));
        assert!(msg.contains(UUID_CHILD_1) || msg.contains("1 child"));

        // Files untouched.
        assert!(parent.exists());
        assert!(child.exists());
    }

    #[test]
    fn delete_session_cascade_removes_children_and_parent() {
        let _lock = ENV_LOCK.lock().unwrap();
        let tmp_home = TempDir::new().unwrap();
        let tmp_pi = TempDir::new().unwrap();
        let _guard = setup_env(tmp_home.path(), tmp_pi.path());

        let parent = seed_top_level(tmp_pi.path(), "-tmp-proj", UUID_TOP, "/tmp/proj");
        let child_1 = seed_subagent(
            tmp_pi.path(),
            "-tmp-proj",
            UUID_TOP,
            "entry_1",
            0,
            UUID_CHILD_1,
            "/tmp/proj",
        );
        let child_2 = seed_subagent(
            tmp_pi.path(),
            "-tmp-proj",
            UUID_TOP,
            "entry_1",
            1,
            UUID_CHILD_2,
            "/tmp/proj",
        );

        let report = delete_session(UUID_TOP, true, false).unwrap();

        assert!(!parent.exists());
        assert!(!child_1.exists());
        assert!(!child_2.exists());
        assert_eq!(report.cascaded, 2);
    }

    #[test]
    fn delete_session_dry_run_does_not_write() {
        let _lock = ENV_LOCK.lock().unwrap();
        let tmp_home = TempDir::new().unwrap();
        let tmp_pi = TempDir::new().unwrap();
        let _guard = setup_env(tmp_home.path(), tmp_pi.path());

        let path = seed_top_level(tmp_pi.path(), "-tmp-proj", UUID_TOP, "/tmp/proj");

        let report = delete_session(UUID_TOP, false, true).unwrap();
        assert!(path.exists());
        assert!(report.paths.iter().any(|p| p == &path));
    }

    #[test]
    fn delete_session_dry_run_with_cascade_predicts_children() {
        let _lock = ENV_LOCK.lock().unwrap();
        let tmp_home = TempDir::new().unwrap();
        let tmp_pi = TempDir::new().unwrap();
        let _guard = setup_env(tmp_home.path(), tmp_pi.path());

        let parent = seed_top_level(tmp_pi.path(), "-tmp-proj", UUID_TOP, "/tmp/proj");
        let child = seed_subagent(
            tmp_pi.path(),
            "-tmp-proj",
            UUID_TOP,
            "entry_1",
            0,
            UUID_CHILD_1,
            "/tmp/proj",
        );

        let report = delete_session(UUID_TOP, true, true).unwrap();
        assert!(parent.exists());
        assert!(child.exists());
        assert_eq!(report.cascaded, 1);
        assert!(report.paths.iter().any(|p| p == &child));
        assert!(report.paths.iter().any(|p| p == &parent));
    }

    #[test]
    fn delete_session_is_noop_on_missing() {
        let _lock = ENV_LOCK.lock().unwrap();
        let tmp_home = TempDir::new().unwrap();
        let tmp_pi = TempDir::new().unwrap();
        let _guard = setup_env(tmp_home.path(), tmp_pi.path());

        let report = delete_session(UUID_TOP, false, false).unwrap();
        assert_eq!(report.count(), 0);
        assert_eq!(report.cascaded, 0);
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

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

/// Report of paths wiped by [`delete_session`].
///
/// Used by the top-level CLI handler to render per-session human /
/// JSON / NUL output.  Empty / missing files are NOT reported as
/// "deleted" — only paths that actually existed before the call
/// appear in `paths`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClaudeDeleteReport {
    /// Absolute paths of files (and possibly the project dir) that
    /// were unlinked.  Order: transcript JSONL → debug log → pruned
    /// project dir (if it became empty).
    pub paths: Vec<PathBuf>,
    /// `true` when the project dir was pruned because the last
    /// session under it was deleted.
    pub pruned_project_dir: bool,
}

impl ClaudeDeleteReport {
    /// Total number of files / dirs unlinked.
    pub fn count(&self) -> usize {
        self.paths.len()
    }
}

/// Wipe every storage location a Claude Code session occupies.
///
/// Order (per `doc/admin.org § Atomicity / partial failure`):
///   1. Remove the session-index cache entry (cheap, in-process).
///   2. Delete the debug log if it exists (optional secondary).
///   3. Delete the transcript JSONL (primary).
///   4. If the parent project directory is now empty, prune it.
///   5. Persist the updated cache.
///
/// Missing files are silently ignored — re-running delete on an
/// already-wiped session is a no-op (returns an empty report).
///
/// `dry_run` mode reports what WOULD be deleted without performing
/// any writes (filesystem inspection only).
pub fn delete_session(session_id: &str, dry_run: bool) -> Result<ClaudeDeleteReport> {
    use crate::session_index::{self as idx, CachedHarnessIndex};

    let mut report = ClaudeDeleteReport::default();

    let jsonl = session::find_session_file(session_id);
    let debug = {
        let p = resolve_debug_file(session_id);
        if p.exists() {
            Some(p)
        } else {
            None
        }
    };

    if dry_run {
        if let Some(path) = &debug {
            report.paths.push(path.clone());
        }
        if let Some(path) = &jsonl {
            report.paths.push(path.clone());
            // Predict project-dir prune: it would happen iff the
            // directory contains exactly one entry (the file we're
            // about to delete).
            if let Some(parent) = path.parent() {
                if would_become_empty(parent, path) {
                    report.paths.push(parent.to_path_buf());
                    report.pruned_project_dir = true;
                }
            }
        }
        return Ok(report);
    }

    // 1. Cache entry — load → mutate → defer save until step 5.
    let mut cache = idx::load_harness_index(session_index::CACHE_FILE)
        .unwrap_or_else(CachedHarnessIndex::empty);
    cache.remove_session(session_id);

    // 2. Debug log.
    if let Some(path) = debug {
        if path.exists() {
            std::fs::remove_file(&path).map_err(|error| {
                anyhow::anyhow!("Failed to remove debug log {}: {}", path.display(), error)
            })?;
            log::debug!(
                "claudecode delete {}: removed {}",
                session_id,
                path.display()
            );
            report.paths.push(path);
        }
    }

    // 3. Transcript JSONL.
    if let Some(path) = jsonl {
        if path.exists() {
            std::fs::remove_file(&path).map_err(|error| {
                anyhow::anyhow!("Failed to remove transcript {}: {}", path.display(), error)
            })?;
            log::debug!(
                "claudecode delete {}: removed {}",
                session_id,
                path.display()
            );
            report.paths.push(path.clone());

            // 4. Prune empty project dir (silently — non-empty is
            // the common case and not a failure).
            if let Some(parent) = path.parent() {
                if std::fs::remove_dir(parent).is_ok() {
                    log::debug!(
                        "claudecode delete {}: pruned empty project dir {}",
                        session_id,
                        parent.display()
                    );
                    report.paths.push(parent.to_path_buf());
                    report.pruned_project_dir = true;
                }
            }
        }
    }

    // 5. Persist cache (always — even if nothing was on disk, the
    // cache entry needed to go).
    let cache_dir = idx::cache_dir().ok_or_else(|| {
        anyhow::anyhow!("Unable to resolve XDG cache dir for session-index cache")
    })?;
    std::fs::create_dir_all(&cache_dir).map_err(|error| {
        anyhow::anyhow!(
            "Failed to create cache dir {}: {}",
            cache_dir.display(),
            error
        )
    })?;
    idx::save_harness_index(session_index::CACHE_FILE, &cache)?;

    Ok(report)
}

/// Predicate: would `parent` become empty if `child` were unlinked?
///
/// Used by `delete_session(_, dry_run=true)` to predict whether the
/// project-dir prune step would fire.  Returns `false` on permission
/// errors / missing dirs (the safe default — predict no prune).
fn would_become_empty(parent: &std::path::Path, child: &std::path::Path) -> bool {
    let entries = match std::fs::read_dir(parent) {
        Ok(it) => it,
        Err(_) => return false,
    };
    let mut others = 0usize;
    for entry in entries.flatten() {
        if entry.path() != child {
            others += 1;
            if others > 0 {
                return false;
            }
        }
    }
    others == 0
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
mod delete_tests {
    use super::*;
    use std::path::Path;
    use std::sync::Mutex;
    use tempfile::TempDir;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvGuard {
        home: Option<String>,
        xdg_cache_home: Option<String>,
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe {
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

    fn with_temp_home(home: &Path) -> EnvGuard {
        let guard = EnvGuard {
            home: std::env::var("HOME").ok(),
            xdg_cache_home: std::env::var("XDG_CACHE_HOME").ok(),
        };
        unsafe {
            std::env::set_var("HOME", home);
            std::env::set_var("XDG_CACHE_HOME", home.join(".cache"));
        }
        guard
    }

    /// Create `~/.claude/projects/<encoded>/<session>.jsonl` and an
    /// optional `~/.claude/debug/<session>.txt` under the given fake
    /// home directory.  Returns (jsonl_path, optional debug_path).
    fn seed_session(
        home: &Path,
        encoded_proj: &str,
        session_id: &str,
        with_debug: bool,
    ) -> (std::path::PathBuf, Option<std::path::PathBuf>) {
        let project_dir = home.join(".claude").join("projects").join(encoded_proj);
        std::fs::create_dir_all(&project_dir).unwrap();
        let jsonl = project_dir.join(format!("{}.jsonl", session_id));
        std::fs::write(&jsonl, r#"{"cwd":"/proj","type":"user"}"#).unwrap();

        let debug = if with_debug {
            let debug_dir = home.join(".claude").join("debug");
            std::fs::create_dir_all(&debug_dir).unwrap();
            let path = debug_dir.join(format!("{}.txt", session_id));
            std::fs::write(&path, "debug log content").unwrap();
            Some(path)
        } else {
            None
        };

        (jsonl, debug)
    }

    #[test]
    fn delete_session_removes_jsonl_and_debug_and_prunes_empty_project_dir() {
        let _lock = ENV_LOCK.lock().unwrap();
        let tmp = TempDir::new().unwrap();
        let _guard = with_temp_home(tmp.path());
        let (jsonl, debug) = seed_session(tmp.path(), "-proj-a", "ses_aaa", true);
        let project_dir = jsonl.parent().unwrap().to_path_buf();

        let report = delete_session("ses_aaa", false).unwrap();

        assert!(!jsonl.exists(), "JSONL should be deleted");
        assert!(
            !debug.as_ref().unwrap().exists(),
            "debug log should be deleted"
        );
        assert!(
            !project_dir.exists(),
            "empty project dir should have been pruned"
        );
        assert!(report.pruned_project_dir);
        assert_eq!(report.count(), 3); // debug + jsonl + project dir
    }

    #[test]
    fn delete_session_leaves_non_empty_project_dir_alone() {
        let _lock = ENV_LOCK.lock().unwrap();
        let tmp = TempDir::new().unwrap();
        let _guard = with_temp_home(tmp.path());
        let (target_jsonl, _) = seed_session(tmp.path(), "-proj-a", "ses_target", false);
        // Sibling session in the same project dir — keeps the dir non-empty.
        seed_session(tmp.path(), "-proj-a", "ses_sibling", false);

        let report = delete_session("ses_target", false).unwrap();

        assert!(!target_jsonl.exists());
        assert!(target_jsonl.parent().unwrap().exists(), "project dir kept");
        assert!(!report.pruned_project_dir);
        assert_eq!(report.count(), 1);
    }

    #[test]
    fn delete_session_works_when_debug_log_missing() {
        let _lock = ENV_LOCK.lock().unwrap();
        let tmp = TempDir::new().unwrap();
        let _guard = with_temp_home(tmp.path());
        let (jsonl, _) = seed_session(tmp.path(), "-proj-a", "ses_nodbg", false);

        let report = delete_session("ses_nodbg", false).unwrap();

        assert!(!jsonl.exists());
        // Only the jsonl + pruned dir = 2 entries (no debug log).
        assert_eq!(report.count(), 2);
    }

    #[test]
    fn delete_session_is_noop_on_missing_session() {
        let _lock = ENV_LOCK.lock().unwrap();
        let tmp = TempDir::new().unwrap();
        let _guard = with_temp_home(tmp.path());

        // No seeded session — directories don't even exist.
        let report = delete_session("ses_ghost", false).unwrap();
        assert_eq!(report.count(), 0);
        assert!(!report.pruned_project_dir);
    }

    #[test]
    fn delete_session_dry_run_does_not_write() {
        let _lock = ENV_LOCK.lock().unwrap();
        let tmp = TempDir::new().unwrap();
        let _guard = with_temp_home(tmp.path());
        let (jsonl, debug) = seed_session(tmp.path(), "-proj-a", "ses_aaa", true);

        let report = delete_session("ses_aaa", true).unwrap();

        // Files still present.
        assert!(jsonl.exists());
        assert!(debug.as_ref().unwrap().exists());
        // But the report enumerates what WOULD have been deleted.
        assert!(report.pruned_project_dir);
        assert!(report.paths.iter().any(|p| p == &jsonl));
        assert!(report.paths.iter().any(|p| p == debug.as_ref().unwrap()));
    }
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

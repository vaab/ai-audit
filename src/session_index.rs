//! Cross-harness session metadata index with on-disk cache.
//!
//! Every `activity get` invocation needs to know which sessions exist
//! per harness/project.  Without caching, that costs ~3.5 s of file
//! reads on a moderate corpus (15 k+ Claude sessions).  This module
//! provides shared types and helpers; each harness owns its own
//! cache file and incremental-update logic in
//! ``src/<harness>/session_index.rs``.
//!
//! Cache layout (on disk):
//!
//! ```text
//! ~/.cache/ai-audit/session-index/
//!   meta.json          # { schema_version, last_run_at }
//!   claudecode.json    # CachedHarnessIndex
//!   opencode.json      # CachedHarnessIndex
//!   pi.json            # CachedHarnessIndex
//! ```
//!
//! `last_run_at` is the wall-clock seconds-since-epoch captured at
//! the start of the latest successful update.  On the next run, only
//! sessions whose underlying file/db row was created or modified
//! after that timestamp are re-parsed; everything else is reused
//! verbatim from cache.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

/// On-disk schema version.  Bump when the persisted shape changes.
pub const SCHEMA_VERSION: u32 = 1;

/// One persisted session record.
///
/// `cwd_raw` is the **unsimplified** cwd string read from the
/// session's source-of-truth (JSONL header for Claude/Pi, DB column
/// for OpenCode).  Simplification (via `Config::simplify_path`) is
/// applied at load time so the cache survives changes to the user's
/// path-simplification rules.
///
/// `path` is the on-disk session file path for file-based harnesses
/// (Claude, Pi).  OpenCode persists session bodies through its DB and
/// per-message JSON files, so the top-level "session file" concept
/// doesn't apply — `path` is `None`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedSession {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub path: Option<PathBuf>,
    pub cwd_raw: String,
    pub is_child: bool,
}

/// Cross-run metadata stored at `meta.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheMeta {
    pub schema_version: u32,
    pub last_run_at: i64,
}

/// On-disk shape for a single harness.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CachedHarnessIndex {
    pub schema_version: u32,
    pub sessions: Vec<CachedSession>,
}

impl CachedHarnessIndex {
    pub fn empty() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            sessions: Vec::new(),
        }
    }
}

/// Root directory for the session-index cache.
pub fn cache_dir() -> Option<PathBuf> {
    Some(dirs::cache_dir()?.join("ai-audit").join("session-index"))
}

/// Path to the cross-harness metadata file.
pub fn meta_path() -> Option<PathBuf> {
    Some(cache_dir()?.join("meta.json"))
}

/// Read `meta.json`; treat any error / schema mismatch as "no cache".
pub fn load_meta() -> Option<CacheMeta> {
    let path = meta_path()?;
    let content = fs::read_to_string(path).ok()?;
    let meta: CacheMeta = serde_json::from_str(&content).ok()?;
    if meta.schema_version != SCHEMA_VERSION {
        return None;
    }
    Some(meta)
}

/// Atomically write `meta.json` with the given `last_run_at`.
pub fn save_meta(last_run_at: i64) -> Result<()> {
    let dir = cache_dir().context("cache directory unavailable")?;
    fs::create_dir_all(&dir).with_context(|| format!("Failed to create {}", dir.display()))?;
    let payload = CacheMeta {
        schema_version: SCHEMA_VERSION,
        last_run_at,
    };
    write_json_atomic(&dir.join("meta.json"), &payload)
}

/// Load a per-harness cache file.  Returns `None` on any failure
/// (missing, corrupt, schema mismatch) — callers fall back to a cold
/// rebuild.
pub fn load_harness_index(file_name: &str) -> Option<CachedHarnessIndex> {
    let dir = cache_dir()?;
    let path = dir.join(file_name);
    let content = fs::read_to_string(&path).ok()?;
    let idx: CachedHarnessIndex = serde_json::from_str(&content).ok()?;
    if idx.schema_version != SCHEMA_VERSION {
        return None;
    }
    Some(idx)
}

/// Atomically save a per-harness cache file.
pub fn save_harness_index(file_name: &str, index: &CachedHarnessIndex) -> Result<()> {
    let dir = cache_dir().context("cache directory unavailable")?;
    fs::create_dir_all(&dir).with_context(|| format!("Failed to create {}", dir.display()))?;
    write_json_atomic(&dir.join(file_name), index)
}

/// Tempfile-then-rename atomic write.
fn write_json_atomic<T: Serialize>(target: &Path, payload: &T) -> Result<()> {
    let parent = target
        .parent()
        .context("target has no parent directory")?
        .to_path_buf();
    let stem = target
        .file_name()
        .context("target has no file name")?
        .to_string_lossy()
        .to_string();
    let tmp = parent.join(format!(".{}.tmp-{}", stem, std::process::id()));
    let json = serde_json::to_string(payload)?;
    fs::write(&tmp, json).with_context(|| format!("Failed to write {}", tmp.display()))?;
    fs::rename(&tmp, target)
        .with_context(|| format!("Failed to rename {} to {}", tmp.display(), target.display()))?;
    Ok(())
}

/// Group cached sessions by their **simplified** project_dir using
/// the supplied closure.  This is what callers (per-harness modules)
/// expose to consumers — the cache stores raw cwds; lookup happens by
/// simplified form.
pub fn group_by_simplified<F>(
    sessions: &[CachedSession],
    simplify: F,
) -> HashMap<String, Vec<CachedSession>>
where
    F: Fn(&str) -> String,
{
    let mut out: HashMap<String, Vec<CachedSession>> = HashMap::new();
    for s in sessions {
        let key = simplify(&s.cwd_raw);
        out.entry(key).or_default().push(s.clone());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(id: &str, cwd: &str, is_child: bool) -> CachedSession {
        CachedSession {
            id: id.to_string(),
            path: None,
            cwd_raw: cwd.to_string(),
            is_child,
        }
    }

    #[test]
    fn group_by_simplified_groups_identical_keys() {
        let sessions = vec![
            s("a", "/foo", false),
            s("b", "/foo", false),
            s("c", "/bar", false),
        ];
        let grouped = group_by_simplified(&sessions, |raw| raw.to_string());
        assert_eq!(grouped.get("/foo").map(Vec::len), Some(2));
        assert_eq!(grouped.get("/bar").map(Vec::len), Some(1));
    }

    #[test]
    fn group_by_simplified_applies_simplification() {
        let sessions = vec![s("a", "/dev/charm", false), s("b", "/dev/charm", false)];
        let grouped = group_by_simplified(&sessions, |raw| {
            // toy simplifier: replace "/dev/" with "DEV>"
            raw.replace("/dev/", "DEV>")
        });
        assert!(grouped.contains_key("DEV>charm"));
        assert!(!grouped.contains_key("/dev/charm"));
    }

    #[test]
    fn group_by_simplified_handles_empty() {
        let sessions: Vec<CachedSession> = Vec::new();
        let grouped = group_by_simplified(&sessions, |s| s.to_string());
        assert!(grouped.is_empty());
    }

    #[test]
    fn cached_session_round_trips_through_serde() {
        let s = CachedSession {
            id: "abc".into(),
            path: Some(PathBuf::from("/tmp/x.jsonl")),
            cwd_raw: "/home/user".into(),
            is_child: true,
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: CachedSession = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn cached_session_omits_path_when_none() {
        let s = CachedSession {
            id: "abc".into(),
            path: None,
            cwd_raw: "/home/user".into(),
            is_child: false,
        };
        let json = serde_json::to_string(&s).unwrap();
        // path: None should not appear in the serialized output
        assert!(!json.contains("path"), "got: {}", json);
    }

    #[test]
    fn cached_harness_index_default_is_empty() {
        let idx = CachedHarnessIndex::empty();
        assert_eq!(idx.schema_version, SCHEMA_VERSION);
        assert!(idx.sessions.is_empty());
    }

    #[test]
    fn cached_harness_index_round_trips() {
        let idx = CachedHarnessIndex {
            schema_version: SCHEMA_VERSION,
            sessions: vec![s("a", "/foo", false), s("b", "/bar", true)],
        };
        let json = serde_json::to_string(&idx).unwrap();
        let back: CachedHarnessIndex = serde_json::from_str(&json).unwrap();
        assert_eq!(back.sessions.len(), 2);
        assert_eq!(back.sessions[0].id, "a");
        assert!(back.sessions[1].is_child);
    }
}

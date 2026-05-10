//! Cross-harness session metadata index with on-disk cache (v2).
//!
//! Cache layout (on disk):
//!
//! ```text
//! ~/.cache/ai-audit/session-index/
//!   claudecode.json    # CachedHarnessIndex
//!   opencode.json      # CachedHarnessIndex (file-storage portion only)
//!   pi.json            # CachedHarnessIndex
//! ```
//!
//! Each per-harness file holds a `CachedHarnessIndex` containing:
//!
//! - `sessions_by_id`: per-session metadata, including the set of
//!   distinct cwds the session has ever recorded.
//! - `by_path`: raw-cwd → list of session_ids that touched that cwd.
//!
//! `by_path` is a denormalised view of `sessions_by_id` — it can be
//! rebuilt from the latter at any time.  We persist both for
//! disk → ready-to-query load with no re-grouping at startup.
//!
//! ## Update model
//!
//! Each session entry carries `mtime_ns` (file mtime at last full
//! scan).  On warm runs:
//!
//! - If file `mtime_ns` is unchanged, the session is skipped.
//! - If `mtime_ns` changed, the session file is re-read **fully**
//!   and the session's `cwds` set is **rebuilt from scratch**
//!   (replacing the previous set).  We do not trust "events past
//!   timestamp T are strictly new" because some harnesses may
//!   rewrite or drop entries; full re-read is the only safe option
//!   when a file changes.
//!
//! For harnesses where cwd is recorded only at the session-header
//! level (Pi), the per-session re-read still happens on mtime change
//! but yields a single-element `cwds` set.
//!
//! OpenCode's DB-backed sessions bypass this cache entirely and are
//! queried directly via SQL — the DB row is the source of truth and
//! `WHERE directory = ?` is faster than any file-based shortcut.
//! Only OpenCode's file-storage portion participates in the cache.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

/// On-disk schema version.  Bump when the persisted shape changes.
pub const SCHEMA_VERSION: u32 = 2;

/// Per-session record.
///
/// `cwds` is the set of every distinct cwd this session has touched
/// across its lifetime, recorded raw (un-simplified).  Simplification
/// is applied at lookup time so the cache survives changes to the
/// user's path-simplification rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedSession {
    /// Stable session identifier (UUID for Claude, ``ses_*`` for
    /// OpenCode, UUIDv7 for Pi).
    pub id: String,
    /// On-disk session file path; `None` for OpenCode (no single
    /// file backs a session).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub path: Option<PathBuf>,
    pub is_child: bool,
    /// File modification time at the last successful scan, expressed
    /// as nanoseconds since the Unix epoch.  Compared against
    /// `fs::metadata().modified()` to gate full re-scans.
    pub mtime_ns: u64,
    /// Distinct cwds touched by this session, raw (un-simplified).
    /// Sorted for deterministic JSON output.
    pub cwds: Vec<String>,
}

impl CachedSession {
    /// Insert a new cwd into `cwds`, preserving sorted order, no
    /// duplicates.  Returns `true` if the cwd was new.
    pub fn add_cwd(&mut self, cwd: String) -> bool {
        match self.cwds.binary_search(&cwd) {
            Ok(_) => false,
            Err(pos) => {
                self.cwds.insert(pos, cwd);
                true
            }
        }
    }
}

/// On-disk shape for a single harness.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CachedHarnessIndex {
    pub schema_version: u32,
    /// Per-session metadata, keyed by session id.
    pub sessions_by_id: HashMap<String, CachedSession>,
    /// Reverse index: raw cwd → list of session ids that touched it.
    /// Persisted to skip re-grouping on load.
    pub by_path: HashMap<String, Vec<String>>,
}

impl CachedHarnessIndex {
    pub fn empty() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            sessions_by_id: HashMap::new(),
            by_path: HashMap::new(),
        }
    }

    /// Rebuild `by_path` from `sessions_by_id`.  Call after mutating
    /// any session's `cwds` set, before persisting.
    pub fn rebuild_by_path(&mut self) {
        let mut by_path: HashMap<String, Vec<String>> = HashMap::new();
        for (id, session) in &self.sessions_by_id {
            for cwd in &session.cwds {
                by_path.entry(cwd.clone()).or_default().push(id.clone());
            }
        }
        for ids in by_path.values_mut() {
            ids.sort();
            ids.dedup();
        }
        self.by_path = by_path;
    }

    /// Drop a session from the index (both maps).
    pub fn remove_session(&mut self, session_id: &str) {
        if let Some(session) = self.sessions_by_id.remove(session_id) {
            for cwd in &session.cwds {
                if let Some(ids) = self.by_path.get_mut(cwd) {
                    ids.retain(|id| id != session_id);
                    if ids.is_empty() {
                        self.by_path.remove(cwd);
                    }
                }
            }
        }
    }

    /// Look up sessions whose simplified cwd matches `categ_id`.
    /// Iterates the `by_path` keys, applying `simplify` to each.
    /// Returns the union of all session entries in matching buckets,
    /// deduplicated by id.
    pub fn lookup_by_categ_id<F>(&self, categ_id: &str, simplify: F) -> Vec<&CachedSession>
    where
        F: Fn(&str) -> String,
    {
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for (raw_cwd, ids) in &self.by_path {
            if simplify(raw_cwd) != categ_id {
                continue;
            }
            for id in ids {
                if !seen.insert(id.as_str()) {
                    continue;
                }
                if let Some(s) = self.sessions_by_id.get(id) {
                    out.push(s);
                }
            }
        }
        out
    }
}

/// Root directory for the session-index cache.
pub fn cache_dir() -> Option<PathBuf> {
    Some(dirs::cache_dir()?.join("ai-audit").join("session-index"))
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

/// Atomically save a per-harness cache file.  Caller is responsible
/// for invoking `rebuild_by_path` before save when `sessions_by_id`
/// changed.
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

/// Convert a `Metadata` to nanoseconds since the Unix epoch.
/// Returns 0 on error.
pub fn mtime_ns_of(metadata: &fs::Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| {
            d.as_secs()
                .saturating_mul(1_000_000_000)
                .saturating_add(u64::from(d.subsec_nanos()))
        })
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(id: &str, cwds: &[&str], is_child: bool) -> CachedSession {
        CachedSession {
            id: id.to_string(),
            path: None,
            is_child,
            mtime_ns: 0,
            cwds: cwds.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn schema_version_is_two() {
        assert_eq!(SCHEMA_VERSION, 2);
    }

    #[test]
    fn add_cwd_inserts_sorted_no_duplicates() {
        let mut session = s("a", &[], false);
        assert!(session.add_cwd("/b".to_string()));
        assert!(session.add_cwd("/a".to_string()));
        assert!(!session.add_cwd("/a".to_string()));
        assert!(session.add_cwd("/c".to_string()));
        assert_eq!(session.cwds, vec!["/a", "/b", "/c"]);
    }

    #[test]
    fn rebuild_by_path_groups_sessions_by_each_cwd_they_touched() {
        let mut idx = CachedHarnessIndex::empty();
        idx.sessions_by_id
            .insert("ses_1".to_string(), s("ses_1", &["/a", "/b"], false));
        idx.sessions_by_id
            .insert("ses_2".to_string(), s("ses_2", &["/a"], false));
        idx.rebuild_by_path();
        let mut a_ids = idx.by_path.get("/a").cloned().unwrap_or_default();
        a_ids.sort();
        assert_eq!(a_ids, vec!["ses_1", "ses_2"]);
        assert_eq!(idx.by_path.get("/b"), Some(&vec!["ses_1".to_string()]));
        assert!(!idx.by_path.contains_key("/missing"));
    }

    #[test]
    fn rebuild_by_path_handles_empty() {
        let mut idx = CachedHarnessIndex::empty();
        idx.rebuild_by_path();
        assert!(idx.by_path.is_empty());
    }

    #[test]
    fn rebuild_by_path_dedups_within_bucket() {
        let mut idx = CachedHarnessIndex::empty();
        let mut session = s("ses_1", &["/a"], false);
        session.cwds.push("/a".to_string()); // simulated corruption
        idx.sessions_by_id.insert("ses_1".to_string(), session);
        idx.rebuild_by_path();
        assert_eq!(idx.by_path.get("/a"), Some(&vec!["ses_1".to_string()]));
    }

    #[test]
    fn remove_session_clears_both_maps() {
        let mut idx = CachedHarnessIndex::empty();
        idx.sessions_by_id
            .insert("ses_1".to_string(), s("ses_1", &["/a", "/b"], false));
        idx.sessions_by_id
            .insert("ses_2".to_string(), s("ses_2", &["/a"], false));
        idx.rebuild_by_path();
        idx.remove_session("ses_1");
        assert!(!idx.sessions_by_id.contains_key("ses_1"));
        assert_eq!(idx.by_path.get("/a"), Some(&vec!["ses_2".to_string()]));
        assert!(!idx.by_path.contains_key("/b"));
    }

    #[test]
    fn remove_session_is_noop_when_id_unknown() {
        let mut idx = CachedHarnessIndex::empty();
        idx.sessions_by_id
            .insert("ses_1".to_string(), s("ses_1", &["/a"], false));
        idx.rebuild_by_path();
        idx.remove_session("missing");
        assert_eq!(idx.sessions_by_id.len(), 1);
    }

    #[test]
    fn lookup_by_categ_id_matches_via_simplify_closure() {
        let mut idx = CachedHarnessIndex::empty();
        idx.sessions_by_id
            .insert("ses_1".to_string(), s("ses_1", &["/dev/charm"], false));
        idx.sessions_by_id
            .insert("ses_2".to_string(), s("ses_2", &["/dev/charm"], false));
        idx.sessions_by_id
            .insert("ses_3".to_string(), s("ses_3", &["/home/vaab"], false));
        idx.rebuild_by_path();

        let simplify = |raw: &str| raw.replace("/dev/", "DEV>");
        let mut found: Vec<&str> = idx
            .lookup_by_categ_id("DEV>charm", simplify)
            .iter()
            .map(|s| s.id.as_str())
            .collect();
        found.sort();
        assert_eq!(found, vec!["ses_1", "ses_2"]);
    }

    #[test]
    fn lookup_by_categ_id_dedups_when_session_in_multiple_buckets() {
        let mut idx = CachedHarnessIndex::empty();
        idx.sessions_by_id.insert(
            "ses_1".to_string(),
            s("ses_1", &["/path/A", "/other/A"], false),
        );
        idx.rebuild_by_path();

        let simplify = |raw: &str| {
            raw.rsplit_once('/')
                .map(|(_, last)| last.to_string())
                .unwrap_or_default()
        };
        let found = idx.lookup_by_categ_id("A", simplify);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, "ses_1");
    }

    #[test]
    fn lookup_by_categ_id_returns_empty_on_no_match() {
        let mut idx = CachedHarnessIndex::empty();
        idx.sessions_by_id
            .insert("ses_1".to_string(), s("ses_1", &["/a"], false));
        idx.rebuild_by_path();
        let simplify = |raw: &str| raw.to_string();
        assert!(idx.lookup_by_categ_id("/missing", simplify).is_empty());
    }

    #[test]
    fn cached_session_round_trips_through_serde() {
        let session = CachedSession {
            id: "abc".into(),
            path: Some(PathBuf::from("/tmp/x.jsonl")),
            is_child: true,
            mtime_ns: 12345,
            cwds: vec!["/a".into(), "/b".into()],
        };
        let json = serde_json::to_string(&session).unwrap();
        let back: CachedSession = serde_json::from_str(&json).unwrap();
        assert_eq!(session, back);
    }

    #[test]
    fn cached_session_omits_path_when_none() {
        let session = CachedSession {
            id: "abc".into(),
            path: None,
            is_child: false,
            mtime_ns: 0,
            cwds: vec![],
        };
        let json = serde_json::to_string(&session).unwrap();
        assert!(!json.contains("\"path\""), "got: {}", json);
    }

    #[test]
    fn cached_harness_index_round_trips_with_both_maps() {
        let mut idx = CachedHarnessIndex::empty();
        idx.sessions_by_id
            .insert("a".to_string(), s("a", &["/x", "/y"], false));
        idx.sessions_by_id
            .insert("b".to_string(), s("b", &["/x"], true));
        idx.rebuild_by_path();
        let json = serde_json::to_string(&idx).unwrap();
        let back: CachedHarnessIndex = serde_json::from_str(&json).unwrap();
        assert_eq!(back.schema_version, SCHEMA_VERSION);
        assert_eq!(back.sessions_by_id.len(), 2);
        assert_eq!(back.by_path.len(), 2);
        let mut x_ids = back.by_path.get("/x").cloned().unwrap_or_default();
        x_ids.sort();
        assert_eq!(x_ids, vec!["a", "b"]);
    }

    #[test]
    fn cached_harness_index_default_has_correct_schema() {
        let idx = CachedHarnessIndex::empty();
        assert_eq!(idx.schema_version, SCHEMA_VERSION);
        assert!(idx.sessions_by_id.is_empty());
        assert!(idx.by_path.is_empty());
    }

    #[test]
    fn mtime_ns_of_returns_nonzero_for_real_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        fs::write(&path, b"x").unwrap();
        let meta = fs::metadata(&path).unwrap();
        assert!(mtime_ns_of(&meta) > 0);
    }
}

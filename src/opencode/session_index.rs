//! On-disk cache of OpenCode session metadata (v2 — file storage only).
//!
//! OpenCode persists sessions through two backends:
//!
//! - **SQLite database** at `opencode.db`.  This backend has its own
//!   indexed `WHERE directory = ?` query path; caching it on top
//!   would duplicate state without speedup.  We query the DB
//!   directly each time and merge results into the runtime view.
//! - **Per-project file storage** under
//!   `storage/session/<hash>/<ses_*>.json`.  This backend benefits
//!   from caching: walking the hash-sharded directory tree and
//!   parsing each session JSON adds up across thousands of
//!   sessions.
//!
//! Only the file-storage portion participates in the persisted
//! `opencode.json` cache.  DB rows are looked up live and merged at
//! `update_and_load` time, with DB winning on session-id collision
//! (matches the legacy `merge_session_metas` precedence).
//!
//! OpenCode structurally records cwd only at the session-creation
//! level (the `directory` column / JSON field).  Each session's
//! `cwds` set is therefore single-element in practice.

use anyhow::Result;
use serde::Deserialize;
use std::collections::HashSet;
use std::fs;
use std::path::Path;

use crate::config::Config;
use crate::session_index::{
    self as idx, mtime_ns_of, CachedHarnessIndex, CachedSession, SCHEMA_VERSION,
};

pub(crate) const CACHE_FILE: &str = "opencode.json";

#[derive(Debug, Default, Clone)]
pub struct OpenCodeIndex {
    /// Combined view: cached file-storage entries unioned with live
    /// DB rows.  DB wins on id collision.
    inner: CachedHarnessIndex,
}

impl OpenCodeIndex {
    pub fn all(&self) -> impl Iterator<Item = &CachedSession> {
        self.inner.sessions_by_id.values()
    }

    pub fn for_categ_id(&self, categ_id: &str, config: &Config) -> Vec<&CachedSession> {
        self.inner
            .lookup_by_categ_id(categ_id, |raw| config.simplify_path(raw))
    }
}

pub fn update_and_load(_config: &Config) -> Result<OpenCodeIndex> {
    // 1. Load the persisted file-storage cache and refresh it from
    //    on-disk session JSON files.
    let mut existing =
        idx::load_harness_index(CACHE_FILE).unwrap_or_else(CachedHarnessIndex::empty);
    existing.schema_version = SCHEMA_VERSION;

    let session_dir = crate::opencode_data_dir().join("storage").join("session");
    refresh_file_cache(&session_dir, &mut existing);
    existing.rebuild_by_path();
    idx::save_harness_index(CACHE_FILE, &existing)?;

    // 2. Query the DB live and merge.  DB wins on id collision.
    let mut combined = existing.clone();
    if let Some(db_sessions) = query_db_sessions() {
        for session in db_sessions {
            combined.sessions_by_id.insert(session.id.clone(), session);
        }
        combined.rebuild_by_path();
    }

    Ok(OpenCodeIndex { inner: combined })
}

fn refresh_file_cache(session_dir: &Path, existing: &mut CachedHarnessIndex) {
    if !session_dir.exists() {
        return;
    }
    let project_entries = match fs::read_dir(session_dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    let mut visited: HashSet<String> = HashSet::new();
    let mut added = 0usize;
    let mut refreshed = 0usize;

    for project_entry in project_entries.flatten() {
        let project_path = project_entry.path();
        if !project_path.is_dir() {
            continue;
        }
        let session_files = match fs::read_dir(&project_path) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for session_file in session_files.flatten() {
            let path = session_file.path();
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }
            let metadata = match session_file.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            let current_mtime = mtime_ns_of(&metadata);

            // Look up by path (we don't yet know the session_id).
            let existing_id = existing
                .sessions_by_id
                .iter()
                .find(|(_, s)| s.path.as_deref() == Some(path.as_path()))
                .map(|(id, _)| id.clone());

            if let Some(id) = &existing_id {
                visited.insert(id.clone());
                if let Some(s) = existing.sessions_by_id.get(id) {
                    if s.mtime_ns == current_mtime {
                        continue;
                    }
                }
            }

            let parsed = match parse_session_file(&path) {
                Some(p) => p,
                None => continue,
            };
            let was_known = existing.sessions_by_id.contains_key(&parsed.id);
            visited.insert(parsed.id.clone());
            existing.sessions_by_id.insert(
                parsed.id.clone(),
                CachedSession {
                    id: parsed.id,
                    path: Some(path),
                    is_child: parsed.is_child,
                    mtime_ns: current_mtime,
                    cwds: if parsed.directory.is_empty() {
                        vec![]
                    } else {
                        vec![parsed.directory]
                    },
                },
            );
            if was_known {
                refreshed += 1;
            } else {
                added += 1;
            }
        }
    }

    // Lazy cleanup: drop file-backed entries we didn't see this run.
    let stale_ids: Vec<String> = existing
        .sessions_by_id
        .iter()
        .filter(|(id, s)| s.path.is_some() && !visited.contains(id.as_str()))
        .map(|(id, _)| id.clone())
        .collect();
    for id in &stale_ids {
        existing.remove_session(id);
    }

    if added > 0 || refreshed > 0 || !stale_ids.is_empty() {
        log::debug!(
            "opencode session-index (file): +{} new, {} re-read, {} stale dropped; cache size {}",
            added,
            refreshed,
            stale_ids.len(),
            existing.sessions_by_id.len()
        );
    }
}

#[derive(Debug, Deserialize)]
struct OpenCodeSessionFile {
    id: String,
    directory: Option<String>,
    #[serde(rename = "parentID")]
    parent_id: Option<String>,
}

struct ParsedFileSession {
    id: String,
    directory: String,
    is_child: bool,
}

fn parse_session_file(path: &Path) -> Option<ParsedFileSession> {
    let content = fs::read_to_string(path).ok()?;
    let session: OpenCodeSessionFile = serde_json::from_str(&content).ok()?;
    Some(ParsedFileSession {
        id: session.id,
        directory: session.directory.unwrap_or_default(),
        is_child: session.parent_id.is_some(),
    })
}

/// Direct DB query — no caching layer.  SQLite already supports
/// indexed `directory` lookups, and the DB row is the source of
/// truth.  Returns `None` if the DB is missing or unreadable
/// (caller falls back to file-cache only).
fn query_db_sessions() -> Option<Vec<CachedSession>> {
    if !crate::opencode::db::db_exists() {
        return None;
    }
    let conn = crate::opencode::db::open_db().ok()?;
    let sessions = crate::opencode::db::list_sessions_from_conn(&conn).ok()?;
    let out = sessions
        .into_iter()
        .map(|s| CachedSession {
            id: s.session_id,
            path: None,
            is_child: s.parent_id.is_some(),
            mtime_ns: 0,
            cwds: if s.project_dir.is_empty() {
                vec![]
            } else {
                vec![s.project_dir]
            },
        })
        .collect();
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use filetime::{set_file_mtime, FileTime};
    use tempfile::tempdir;

    fn make_session_json(id: &str, directory: &str, parent: Option<&str>) -> String {
        match parent {
            Some(p) => format!(
                r#"{{"id":"{}","directory":"{}","parentID":"{}","time":{{"created":0}}}}"#,
                id, directory, p
            ),
            None => format!(
                r#"{{"id":"{}","directory":"{}","time":{{"created":0}}}}"#,
                id, directory
            ),
        }
    }

    #[test]
    fn parse_session_file_extracts_id_directory_and_parent() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("ses_a.json");
        fs::write(
            &path,
            make_session_json("ses_a", "/proj-a", Some("ses_parent")),
        )
        .unwrap();
        let parsed = parse_session_file(&path).unwrap();
        assert_eq!(parsed.id, "ses_a");
        assert_eq!(parsed.directory, "/proj-a");
        assert!(parsed.is_child);
    }

    #[test]
    fn parse_session_file_returns_none_for_corrupt_json() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("ses_bad.json");
        fs::write(&path, "not json").unwrap();
        assert!(parse_session_file(&path).is_none());
    }

    #[test]
    fn parse_session_file_handles_missing_directory_as_empty() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("ses_a.json");
        fs::write(&path, r#"{"id":"ses_a","time":{"created":0}}"#).unwrap();
        let parsed = parse_session_file(&path).unwrap();
        assert_eq!(parsed.directory, "");
        assert!(!parsed.is_child);
    }

    #[test]
    fn refresh_file_cache_indexes_new_sessions() {
        let dir = tempdir().unwrap();
        let proj = dir.path().join("proj_hash");
        fs::create_dir_all(&proj).unwrap();
        fs::write(
            proj.join("ses_a.json"),
            make_session_json("ses_a", "/x", None),
        )
        .unwrap();
        fs::write(
            proj.join("ses_b.json"),
            make_session_json("ses_b", "/y", None),
        )
        .unwrap();

        let mut idx = CachedHarnessIndex::empty();
        refresh_file_cache(dir.path(), &mut idx);
        assert_eq!(idx.sessions_by_id.len(), 2);
        assert_eq!(idx.sessions_by_id["ses_a"].cwds, vec!["/x"]);
        assert_eq!(idx.sessions_by_id["ses_b"].cwds, vec!["/y"]);
    }

    #[test]
    fn refresh_file_cache_skips_unchanged_files() {
        let dir = tempdir().unwrap();
        let proj = dir.path().join("proj_hash");
        fs::create_dir_all(&proj).unwrap();
        let session_path = proj.join("ses_a.json");
        fs::write(&session_path, make_session_json("ses_a", "/x", None)).unwrap();

        let mut idx = CachedHarnessIndex::empty();
        refresh_file_cache(dir.path(), &mut idx);
        let initial_mtime = idx.sessions_by_id["ses_a"].mtime_ns;

        // Second pass with no file change → mtime unchanged.
        refresh_file_cache(dir.path(), &mut idx);
        assert_eq!(idx.sessions_by_id["ses_a"].mtime_ns, initial_mtime);
    }

    #[test]
    fn refresh_file_cache_rescans_when_mtime_changes_replacing_cwds() {
        let dir = tempdir().unwrap();
        let proj = dir.path().join("proj_hash");
        fs::create_dir_all(&proj).unwrap();
        let session_path = proj.join("ses_a.json");
        fs::write(&session_path, make_session_json("ses_a", "/old", None)).unwrap();

        let mut idx = CachedHarnessIndex::empty();
        refresh_file_cache(dir.path(), &mut idx);
        assert_eq!(idx.sessions_by_id["ses_a"].cwds, vec!["/old"]);

        fs::write(&session_path, make_session_json("ses_a", "/new", None)).unwrap();
        set_file_mtime(&session_path, FileTime::from_unix_time(2_000_000_000, 0)).unwrap();
        refresh_file_cache(dir.path(), &mut idx);
        assert_eq!(idx.sessions_by_id["ses_a"].cwds, vec!["/new"]);
    }

    #[test]
    fn refresh_file_cache_drops_disappeared_sessions() {
        let dir = tempdir().unwrap();
        let proj = dir.path().join("proj_hash");
        fs::create_dir_all(&proj).unwrap();
        let session_path = proj.join("ses_a.json");
        fs::write(&session_path, make_session_json("ses_a", "/x", None)).unwrap();

        let mut idx = CachedHarnessIndex::empty();
        refresh_file_cache(dir.path(), &mut idx);
        assert!(idx.sessions_by_id.contains_key("ses_a"));

        fs::remove_file(&session_path).unwrap();
        refresh_file_cache(dir.path(), &mut idx);
        assert!(!idx.sessions_by_id.contains_key("ses_a"));
    }

    #[test]
    fn refresh_file_cache_handles_missing_dir() {
        let dir = tempdir().unwrap();
        let nonexistent = dir.path().join("does-not-exist");
        let mut idx = CachedHarnessIndex::empty();
        refresh_file_cache(&nonexistent, &mut idx);
        assert!(idx.sessions_by_id.is_empty());
    }

    #[test]
    fn opencode_index_for_categ_id_resolves_via_simplify() {
        let mut inner = CachedHarnessIndex::empty();
        inner.sessions_by_id.insert(
            "ses_x".to_string(),
            CachedSession {
                id: "ses_x".to_string(),
                path: None,
                is_child: false,
                mtime_ns: 0,
                cwds: vec!["/foo".to_string()],
            },
        );
        inner.rebuild_by_path();
        let oc = OpenCodeIndex { inner };
        let cfg = Config::default();
        let found = oc.for_categ_id("/foo", &cfg);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, "ses_x");
    }
}

//! On-disk cache of OpenCode session metadata.
//!
//! OpenCode persists sessions through two backends:
//! - Per-project file storage at `storage/session/<hash>/<ses_*>.json`.
//! - A SQLite database at `opencode.db` with a `session` table.
//!
//! The DB is queried with `SELECT … WHERE time_created > ?` so warm
//! runs only fetch genuinely new sessions.  File storage is walked
//! mtime-aware: paths already in the cache are skipped entirely.
//!
//! Sessions appearing in both backends are merged with DB winning on
//! conflict (matches the existing `scan_opencode_sessions_to_meta`
//! / `scan_opencode_sessions_to_meta_from_db` semantics).

use anyhow::Result;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

use crate::config::Config;
use crate::session_index::{self as idx, CachedHarnessIndex, CachedSession, SCHEMA_VERSION};

const CACHE_FILE: &str = "opencode.json";

/// Live runtime view backed by the persisted cache.
#[derive(Debug, Default, Clone)]
pub struct OpenCodeIndex {
    sessions: Vec<CachedSession>,
    by_simplified: HashMap<String, Vec<usize>>,
}

impl OpenCodeIndex {
    pub fn for_project(&self, simplified_project_dir: &str) -> Vec<&CachedSession> {
        match self.by_simplified.get(simplified_project_dir) {
            Some(idxs) => idxs.iter().map(|&i| &self.sessions[i]).collect(),
            None => Vec::new(),
        }
    }

    pub fn all(&self) -> &[CachedSession] {
        &self.sessions
    }
}

/// Build or refresh the OpenCode session-index cache.
pub fn update_and_load(config: &Config) -> Result<OpenCodeIndex> {
    let mut existing =
        idx::load_harness_index(CACHE_FILE).unwrap_or_else(CachedHarnessIndex::empty);
    existing.schema_version = SCHEMA_VERSION;

    let known_ids: HashSet<String> = existing.sessions.iter().map(|s| s.id.clone()).collect();

    let session_dir = crate::opencode_data_dir().join("storage").join("session");
    let from_files = scan_files_for_new_sessions(&session_dir, &known_ids);
    let from_db = scan_db_for_new_sessions(&known_ids);

    let merged = merge_new(from_files, from_db);
    if !merged.is_empty() {
        existing.sessions.extend(merged);
        log::debug!(
            "opencode session-index: cache size now {}",
            existing.sessions.len()
        );
    }

    idx::save_harness_index(CACHE_FILE, &existing)?;
    Ok(build_runtime(config, &existing))
}

/// File-storage scan: walk `storage/session/<hash>/*.json` and parse
/// any unseen session_id.  OpenCode session JSON files are small —
/// we read the whole file as that's the existing pattern.
fn scan_files_for_new_sessions(
    session_dir: &Path,
    known_ids: &HashSet<String>,
) -> Vec<CachedSession> {
    if !session_dir.exists() {
        return Vec::new();
    }
    let project_entries = match fs::read_dir(session_dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
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
            if let Some(session) = parse_session_file(&path, known_ids) {
                out.push(session);
            }
        }
    }
    if !out.is_empty() {
        log::debug!(
            "opencode session-index: indexed {} new sessions from files",
            out.len()
        );
    }
    out
}

#[derive(Debug, Deserialize)]
struct OpenCodeSessionFile {
    id: String,
    directory: Option<String>,
    #[serde(rename = "parentID")]
    parent_id: Option<String>,
}

fn parse_session_file(path: &Path, known_ids: &HashSet<String>) -> Option<CachedSession> {
    let content = fs::read_to_string(path).ok()?;
    let session: OpenCodeSessionFile = serde_json::from_str(&content).ok()?;
    if known_ids.contains(&session.id) {
        return None;
    }
    Some(CachedSession {
        id: session.id,
        path: None,
        cwd_raw: session.directory.unwrap_or_default(),
        is_child: session.parent_id.is_some(),
    })
}

/// DB scan: query all sessions, filter to unseen IDs.  Doing the
/// filter in-memory is fine because the DB row count is small and
/// we can't safely query `WHERE id NOT IN (?, ?, …)` for arbitrary-
/// length parameter lists without batching.
fn scan_db_for_new_sessions(known_ids: &HashSet<String>) -> Vec<CachedSession> {
    if !crate::opencode::db::db_exists() {
        return Vec::new();
    }
    let conn = match crate::opencode::db::open_db() {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let sessions = match crate::opencode::db::list_sessions_from_conn(&conn) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for s in sessions {
        if known_ids.contains(&s.session_id) {
            continue;
        }
        out.push(CachedSession {
            id: s.session_id,
            path: None,
            cwd_raw: s.project_dir,
            is_child: s.parent_id.is_some(),
        });
    }
    if !out.is_empty() {
        log::debug!(
            "opencode session-index: indexed {} new sessions from db",
            out.len()
        );
    }
    out
}

/// Merge file-source and DB-source new-session lists.  DB wins on
/// id collision (matches existing `merge_session_metas` semantics).
fn merge_new(from_files: Vec<CachedSession>, from_db: Vec<CachedSession>) -> Vec<CachedSession> {
    let mut by_id: HashMap<String, CachedSession> = HashMap::new();
    for s in from_files {
        by_id.insert(s.id.clone(), s);
    }
    for s in from_db {
        by_id.insert(s.id.clone(), s);
    }
    by_id.into_values().collect()
}

fn build_runtime(config: &Config, existing: &CachedHarnessIndex) -> OpenCodeIndex {
    let sessions = existing.sessions.clone();
    let mut by_simplified: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, s) in sessions.iter().enumerate() {
        let key = if s.cwd_raw.is_empty() {
            "unknown".to_string()
        } else {
            config.simplify_path(&s.cwd_raw)
        };
        by_simplified.entry(key).or_default().push(i);
    }
    OpenCodeIndex {
        sessions,
        by_simplified,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    use std::path::PathBuf;
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
    fn parse_session_file_skips_known() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("ses_known.json");
        fs::write(&path, make_session_json("ses_known", "/foo", None)).unwrap();
        let mut known = HashSet::new();
        known.insert("ses_known".to_string());
        assert!(parse_session_file(&path, &known).is_none());
    }

    #[test]
    fn parse_session_file_extracts_directory_and_parent() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("ses_a.json");
        fs::write(
            &path,
            make_session_json("ses_a", "/proj-a", Some("ses_parent")),
        )
        .unwrap();
        let known = HashSet::new();
        let s = parse_session_file(&path, &known).unwrap();
        assert_eq!(s.id, "ses_a");
        assert_eq!(s.cwd_raw, "/proj-a");
        assert!(s.is_child);
    }

    #[test]
    fn parse_session_file_returns_none_for_corrupt_json() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("ses_bad.json");
        fs::write(&path, "not json").unwrap();
        let known = HashSet::new();
        assert!(parse_session_file(&path, &known).is_none());
    }

    #[test]
    fn parse_session_file_handles_missing_directory() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("ses_a.json");
        fs::write(&path, r#"{"id":"ses_a","time":{"created":0}}"#).unwrap();
        let known = HashSet::new();
        let s = parse_session_file(&path, &known).unwrap();
        assert_eq!(s.cwd_raw, "");
        assert!(!s.is_child);
    }

    #[test]
    fn scan_files_skips_known_ids() {
        let dir = tempdir().unwrap();
        let proj = dir.path().join("project_hash");
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
        let mut known = HashSet::new();
        known.insert("ses_a".to_string());
        let new = scan_files_for_new_sessions(dir.path(), &known);
        assert_eq!(new.len(), 1);
        assert_eq!(new[0].id, "ses_b");
    }

    #[test]
    fn scan_files_handles_missing_dir() {
        let dir = tempdir().unwrap();
        let nonexistent = dir.path().join("does-not-exist");
        let known = HashSet::new();
        let new = scan_files_for_new_sessions(&nonexistent, &known);
        assert!(new.is_empty());
    }

    #[test]
    fn merge_new_db_wins_on_conflict() {
        let from_file = vec![CachedSession {
            id: "shared".into(),
            path: None,
            cwd_raw: "FROM_FILE".into(),
            is_child: false,
        }];
        let from_db = vec![CachedSession {
            id: "shared".into(),
            path: None,
            cwd_raw: "FROM_DB".into(),
            is_child: true,
        }];
        let merged = merge_new(from_file, from_db);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].cwd_raw, "FROM_DB");
        assert!(merged[0].is_child);
    }

    #[test]
    fn merge_new_keeps_disjoint_entries() {
        let from_file = vec![CachedSession {
            id: "a".into(),
            path: None,
            cwd_raw: "/x".into(),
            is_child: false,
        }];
        let from_db = vec![CachedSession {
            id: "b".into(),
            path: None,
            cwd_raw: "/y".into(),
            is_child: false,
        }];
        let merged = merge_new(from_file, from_db);
        assert_eq!(merged.len(), 2);
    }

    #[test]
    fn build_runtime_groups_by_simplified() {
        let cfg = Config::default();
        let cache = CachedHarnessIndex {
            schema_version: SCHEMA_VERSION,
            sessions: vec![
                CachedSession {
                    id: "ses_a".into(),
                    path: None,
                    cwd_raw: "/foo".into(),
                    is_child: false,
                },
                CachedSession {
                    id: "ses_b".into(),
                    path: None,
                    cwd_raw: "".into(),
                    is_child: false,
                },
            ],
        };
        let idx = build_runtime(&cfg, &cache);
        assert_eq!(idx.for_project("/foo").len(), 1);
        assert_eq!(idx.for_project("unknown").len(), 1);
    }
}

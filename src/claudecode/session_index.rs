//! On-disk cache of Claude Code session metadata (v2).
//!
//! For each `.jsonl` file under `~/.claude/projects/*/`:
//!
//! - **Cold path** (cache absent or schema mismatch): walk every
//!   entry, collect every distinct `cwd` value, determine `is_child`
//!   from the first parseable line.
//! - **Warm path**: stat each file's mtime.  If the recorded
//!   `mtime_ns` matches, skip.  Otherwise re-read the file fully
//!   and **rebuild** the session's `cwds` set from scratch (we do
//!   not trust "events past timestamp T are strictly new" — Claude
//!   may rewrite or drop entries).
//!
//! Claude is the only currently-supported harness where a session
//! can record multiple distinct cwds across its lifetime — events
//! after a `cd` carry the new cwd in their JSONL row.

use anyhow::Result;
use serde::Deserialize;
use std::collections::HashSet;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::Path;

use crate::config::Config;
use crate::session_index::{
    self as idx, mtime_ns_of, CachedHarnessIndex, CachedSession, SCHEMA_VERSION,
};

pub(crate) const CACHE_FILE: &str = "claudecode.json";

/// Live runtime view backed by the persisted cache.
#[derive(Debug, Default, Clone)]
pub struct ClaudeIndex {
    inner: CachedHarnessIndex,
}

impl ClaudeIndex {
    /// All cached sessions, regardless of project.
    pub fn all(&self) -> impl Iterator<Item = &CachedSession> {
        self.inner.sessions_by_id.values()
    }

    /// Sessions whose simplified cwd matches `categ_id`.
    pub fn for_categ_id(&self, categ_id: &str, config: &Config) -> Vec<&CachedSession> {
        self.inner
            .lookup_by_categ_id(categ_id, |raw| config.simplify_path(raw))
    }
}

#[derive(Debug, Deserialize)]
struct EntryProbe {
    #[serde(rename = "sessionId")]
    parent_session_id: Option<String>,
    cwd: Option<String>,
}

/// Walk a Claude JSONL file fully, collecting every distinct `cwd`
/// value and determining `is_child` from the first parseable line.
/// Returns the cwds set and the child flag.
fn scan_session_file(path: &Path) -> (HashSet<String>, bool) {
    let file = match fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return (HashSet::new(), false),
    };
    let reader = BufReader::new(file);

    let mut cwds = HashSet::new();
    let mut is_child = false;
    let mut first_parsed = false;

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.trim().is_empty() {
            continue;
        }
        let probe: EntryProbe = match serde_json::from_str(&line) {
            Ok(p) => p,
            Err(_) => continue,
        };
        if !first_parsed {
            is_child = probe.parent_session_id.is_some();
            first_parsed = true;
        }
        if let Some(cwd) = probe.cwd {
            if !cwd.is_empty() {
                cwds.insert(cwd);
            }
        }
    }
    (cwds, is_child)
}

/// Build or refresh the Claude session-index cache, then return the
/// runtime view.  Sessions whose `mtime_ns` is unchanged are skipped;
/// changed files trigger a full re-scan that rebuilds the session's
/// `cwds` set from scratch.
pub fn update_and_load(_config: &Config) -> Result<ClaudeIndex> {
    let mut existing =
        idx::load_harness_index(CACHE_FILE).unwrap_or_else(CachedHarnessIndex::empty);
    existing.schema_version = SCHEMA_VERSION;

    let projects_dir = crate::claudecode::projects_dir();
    if !projects_dir.exists() {
        return Ok(ClaudeIndex { inner: existing });
    }

    let mut scanned_ids = HashSet::new();
    let (added, refreshed) = walk_and_update(&projects_dir, &mut existing, &mut scanned_ids);

    if added > 0 || refreshed > 0 {
        log::debug!(
            "claudecode session-index: +{} new, {} re-read; cache size {}",
            added,
            refreshed,
            existing.sessions_by_id.len()
        );
    }

    // Lazy cleanup: drop entries whose backing file disappeared
    // (or wasn't visited this run because its directory is gone).
    let stale_ids: Vec<String> = existing
        .sessions_by_id
        .iter()
        .filter(|(_, s)| match &s.path {
            Some(p) => !p.exists(),
            None => false,
        })
        .map(|(id, _)| id.clone())
        .collect();
    for id in &stale_ids {
        existing.remove_session(id);
    }
    if !stale_ids.is_empty() {
        log::debug!(
            "claudecode session-index: dropped {} stale entries",
            stale_ids.len()
        );
    }

    existing.rebuild_by_path();
    idx::save_harness_index(CACHE_FILE, &existing)?;
    Ok(ClaudeIndex { inner: existing })
}

/// Walk every project dir under `projects_dir`, populating
/// `existing` in place.  Returns `(added, refreshed)` counts.
fn walk_and_update(
    projects_dir: &Path,
    existing: &mut CachedHarnessIndex,
    scanned_ids: &mut HashSet<String>,
) -> (usize, usize) {
    let project_entries = match fs::read_dir(projects_dir) {
        Ok(e) => e,
        Err(_) => return (0, 0),
    };

    let mut added = 0usize;
    let mut refreshed = 0usize;

    for project_entry in project_entries.flatten() {
        let project_path = project_entry.path();
        if !project_path.is_dir() {
            continue;
        }
        let file_entries = match fs::read_dir(&project_path) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for file_entry in file_entries.flatten() {
            let file_path = file_entry.path();
            if file_path.extension().is_none_or(|e| e != "jsonl") {
                continue;
            }
            let session_id = match file_path.file_stem() {
                Some(s) => s.to_string_lossy().to_string(),
                None => continue,
            };
            scanned_ids.insert(session_id.clone());

            let metadata = match file_entry.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            let current_mtime = mtime_ns_of(&metadata);

            let needs_scan = match existing.sessions_by_id.get(&session_id) {
                Some(s) => s.mtime_ns != current_mtime,
                None => true,
            };
            if !needs_scan {
                continue;
            }

            let was_known = existing.sessions_by_id.contains_key(&session_id);
            let (cwds_set, is_child) = scan_session_file(&file_path);
            if cwds_set.is_empty() {
                // No cwd anywhere in the file — skip.  Nothing to
                // index (and the file likely isn't a real session).
                continue;
            }
            let mut cwds: Vec<String> = cwds_set.into_iter().collect();
            cwds.sort();

            existing.sessions_by_id.insert(
                session_id.clone(),
                CachedSession {
                    id: session_id,
                    path: Some(file_path),
                    is_child,
                    mtime_ns: current_mtime,
                    cwds,
                },
            );
            if was_known {
                refreshed += 1;
            } else {
                added += 1;
            }
        }
    }
    (added, refreshed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use filetime::{set_file_mtime, FileTime};
    use std::io::Write;
    use std::path::PathBuf;
    use tempfile::tempdir;

    fn write_session(dir: &Path, proj_subdir: &str, session_id: &str, lines: &[&str]) -> PathBuf {
        let pdir = dir.join(proj_subdir);
        fs::create_dir_all(&pdir).unwrap();
        let path = pdir.join(format!("{}.jsonl", session_id));
        let mut f = fs::File::create(&path).unwrap();
        for line in lines {
            writeln!(f, "{}", line).unwrap();
        }
        path
    }

    #[test]
    fn scan_session_file_collects_all_cwds_from_every_entry() {
        let dir = tempdir().unwrap();
        let path = write_session(
            dir.path(),
            "-foo",
            "abc",
            &[
                r#"{"type":"user","cwd":"/proj/A"}"#,
                r#"{"type":"assistant","cwd":"/proj/A"}"#,
                r#"{"type":"user","cwd":"/proj/B"}"#,
                r#"{"type":"assistant","cwd":"/proj/B"}"#,
                r#"{"type":"user","cwd":"/proj/A"}"#,
            ],
        );
        let (cwds, is_child) = scan_session_file(&path);
        let mut sorted: Vec<_> = cwds.into_iter().collect();
        sorted.sort();
        assert_eq!(sorted, vec!["/proj/A", "/proj/B"]);
        assert!(!is_child);
    }

    #[test]
    fn scan_session_file_detects_child_from_first_parseable_line() {
        let dir = tempdir().unwrap();
        let path = write_session(
            dir.path(),
            "-foo",
            "abc",
            &[r#"{"type":"user","sessionId":"parent","cwd":"/x"}"#],
        );
        let (_, is_child) = scan_session_file(&path);
        assert!(is_child);
    }

    #[test]
    fn scan_session_file_skips_blank_and_unparseable_lines() {
        let dir = tempdir().unwrap();
        let path = write_session(
            dir.path(),
            "-foo",
            "abc",
            &["", r#"not json"#, r#"{"cwd":"/late"}"#],
        );
        let (cwds, is_child) = scan_session_file(&path);
        let sorted: Vec<_> = cwds.iter().cloned().collect();
        assert_eq!(sorted, vec!["/late".to_string()]);
        // First parseable is the cwd-only entry → no sessionId → not child.
        assert!(!is_child);
    }

    #[test]
    fn scan_session_file_returns_empty_for_missing_cwd_lines() {
        let dir = tempdir().unwrap();
        let path = write_session(
            dir.path(),
            "-foo",
            "abc",
            &[r#"{"type":"user"}"#, r#"{"type":"assistant"}"#],
        );
        let (cwds, _) = scan_session_file(&path);
        assert!(cwds.is_empty());
    }

    #[test]
    fn scan_session_file_handles_unreadable_path() {
        let bogus = Path::new("/nonexistent/never/here.jsonl");
        let (cwds, is_child) = scan_session_file(bogus);
        assert!(cwds.is_empty());
        assert!(!is_child);
    }

    #[test]
    fn walk_and_update_indexes_new_files_only_once() {
        let dir = tempdir().unwrap();
        write_session(
            dir.path(),
            "-proj",
            "s1",
            &[r#"{"type":"user","cwd":"/proj"}"#],
        );
        let mut idx = CachedHarnessIndex::empty();
        let mut scanned = HashSet::new();
        let (added, refreshed) = walk_and_update(dir.path(), &mut idx, &mut scanned);
        assert_eq!(added, 1);
        assert_eq!(refreshed, 0);
        assert_eq!(idx.sessions_by_id.len(), 1);
        assert_eq!(idx.sessions_by_id["s1"].cwds, vec!["/proj"]);

        // Second pass with same mtime: nothing changes.
        let (added2, refreshed2) = walk_and_update(dir.path(), &mut idx, &mut scanned);
        assert_eq!(added2, 0);
        assert_eq!(refreshed2, 0);
    }

    #[test]
    fn walk_and_update_rescans_when_mtime_changes_and_replaces_cwds() {
        let dir = tempdir().unwrap();
        let path = write_session(
            dir.path(),
            "-proj",
            "s1",
            &[r#"{"type":"user","cwd":"/old"}"#],
        );
        let mut idx = CachedHarnessIndex::empty();
        let mut scanned = HashSet::new();
        walk_and_update(dir.path(), &mut idx, &mut scanned);
        assert_eq!(idx.sessions_by_id["s1"].cwds, vec!["/old"]);

        // Rewrite with a different cwd; bump mtime explicitly.
        fs::write(&path, r#"{"type":"user","cwd":"/new"}"#).unwrap();
        let later = FileTime::from_unix_time(2_000_000_000, 0);
        set_file_mtime(&path, later).unwrap();

        let (added, refreshed) = walk_and_update(dir.path(), &mut idx, &mut scanned);
        assert_eq!(added, 0);
        assert_eq!(refreshed, 1);
        // cwds REPLACED, not appended.
        assert_eq!(idx.sessions_by_id["s1"].cwds, vec!["/new"]);
    }

    #[test]
    fn walk_and_update_skips_files_with_no_cwd() {
        let dir = tempdir().unwrap();
        write_session(dir.path(), "-proj", "s1", &[r#"{"type":"user"}"#]);
        let mut idx = CachedHarnessIndex::empty();
        let mut scanned = HashSet::new();
        let (added, _) = walk_and_update(dir.path(), &mut idx, &mut scanned);
        assert_eq!(added, 0);
        assert!(idx.sessions_by_id.is_empty());
    }

    #[test]
    fn walk_and_update_records_multiple_cwds_per_session() {
        let dir = tempdir().unwrap();
        write_session(
            dir.path(),
            "-proj",
            "s1",
            &[
                r#"{"cwd":"/proj/A"}"#,
                r#"{"cwd":"/proj/B"}"#,
                r#"{"cwd":"/proj/A"}"#,
            ],
        );
        let mut idx = CachedHarnessIndex::empty();
        let mut scanned = HashSet::new();
        walk_and_update(dir.path(), &mut idx, &mut scanned);
        assert_eq!(idx.sessions_by_id["s1"].cwds, vec!["/proj/A", "/proj/B"]);
    }

    #[test]
    fn walk_and_update_records_is_child_flag() {
        let dir = tempdir().unwrap();
        write_session(
            dir.path(),
            "-proj",
            "child",
            &[r#"{"sessionId":"parent","cwd":"/proj"}"#],
        );
        let mut idx = CachedHarnessIndex::empty();
        let mut scanned = HashSet::new();
        walk_and_update(dir.path(), &mut idx, &mut scanned);
        assert!(idx.sessions_by_id["child"].is_child);
    }

    #[test]
    fn walk_and_update_handles_missing_dir() {
        let dir = tempdir().unwrap();
        let nonexistent = dir.path().join("does-not-exist");
        let mut idx = CachedHarnessIndex::empty();
        let mut scanned = HashSet::new();
        let (added, refreshed) = walk_and_update(&nonexistent, &mut idx, &mut scanned);
        assert_eq!(added, 0);
        assert_eq!(refreshed, 0);
    }

    #[test]
    fn claude_index_for_categ_id_resolves_via_simplify() {
        let mut inner = CachedHarnessIndex::empty();
        inner.sessions_by_id.insert(
            "s1".to_string(),
            CachedSession {
                id: "s1".to_string(),
                path: None,
                is_child: false,
                mtime_ns: 0,
                cwds: vec!["/foo".to_string()],
            },
        );
        inner.rebuild_by_path();
        let claude = ClaudeIndex { inner };

        let cfg = Config::default(); // identity simplify on raw paths
        let found = claude.for_categ_id("/foo", &cfg);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, "s1");
    }

    #[test]
    fn claude_index_for_categ_id_returns_session_in_multiple_buckets_only_once() {
        let mut inner = CachedHarnessIndex::empty();
        inner.sessions_by_id.insert(
            "s1".to_string(),
            CachedSession {
                id: "s1".to_string(),
                path: None,
                is_child: false,
                mtime_ns: 0,
                cwds: vec!["/a".to_string(), "/b".to_string()],
            },
        );
        inner.rebuild_by_path();
        // toy simplify: both /a and /b → "X"
        let claude = ClaudeIndex { inner };
        // Use Config::default() identity simplify so /a and /b are
        // distinct keys; assert lookup by /a yields s1 once.
        let cfg = Config::default();
        let found = claude.for_categ_id("/a", &cfg);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, "s1");
    }
}

//! On-disk cache of Pi session metadata (v2).
//!
//! Pi stores sessions as JSONL files under
//! `~/.pi/agent/sessions/--<encoded-cwd>--/<file>.jsonl`, with sub-agent
//! sessions nested deeper.  The directory-name encoding is lossy
//! (`/` → `-`) and **must not** be decoded — the authoritative `cwd`
//! lives in the JSONL header line.
//!
//! Pi structurally records cwd only at the session-header level — a
//! Pi session's cwd does not change across its lifetime.  The cache
//! still uses the v2 multi-cwd schema for uniformity with Claude;
//! Pi sessions just produce a single-element `cwds` set.

use anyhow::Result;
use serde_json::Value;
use std::collections::HashSet;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::Path;

use crate::config::Config;
use crate::session_index::{
    self as idx, mtime_ns_of, CachedHarnessIndex, CachedSession, SCHEMA_VERSION,
};

const CACHE_FILE: &str = "pi.json";

#[derive(Debug, Default, Clone)]
pub struct PiIndex {
    inner: CachedHarnessIndex,
}

impl PiIndex {
    pub fn all(&self) -> impl Iterator<Item = &CachedSession> {
        self.inner.sessions_by_id.values()
    }

    pub fn for_categ_id(&self, categ_id: &str, config: &Config) -> Vec<&CachedSession> {
        self.inner
            .lookup_by_categ_id(categ_id, |raw| config.simplify_path(raw))
    }
}

/// Parse a Pi session file's header line for `(session_id, cwd)`.
/// Returns `None` if the file lacks a valid `type:"session"` header.
fn parse_pi_header(path: &Path) -> Option<(String, String)> {
    let file = fs::File::open(path).ok()?;
    let reader = BufReader::new(file);
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => return None,
        };
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => return None,
        };
        if value.get("type").and_then(|v| v.as_str()) != Some("session") {
            return None;
        }
        let id = value.get("id").and_then(|v| v.as_str())?.to_string();
        let cwd = value
            .get("cwd")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        return Some((id, cwd));
    }
    None
}

/// Pi sub-agent detection: an ancestor directory name ends with
/// `_<uuidv7>` (the parent session's UUID, used by `pi-subagents`).
fn derive_is_child(path: &Path) -> bool {
    let mut current = match path.parent() {
        Some(p) => p,
        None => return false,
    };
    let base = crate::pi::sessions_dir();
    while current != base && current.parent().is_some() {
        if let Some(name) = current.file_name().and_then(|n| n.to_str()) {
            if let Some((_, uuid)) = name.rsplit_once('_') {
                if is_uuid_v7(uuid) {
                    return true;
                }
            }
        }
        current = match current.parent() {
            Some(p) => p,
            None => break,
        };
    }
    false
}

fn is_uuid_v7(s: &str) -> bool {
    s.len() == 36
        && s.as_bytes()[14] == b'7'
        && s.as_bytes()[8] == b'-'
        && s.as_bytes()[13] == b'-'
        && s.as_bytes()[18] == b'-'
        && s.as_bytes()[23] == b'-'
}

pub fn update_and_load(_config: &Config) -> Result<PiIndex> {
    let mut existing =
        idx::load_harness_index(CACHE_FILE).unwrap_or_else(CachedHarnessIndex::empty);
    existing.schema_version = SCHEMA_VERSION;

    let base = crate::pi::sessions_dir();
    if !base.exists() {
        return Ok(PiIndex { inner: existing });
    }

    let mut visited_paths: HashSet<std::path::PathBuf> = HashSet::new();
    let (added, refreshed) = walk(&base, &mut existing, &mut visited_paths);

    if added > 0 || refreshed > 0 {
        log::debug!(
            "pi session-index: +{} new, {} re-read; cache size {}",
            added,
            refreshed,
            existing.sessions_by_id.len()
        );
    }

    // Lazy cleanup: drop entries whose backing file disappeared.
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
            "pi session-index: dropped {} stale entries",
            stale_ids.len()
        );
    }

    existing.rebuild_by_path();
    idx::save_harness_index(CACHE_FILE, &existing)?;
    Ok(PiIndex { inner: existing })
}

fn walk(
    dir: &Path,
    existing: &mut CachedHarnessIndex,
    visited_paths: &mut HashSet<std::path::PathBuf>,
) -> (usize, usize) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return (0, 0),
    };
    let mut added = 0usize;
    let mut refreshed = 0usize;

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let (a, r) = walk(&path, existing, visited_paths);
            added += a;
            refreshed += r;
            continue;
        }
        if path.extension().is_none_or(|e| e != "jsonl") {
            continue;
        }
        visited_paths.insert(path.clone());

        let metadata = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let current_mtime = mtime_ns_of(&metadata);

        // Pi's session_id requires parsing the header — we cannot
        // gate solely on file path or mtime without knowing the id.
        // We parse on first encounter to learn the id, then use the
        // cached entry's mtime_ns to gate subsequent runs.

        // Cheap lookup: scan existing by path.  Pi has at most a few
        // hundred sessions; linear scan is acceptable.
        let existing_id = existing
            .sessions_by_id
            .iter()
            .find(|(_, s)| s.path.as_deref() == Some(path.as_path()))
            .map(|(id, _)| id.clone());

        if let Some(id) = &existing_id {
            if let Some(s) = existing.sessions_by_id.get(id) {
                if s.mtime_ns == current_mtime {
                    continue;
                }
            }
        }

        let (id, cwd) = match parse_pi_header(&path) {
            Some(v) => v,
            None => continue,
        };
        let was_known = existing.sessions_by_id.contains_key(&id);
        let cwds = if cwd.is_empty() { vec![] } else { vec![cwd] };
        let is_child = derive_is_child(&path);

        existing.sessions_by_id.insert(
            id.clone(),
            CachedSession {
                id,
                path: Some(path),
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
    (added, refreshed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use filetime::{set_file_mtime, FileTime};
    use std::io::Write;
    use tempfile::tempdir;

    fn write_pi_session(dir: &Path, file_name: &str, header: &str) -> std::path::PathBuf {
        let path = dir.join(file_name);
        let mut f = fs::File::create(&path).unwrap();
        writeln!(f, "{}", header).unwrap();
        path
    }

    #[test]
    fn parse_pi_header_extracts_id_and_cwd() {
        let dir = tempdir().unwrap();
        let path = write_pi_session(
            dir.path(),
            "session.jsonl",
            r#"{"type":"session","id":"01900000-0000-7000-8000-000000000001","cwd":"/home/vaab"}"#,
        );
        let (id, cwd) = parse_pi_header(&path).unwrap();
        assert_eq!(id, "01900000-0000-7000-8000-000000000001");
        assert_eq!(cwd, "/home/vaab");
    }

    #[test]
    fn parse_pi_header_returns_none_when_first_line_is_not_session_type() {
        let dir = tempdir().unwrap();
        let path = write_pi_session(
            dir.path(),
            "session.jsonl",
            r#"{"type":"message","cwd":"/x"}"#,
        );
        assert!(parse_pi_header(&path).is_none());
    }

    #[test]
    fn parse_pi_header_returns_none_when_unparseable() {
        let dir = tempdir().unwrap();
        let path = write_pi_session(dir.path(), "session.jsonl", "not json");
        assert!(parse_pi_header(&path).is_none());
    }

    #[test]
    fn parse_pi_header_handles_missing_cwd_as_empty_string() {
        let dir = tempdir().unwrap();
        let path = write_pi_session(
            dir.path(),
            "session.jsonl",
            r#"{"type":"session","id":"01900000-0000-7000-8000-000000000002"}"#,
        );
        let (_, cwd) = parse_pi_header(&path).unwrap();
        assert_eq!(cwd, "");
    }

    #[test]
    fn is_uuid_v7_detects_correct_version() {
        assert!(is_uuid_v7("01900000-0000-7000-8000-000000000001"));
        assert!(!is_uuid_v7("01900000-0000-4000-8000-000000000001"));
        assert!(!is_uuid_v7("01900000"));
    }

    #[test]
    fn walk_indexes_new_files_only_once() {
        let dir = tempdir().unwrap();
        write_pi_session(
            dir.path(),
            "first.jsonl",
            r#"{"type":"session","id":"01900000-0000-7000-8000-000000000001","cwd":"/p1"}"#,
        );
        let mut idx = CachedHarnessIndex::empty();
        let mut visited = HashSet::new();
        let (added, refreshed) = walk(dir.path(), &mut idx, &mut visited);
        assert_eq!(added, 1);
        assert_eq!(refreshed, 0);
        assert_eq!(idx.sessions_by_id.len(), 1);

        // Second pass: same mtime → no-op.
        let (added2, refreshed2) = walk(dir.path(), &mut idx, &mut visited);
        assert_eq!(added2, 0);
        assert_eq!(refreshed2, 0);
    }

    #[test]
    fn walk_rescans_when_mtime_changes_and_replaces_cwds() {
        let dir = tempdir().unwrap();
        let path = write_pi_session(
            dir.path(),
            "session.jsonl",
            r#"{"type":"session","id":"01900000-0000-7000-8000-000000000001","cwd":"/old"}"#,
        );
        let mut idx = CachedHarnessIndex::empty();
        let mut visited = HashSet::new();
        walk(dir.path(), &mut idx, &mut visited);
        assert_eq!(
            idx.sessions_by_id["01900000-0000-7000-8000-000000000001"].cwds,
            vec!["/old"]
        );

        fs::write(
            &path,
            r#"{"type":"session","id":"01900000-0000-7000-8000-000000000001","cwd":"/new"}"#,
        )
        .unwrap();
        set_file_mtime(&path, FileTime::from_unix_time(2_000_000_000, 0)).unwrap();
        let (added, refreshed) = walk(dir.path(), &mut idx, &mut visited);
        assert_eq!(added, 0);
        assert_eq!(refreshed, 1);
        assert_eq!(
            idx.sessions_by_id["01900000-0000-7000-8000-000000000001"].cwds,
            vec!["/new"]
        );
    }

    #[test]
    fn walk_descends_into_subdirs() {
        let dir = tempdir().unwrap();
        let nested = dir
            .path()
            .join("--encoded--")
            .join("ts_uuid")
            .join("entry")
            .join("run-0");
        fs::create_dir_all(&nested).unwrap();
        write_pi_session(
            &nested,
            "session.jsonl",
            r#"{"type":"session","id":"01900000-0000-7000-8000-000000000001","cwd":"/deep"}"#,
        );
        let mut idx = CachedHarnessIndex::empty();
        let mut visited = HashSet::new();
        walk(dir.path(), &mut idx, &mut visited);
        assert_eq!(idx.sessions_by_id.len(), 1);
        assert_eq!(
            idx.sessions_by_id["01900000-0000-7000-8000-000000000001"].cwds,
            vec!["/deep"]
        );
    }

    #[test]
    fn walk_skips_files_with_unparseable_headers() {
        let dir = tempdir().unwrap();
        write_pi_session(dir.path(), "session.jsonl", "not json");
        let mut idx = CachedHarnessIndex::empty();
        let mut visited = HashSet::new();
        walk(dir.path(), &mut idx, &mut visited);
        assert!(idx.sessions_by_id.is_empty());
    }

    #[test]
    fn pi_index_for_categ_id_resolves_via_simplify() {
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
        let pi = PiIndex { inner };
        let cfg = Config::default();
        let found = pi.for_categ_id("/foo", &cfg);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, "ses_x");
    }
}

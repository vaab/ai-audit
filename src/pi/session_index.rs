//! On-disk cache of Pi session metadata.
//!
//! Pi stores sessions as JSONL files under
//! `~/.pi/agent/sessions/--<encoded-cwd>--/<file>.jsonl`, with sub-agent
//! sessions nested deeper.  The directory-name encoding is lossy
//! (`/` → `-`) and **must not** be decoded — the authoritative `cwd`
//! lives in the JSONL header line.
//!
//! Cold path: walk the entire tree, parse the first line of every
//! `.jsonl` to extract `id` + `cwd`.
//!
//! Warm path: same walk but skip files whose path was already
//! indexed in the previous run.  `cwd` and `parent_id` (derived from
//! the file's directory chain) are immutable post-creation, so we
//! never re-parse a known file.

use anyhow::Result;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::session_index::{self as idx, CachedHarnessIndex, CachedSession, SCHEMA_VERSION};

const CACHE_FILE: &str = "pi.json";

/// Live runtime view backed by the persisted cache.
#[derive(Debug, Default, Clone)]
pub struct PiIndex {
    sessions: Vec<CachedSession>,
    by_simplified: HashMap<String, Vec<usize>>,
}

impl PiIndex {
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

/// Build or refresh the Pi session-index cache.
pub fn update_and_load(config: &Config) -> Result<PiIndex> {
    let mut existing =
        idx::load_harness_index(CACHE_FILE).unwrap_or_else(CachedHarnessIndex::empty);
    existing.schema_version = SCHEMA_VERSION;

    let base = crate::pi::sessions_dir();
    if !base.exists() {
        return Ok(build_runtime(config, &existing));
    }

    let known_paths: HashSet<PathBuf> = existing
        .sessions
        .iter()
        .filter_map(|s| s.path.clone())
        .collect();

    let new_sessions = scan_for_new_sessions(&base, &known_paths);

    if !new_sessions.is_empty() {
        existing.sessions.extend(new_sessions);
        log::debug!(
            "pi session-index: cache size now {}",
            existing.sessions.len()
        );
    }

    let before_cleanup = existing.sessions.len();
    existing.sessions.retain(|s| match &s.path {
        Some(p) => p.exists(),
        None => true,
    });
    let dropped = before_cleanup - existing.sessions.len();
    if dropped > 0 {
        log::debug!("pi session-index: dropped {} stale entries", dropped);
    }

    idx::save_harness_index(CACHE_FILE, &existing)?;
    Ok(build_runtime(config, &existing))
}

fn scan_for_new_sessions(base: &Path, known_paths: &HashSet<PathBuf>) -> Vec<CachedSession> {
    let mut out = Vec::new();
    walk(base, known_paths, &mut out);
    if !out.is_empty() {
        log::debug!("pi session-index: indexed {} new sessions", out.len());
    }
    out
}

fn walk(dir: &Path, known: &HashSet<PathBuf>, out: &mut Vec<CachedSession>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(&path, known, out);
            continue;
        }
        if path.extension().is_none_or(|e| e != "jsonl") {
            continue;
        }
        if known.contains(&path) {
            continue;
        }
        if let Some(session) = parse_pi_header(&path) {
            out.push(session);
        }
    }
}

/// Parse the first JSONL line for `id` + `cwd`; derive `is_child`
/// from the file's directory chain (sub-agent sessions live inside a
/// parent's session directory).
fn parse_pi_header(path: &Path) -> Option<CachedSession> {
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
        let cwd_raw = value
            .get("cwd")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let is_child = derive_is_child(path);
        return Some(CachedSession {
            id,
            path: Some(path.to_path_buf()),
            cwd_raw,
            is_child,
        });
    }
    None
}

/// A Pi session is a child when an ancestor directory name ends with
/// `_<uuidv7>` — the parent session's UUID, used by `pi-subagents` to
/// nest sub-agent runs.  Top-level sessions live directly in the
/// `--<encoded-cwd>--` dir and have no such ancestor.
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

fn build_runtime(config: &Config, existing: &CachedHarnessIndex) -> PiIndex {
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
    PiIndex {
        sessions,
        by_simplified,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    fn write_pi_session(dir: &Path, file_name: &str, header: &str) -> PathBuf {
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
        let session = parse_pi_header(&path).unwrap();
        assert_eq!(session.id, "01900000-0000-7000-8000-000000000001");
        assert_eq!(session.cwd_raw, "/home/vaab");
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
    fn parse_pi_header_returns_none_when_file_unparseable() {
        let dir = tempdir().unwrap();
        let path = write_pi_session(dir.path(), "session.jsonl", "not json");
        assert!(parse_pi_header(&path).is_none());
    }

    #[test]
    fn parse_pi_header_handles_missing_cwd_as_empty() {
        let dir = tempdir().unwrap();
        let path = write_pi_session(
            dir.path(),
            "session.jsonl",
            r#"{"type":"session","id":"01900000-0000-7000-8000-000000000002"}"#,
        );
        let s = parse_pi_header(&path).unwrap();
        assert_eq!(s.cwd_raw, "");
    }

    #[test]
    fn is_uuid_v7_detects_correct_version() {
        assert!(is_uuid_v7("01900000-0000-7000-8000-000000000001"));
        // Wrong version nibble at position 14:
        assert!(!is_uuid_v7("01900000-0000-4000-8000-000000000001"));
        // Wrong length:
        assert!(!is_uuid_v7("01900000"));
    }

    #[test]
    fn build_runtime_groups_by_simplified() {
        let cfg = Config::default();
        let cache = CachedHarnessIndex {
            schema_version: SCHEMA_VERSION,
            sessions: vec![
                CachedSession {
                    id: "a".into(),
                    path: Some(PathBuf::from("/x/a.jsonl")),
                    cwd_raw: "/foo".into(),
                    is_child: false,
                },
                CachedSession {
                    id: "b".into(),
                    path: Some(PathBuf::from("/x/b.jsonl")),
                    cwd_raw: "".into(),
                    is_child: false,
                },
            ],
        };
        let idx = build_runtime(&cfg, &cache);
        assert_eq!(idx.for_project("/foo").len(), 1);
        assert_eq!(idx.for_project("unknown").len(), 1);
    }

    #[test]
    fn walk_skips_known_paths() {
        let dir = tempdir().unwrap();
        let p1 = write_pi_session(
            dir.path(),
            "first.jsonl",
            r#"{"type":"session","id":"01900000-0000-7000-8000-000000000001","cwd":"/p1"}"#,
        );
        let _p2 = write_pi_session(
            dir.path(),
            "second.jsonl",
            r#"{"type":"session","id":"01900000-0000-7000-8000-000000000002","cwd":"/p2"}"#,
        );
        let mut known = HashSet::new();
        known.insert(p1.clone());
        let mut out = Vec::new();
        walk(dir.path(), &known, &mut out);
        assert_eq!(out.len(), 1, "p1 already known, only p2 should be added");
        assert_eq!(out[0].cwd_raw, "/p2");
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
        let known = HashSet::new();
        let mut out = Vec::new();
        walk(dir.path(), &known, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].cwd_raw, "/deep");
    }
}

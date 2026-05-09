//! On-disk cache of Claude Code session metadata.
//!
//! Cold path (first run, missing/corrupt cache): walks
//! `~/.claude/projects/*/` and reads the first line of every
//! `.jsonl` to extract cwd + child-session flag.  ~3.5 s on a 15 k+
//! session corpus.
//!
//! Warm path (cache present): for each `.jsonl`, stat its mtime.
//! Files older than `last_run_at` AND already in the cache are
//! skipped.  Only newly-created or modified-but-unknown sessions
//! are opened.  Typical warm-run cost is dominated by stat calls
//! (~150 ms for 15 k files).
//!
//! Cwd and child-status are immutable post-creation, so we never
//! re-read a known session.

use anyhow::Result;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::Path;

use crate::config::Config;
use crate::session_index::{self as idx, CachedHarnessIndex, CachedSession, SCHEMA_VERSION};

/// Cache file name within the session-index directory.
const CACHE_FILE: &str = "claudecode.json";

/// Live runtime view backed by the persisted cache.
#[derive(Debug, Default, Clone)]
pub struct ClaudeIndex {
    sessions: Vec<CachedSession>,
    by_simplified: HashMap<String, Vec<usize>>, // index into ``sessions``
}

impl ClaudeIndex {
    /// Sessions whose simplified project_dir equals the lookup key.
    /// Empty slice if no match.
    pub fn for_project(&self, simplified_project_dir: &str) -> Vec<&CachedSession> {
        match self.by_simplified.get(simplified_project_dir) {
            Some(indexes) => indexes.iter().map(|&i| &self.sessions[i]).collect(),
            None => Vec::new(),
        }
    }

    /// All cached sessions, regardless of project.
    pub fn all(&self) -> &[CachedSession] {
        &self.sessions
    }
}

/// First parseable JSONL line carries both the parent-session probe
/// (`sessionId` → child) and the cwd.  Reading the file once gives us
/// both signals; falling through to subsequent lines for cwd handles
/// the rare case where the first line is malformed.
#[derive(Debug, Deserialize)]
struct HeaderProbe {
    #[serde(rename = "sessionId")]
    parent_session_id: Option<String>,
    cwd: Option<String>,
}

/// Returns `(cwd_raw, is_child)` from the first parseable line.
/// Falls back to scanning subsequent lines for a `cwd` if the first
/// line lacks one.  Returns `(None, false)` on read error.
fn parse_session_header(path: &Path) -> (Option<String>, bool) {
    let file = match fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return (None, false),
    };
    let reader = BufReader::new(file);

    let mut cwd: Option<String> = None;
    let mut is_child = false;
    let mut first_line_seen = false;

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.trim().is_empty() {
            continue;
        }
        let probe: HeaderProbe = match serde_json::from_str(&line) {
            Ok(p) => p,
            Err(_) => continue,
        };
        if !first_line_seen {
            is_child = probe.parent_session_id.is_some();
            first_line_seen = true;
        }
        if cwd.is_none() && probe.cwd.is_some() {
            cwd = probe.cwd;
        }
        if cwd.is_some() {
            break;
        }
    }
    (cwd, is_child)
}

/// Build or refresh the Claude session-index cache, then return the
/// runtime view.  Sessions whose ID is already in the cache are
/// skipped (cwd and `is_child` are immutable post-creation).  Files
/// whose backing file disappeared are dropped as a lazy cleanup.
pub fn update_and_load(config: &Config) -> Result<ClaudeIndex> {
    let mut existing =
        idx::load_harness_index(CACHE_FILE).unwrap_or_else(CachedHarnessIndex::empty);
    existing.schema_version = SCHEMA_VERSION;

    let projects_dir = crate::claudecode::projects_dir();
    if !projects_dir.exists() {
        return Ok(build_runtime(config, &existing));
    }
    let known_ids: HashSet<String> = existing.sessions.iter().map(|s| s.id.clone()).collect();

    let new_sessions = scan_for_new_sessions(&projects_dir, &known_ids);

    if !new_sessions.is_empty() {
        existing.sessions.extend(new_sessions);
        log::debug!(
            "claudecode session-index: cache size now {}",
            existing.sessions.len()
        );
    }

    // Lazy cleanup: drop entries whose backing file disappeared.
    let before_cleanup = existing.sessions.len();
    existing.sessions.retain(|s| match &s.path {
        Some(p) => p.exists(),
        None => true,
    });
    let dropped = before_cleanup - existing.sessions.len();
    if dropped > 0 {
        log::debug!(
            "claudecode session-index: dropped {} stale entries",
            dropped
        );
    }

    idx::save_harness_index(CACHE_FILE, &existing)?;
    Ok(build_runtime(config, &existing))
}

fn scan_for_new_sessions(projects_dir: &Path, known_ids: &HashSet<String>) -> Vec<CachedSession> {
    let project_entries = match fs::read_dir(projects_dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };

    let mut out = Vec::new();

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

            // Skip known sessions: cwd and is_child are immutable
            // post-creation, so even an active session getting events
            // appended doesn't need re-indexing.
            if known_ids.contains(&session_id) {
                continue;
            }

            let (cwd, is_child) = parse_session_header(&file_path);
            let cwd_raw = match cwd {
                Some(c) => c,
                None => {
                    log::trace!(
                        "claudecode session-index: no cwd in {}",
                        file_path.display()
                    );
                    continue;
                }
            };

            out.push(CachedSession {
                id: session_id,
                path: Some(file_path),
                cwd_raw,
                is_child,
            });
        }
    }

    if !out.is_empty() {
        log::debug!(
            "claudecode session-index: indexed {} new sessions",
            out.len()
        );
    }
    out
}

fn build_runtime(config: &Config, existing: &CachedHarnessIndex) -> ClaudeIndex {
    let sessions = existing.sessions.clone();
    let mut by_simplified: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, s) in sessions.iter().enumerate() {
        let key = config.simplify_path(&s.cwd_raw);
        by_simplified.entry(key).or_default().push(i);
    }
    ClaudeIndex {
        sessions,
        by_simplified,
    }
}

/// Convert cached entries to `activity::SessionMeta` shape, using the
/// supplied closure to construct each `SessionMeta`.  Decoupled so
/// `activity.rs` keeps owning that type.
pub fn to_session_metas<T, F>(index: &ClaudeIndex, mut make: F) -> Vec<T>
where
    F: FnMut(&CachedSession, String) -> T,
{
    index
        .sessions
        .iter()
        .map(|s| {
            // Find the simplified key this session was grouped under.
            // We look it up to avoid recomputing ``simplify_path``.
            let key = index
                .by_simplified
                .iter()
                .find(|(_, idxs)| idxs.iter().any(|&i| std::ptr::eq(&index.sessions[i], s)))
                .map(|(k, _)| k.clone())
                .unwrap_or_default();
            make(s, key)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;
    use tempfile::tempdir;

    fn write_session(dir: &Path, proj_subdir: &str, session_id: &str, first_line: &str) -> PathBuf {
        let pdir = dir.join(proj_subdir);
        fs::create_dir_all(&pdir).unwrap();
        let path = pdir.join(format!("{}.jsonl", session_id));
        let mut f = fs::File::create(&path).unwrap();
        writeln!(f, "{}", first_line).unwrap();
        path
    }

    #[test]
    fn parse_session_header_extracts_cwd_and_child_flag() {
        let dir = tempdir().unwrap();
        let path = write_session(
            dir.path(),
            "-foo",
            "abc",
            r#"{"type":"user","sessionId":"parent-uuid","cwd":"/foo"}"#,
        );
        let (cwd, is_child) = parse_session_header(&path);
        assert_eq!(cwd.as_deref(), Some("/foo"));
        assert!(is_child);
    }

    #[test]
    fn parse_session_header_handles_non_child() {
        let dir = tempdir().unwrap();
        let path = write_session(dir.path(), "-foo", "abc", r#"{"type":"user","cwd":"/foo"}"#);
        let (cwd, is_child) = parse_session_header(&path);
        assert_eq!(cwd.as_deref(), Some("/foo"));
        assert!(!is_child);
    }

    #[test]
    fn parse_session_header_skips_blank_lines_then_finds_cwd() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("z.jsonl");
        let mut f = fs::File::create(&path).unwrap();
        writeln!(f).unwrap();
        writeln!(f, r#"{{"type":"user"}}"#).unwrap();
        writeln!(f, r#"{{"cwd":"/late"}}"#).unwrap();
        let (cwd, is_child) = parse_session_header(&path);
        assert_eq!(cwd.as_deref(), Some("/late"));
        assert!(!is_child);
    }

    #[test]
    fn parse_session_header_returns_none_for_unparseable() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("z.jsonl");
        fs::write(&path, "not json\n").unwrap();
        let (cwd, is_child) = parse_session_header(&path);
        assert!(cwd.is_none());
        assert!(!is_child);
    }

    #[test]
    fn build_runtime_groups_by_simplified() {
        let cfg = Config::default(); // identity simplify on raw paths
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
                    cwd_raw: "/foo".into(),
                    is_child: false,
                },
                CachedSession {
                    id: "c".into(),
                    path: Some(PathBuf::from("/x/c.jsonl")),
                    cwd_raw: "/bar".into(),
                    is_child: true,
                },
            ],
        };
        let idx = build_runtime(&cfg, &cache);
        assert_eq!(idx.for_project("/foo").len(), 2);
        assert_eq!(idx.for_project("/bar").len(), 1);
        assert_eq!(idx.for_project("/missing").len(), 0);
        assert_eq!(idx.all().len(), 3);
    }

    #[test]
    fn scan_for_new_sessions_finds_unseen_files() {
        let dir = tempdir().unwrap();
        write_session(
            dir.path(),
            "-proj",
            "s1",
            r#"{"type":"user","cwd":"/proj"}"#,
        );
        write_session(
            dir.path(),
            "-proj",
            "s2",
            r#"{"type":"user","cwd":"/proj"}"#,
        );
        let known: HashSet<String> = HashSet::new();
        let new = scan_for_new_sessions(dir.path(), &known);
        assert_eq!(new.len(), 2);
        let ids: HashSet<String> = new.iter().map(|s| s.id.clone()).collect();
        assert!(ids.contains("s1"));
        assert!(ids.contains("s2"));
    }

    #[test]
    fn scan_for_new_sessions_skips_known_ids() {
        let dir = tempdir().unwrap();
        write_session(
            dir.path(),
            "-proj",
            "s1",
            r#"{"type":"user","cwd":"/proj"}"#,
        );
        let mut known = HashSet::new();
        known.insert("s1".to_string());
        let new = scan_for_new_sessions(dir.path(), &known);
        assert!(new.is_empty(), "known session should be skipped");
    }

    #[test]
    fn scan_for_new_sessions_skips_files_without_cwd() {
        let dir = tempdir().unwrap();
        write_session(dir.path(), "-proj", "s1", r#"{"type":"user"}"#);
        let known = HashSet::new();
        let new = scan_for_new_sessions(dir.path(), &known);
        assert!(new.is_empty(), "session without cwd should be skipped");
    }

    #[test]
    fn scan_for_new_sessions_records_child_flag() {
        let dir = tempdir().unwrap();
        write_session(
            dir.path(),
            "-proj",
            "child",
            r#"{"sessionId":"parent","cwd":"/proj"}"#,
        );
        let known = HashSet::new();
        let new = scan_for_new_sessions(dir.path(), &known);
        assert_eq!(new.len(), 1);
        assert!(new[0].is_child);
    }

    #[test]
    fn scan_for_new_sessions_handles_missing_dir() {
        let dir = tempdir().unwrap();
        let nonexistent = dir.path().join("does-not-exist");
        let known = HashSet::new();
        let new = scan_for_new_sessions(&nonexistent, &known);
        assert!(new.is_empty());
    }
}

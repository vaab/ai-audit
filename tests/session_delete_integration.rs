//! Integration acceptance tests for ``ai-audit session delete``.
//!
//! Each test boots a tempdir-rooted fake home with the relevant
//! harness storage layout (Claude Code JSONLs, OpenCode SQLite +
//! legacy file-tree, or pi JSONLs), invokes the real release binary
//! with ``HOME``/``XDG_*``/``PI_CODING_AGENT_DIR`` redirected, and
//! asserts both stdout and the post-delete filesystem state.
//!
//! Covers the 13 acceptance cases from
//! ``doc/admin.org § Acceptance tests``:
//!
//!  1. Single-session delete (Claude Code) — JSONL + debug log gone.
//!  2. Single-session delete (OpenCode SQLite-only) — DB rows gone.
//!  3. Single-session delete (OpenCode legacy file-tree) — files gone.
//!  4. Single-session delete (Pi top-level).
//!  5. Pi with sub-agents — refuse-without-cascade.
//!  6. Pi with sub-agents — cascade.
//!  7. Filter-based delete (``--search`` + ``--type``).
//!  8. ``--dry-run`` byte-identical snapshot guard.
//!  9. Stdin NUL input from ``session list -0``.
//! 10. Stdin NDJSON input from ``session list -j``.
//! 11. Conflict: positional + filter → bail.
//! 12. Conflict: ``--ids-file`` + positional → bail.
//! 13. Regression: ``permission`` table byte-identical.

use assert_cmd::Command;
use rusqlite::{params, Connection};
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::{tempdir, TempDir};

// =============================================================================
// Helpers — fixture builders
// =============================================================================

const OPENCODE_SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS session (
        id TEXT PRIMARY KEY,
        project_id TEXT,
        parent_id TEXT,
        directory TEXT,
        title TEXT,
        time_created INTEGER NOT NULL,
        time_updated INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS message (
        id TEXT PRIMARY KEY,
        session_id TEXT NOT NULL,
        time_created INTEGER NOT NULL,
        time_updated INTEGER NOT NULL,
        data TEXT NOT NULL
    );
    CREATE TABLE IF NOT EXISTS part (
        id TEXT PRIMARY KEY,
        message_id TEXT NOT NULL,
        session_id TEXT NOT NULL,
        time_created INTEGER NOT NULL,
        time_updated INTEGER NOT NULL,
        data TEXT NOT NULL
    );
    CREATE TABLE IF NOT EXISTS permission (
        project_id TEXT PRIMARY KEY,
        data TEXT NOT NULL
    );";

/// Hand-rolled UUIDv4 (version nibble `4` at offset 14).
const UUID_V4_AAA: &str = "11111111-1111-4111-8111-111111111111";
const UUID_V4_BBB: &str = "22222222-2222-4222-8222-222222222222";

/// Hand-rolled UUIDv7 (version nibble `7` at offset 14).
const UUID_V7_PARENT: &str = "019191cd-7be0-7000-8000-000000000001";
const UUID_V7_CHILD: &str = "019191cd-7be0-7000-8000-000000000002";

/// Build an OpenCode DB at ``<home>/.local/share/opencode/opencode.db``
/// with the given sessions.
///
/// ``sessions``: list of ``(session_id, n_messages_per_session)``.
/// Each message gets one part.  All sessions share project_id ``proj_1``
/// and directory ``/home/u/proj``.  A single ``permission`` row is
/// inserted for regression coverage.
fn build_opencode_db(home: &Path, sessions: &[(&str, usize)]) {
    let db_dir = home.join(".local/share/opencode");
    fs::create_dir_all(&db_dir).unwrap();
    let conn = Connection::open(db_dir.join("opencode.db")).unwrap();
    conn.pragma_update(None, "journal_mode", "WAL").unwrap();
    conn.execute_batch(OPENCODE_SCHEMA).unwrap();
    conn.execute(
        "INSERT INTO permission (project_id, data) VALUES ('proj_1', '{\"allow\":[\"*\"]}')",
        [],
    )
    .unwrap();
    for (sid, n) in sessions {
        conn.execute(
            "INSERT INTO session (id, project_id, parent_id, directory, title, time_created, time_updated) \
             VALUES (?1, 'proj_1', NULL, '/home/u/proj', ?2, 1700000000000, 1700000010000)",
            params![sid, sid],
        ).unwrap();
        for i in 0..*n {
            let msg_id = format!("msg_{}_{}", sid, i);
            conn.execute(
                "INSERT INTO message (id, session_id, time_created, time_updated, data) \
                 VALUES (?1, ?2, 1700000001000, 1700000001000, '{\"role\":\"user\",\"time\":{\"created\":1700000001000}}')",
                params![msg_id, sid],
            ).unwrap();
            let part_id = format!("prt_{}_{}", sid, i);
            conn.execute(
                "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) \
                 VALUES (?1, ?2, ?3, 1700000002000, 1700000002000, '{\"type\":\"text\",\"text\":\"hi\"}')",
                params![part_id, msg_id, sid],
            ).unwrap();
        }
    }
}

/// Seed an OpenCode legacy file-tree entry for ``session_id`` under
/// ``<home>/.local/share/opencode/storage/``.  Creates
/// ``directory-agents/<id>.json``, ``message/<id>/msg_a.json``,
/// ``message/<id>/msg_b.json``, ``part/msg_a/prt_a.json``,
/// ``part/msg_b/prt_b.json``, and
/// ``session/<project_hash>/<id>.json``.
fn build_opencode_legacy_tree(home: &Path, session_id: &str, project_hash: &str) -> LegacyPaths {
    let storage = home.join(".local/share/opencode/storage");

    let dir_agents = storage.join("directory-agents");
    fs::create_dir_all(&dir_agents).unwrap();
    let dir_agents_path = dir_agents.join(format!("{}.json", session_id));
    fs::write(
        &dir_agents_path,
        format!(
            r#"{{"sessionID":"{}","updatedAt":2,"directory":"/home/u/proj"}}"#,
            session_id
        ),
    )
    .unwrap();

    let session_proj = storage.join("session").join(project_hash);
    fs::create_dir_all(&session_proj).unwrap();
    let session_json = session_proj.join(format!("{}.json", session_id));
    fs::write(
        &session_json,
        format!(r#"{{"id":"{}","directory":"/home/u/proj"}}"#, session_id),
    )
    .unwrap();

    let msg_dir = storage.join("message").join(session_id);
    fs::create_dir_all(&msg_dir).unwrap();
    fs::write(msg_dir.join("msg_a.json"), "{}").unwrap();
    fs::write(msg_dir.join("msg_b.json"), "{}").unwrap();

    let part_a = storage.join("part").join("msg_a");
    fs::create_dir_all(&part_a).unwrap();
    fs::write(part_a.join("prt_1.json"), r#"{"type":"text"}"#).unwrap();
    let part_b = storage.join("part").join("msg_b");
    fs::create_dir_all(&part_b).unwrap();
    fs::write(part_b.join("prt_1.json"), r#"{"type":"text"}"#).unwrap();

    LegacyPaths {
        dir_agents: dir_agents_path,
        session_json,
        message_dir: msg_dir,
        part_a_dir: part_a,
        part_b_dir: part_b,
    }
}

struct LegacyPaths {
    dir_agents: PathBuf,
    session_json: PathBuf,
    message_dir: PathBuf,
    part_a_dir: PathBuf,
    part_b_dir: PathBuf,
}

/// Seed a Claude Code session JSONL plus an optional debug log under
/// ``<home>/.claude/projects/<encoded>/<uuid>.jsonl`` and
/// ``<home>/.claude/debug/<uuid>.txt``.
fn build_claudecode_session(
    home: &Path,
    encoded_proj: &str,
    session_uuid: &str,
    with_debug: bool,
) -> ClaudePaths {
    let proj_dir = home.join(".claude/projects").join(encoded_proj);
    fs::create_dir_all(&proj_dir).unwrap();
    let jsonl = proj_dir.join(format!("{}.jsonl", session_uuid));
    fs::write(
        &jsonl,
        format!(
            r#"{{"type":"user","cwd":"/home/u/proj","sessionId":"{}","timestamp":"2026-01-01T00:00:00Z","message":{{"role":"user","content":"hello"}}}}"#,
            session_uuid
        ),
    )
    .unwrap();

    let debug = if with_debug {
        let debug_dir = home.join(".claude/debug");
        fs::create_dir_all(&debug_dir).unwrap();
        let p = debug_dir.join(format!("{}.txt", session_uuid));
        fs::write(&p, "debug-log-content").unwrap();
        Some(p)
    } else {
        None
    };
    ClaudePaths {
        jsonl,
        debug,
        proj_dir,
    }
}

struct ClaudePaths {
    jsonl: PathBuf,
    debug: Option<PathBuf>,
    proj_dir: PathBuf,
}

/// Seed a top-level pi session JSONL at
/// ``<pi_dir>/sessions/<encoded>/<iso>_<uuid>.jsonl``.
fn build_pi_top_level(pi_dir: &Path, encoded: &str, uuid: &str, cwd: &str) -> PathBuf {
    let proj = pi_dir.join("sessions").join(encoded);
    fs::create_dir_all(&proj).unwrap();
    let path = proj.join(format!("2026-01-01T00-00-00_{}.jsonl", uuid));
    fs::write(
        &path,
        format!(
            r#"{{"type":"session","version":3,"id":"{}","timestamp":"2026-01-01T00:00:00Z","cwd":"{}"}}"#,
            uuid, cwd
        ),
    )
    .unwrap();
    path
}

/// Seed a pi sub-agent session at
/// ``<pi_dir>/sessions/<encoded>/<iso>_<parent>/<entry>/run-N/session.jsonl``.
fn build_pi_subagent(
    pi_dir: &Path,
    encoded: &str,
    parent_uuid: &str,
    entry: &str,
    run_n: usize,
    child_uuid: &str,
    cwd: &str,
) -> PathBuf {
    let dir = pi_dir
        .join("sessions")
        .join(encoded)
        .join(format!("2026-01-01T00-00-00_{}", parent_uuid))
        .join(entry)
        .join(format!("run-{}", run_n));
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("session.jsonl");
    fs::write(
        &path,
        format!(
            r#"{{"type":"session","version":3,"id":"{}","timestamp":"2026-01-01T00:00:01Z","cwd":"{}"}}"#,
            child_uuid, cwd
        ),
    )
    .unwrap();
    path
}

/// Common Command builder: invokes the release binary with HOME +
/// XDG dirs + PI dir redirected and the three ``*_SESSION_ID`` env
/// vars cleared (so self-deletion guard doesn't trip).
fn ai_audit(home: &Path, pi_dir: Option<&Path>) -> Command {
    let mut cmd = Command::cargo_bin("ai-audit").unwrap();
    cmd.env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env_remove("OPENCODE_SESSION_ID")
        .env_remove("CLAUDE_SESSION_ID")
        .env_remove("PI_SESSION_ID");
    if let Some(pd) = pi_dir {
        cmd.env("PI_CODING_AGENT_DIR", pd);
    } else {
        cmd.env_remove("PI_CODING_AGENT_DIR");
    }
    cmd
}

// =============================================================================
// Tests
// =============================================================================

// ---------- 1. Claude Code single-session ------------------------------------

#[test]
fn claudecode_delete_removes_jsonl_and_debug_log() {
    let home = tempdir().unwrap();
    let paths = build_claudecode_session(home.path(), "-home-u-proj", UUID_V4_AAA, true);
    assert!(paths.jsonl.exists());
    assert!(paths.debug.as_ref().unwrap().exists());

    let out = ai_audit(home.path(), None)
        .args(["session", "delete", UUID_V4_AAA])
        .assert()
        .success();

    assert!(!paths.jsonl.exists());
    assert!(!paths.debug.unwrap().exists());
    // Empty project dir pruned.
    assert!(!paths.proj_dir.exists());

    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(stdout.contains("deleted"), "stdout: {}", stdout);
    assert!(stdout.contains("claudecode"), "stdout: {}", stdout);
    assert!(stdout.contains("Deleted: 1."), "stdout: {}", stdout);
}

// ---------- 2. OpenCode SQLite-only single-session ---------------------------

#[test]
fn opencode_delete_wipes_db_rows_keeps_sibling_and_permission_table() {
    let home = tempdir().unwrap();
    build_opencode_db(home.path(), &[("ses_target", 2), ("ses_keep", 1)]);

    let db_path = home.path().join(".local/share/opencode/opencode.db");

    // Snapshot permission BEFORE.
    let conn = Connection::open(&db_path).unwrap();
    let perm_before: String = conn
        .query_row(
            "SELECT data FROM permission WHERE project_id = 'proj_1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    drop(conn);

    ai_audit(home.path(), None)
        .args(["session", "delete", "ses_target"])
        .assert()
        .success();

    let conn = Connection::open(&db_path).unwrap();
    let n_target: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM session WHERE id = 'ses_target'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let n_keep: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM session WHERE id = 'ses_keep'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let n_msg_target: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM message WHERE session_id = 'ses_target'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let n_part_target: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM part WHERE session_id = 'ses_target'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let perm_after: String = conn
        .query_row(
            "SELECT data FROM permission WHERE project_id = 'proj_1'",
            [],
            |r| r.get(0),
        )
        .unwrap();

    assert_eq!(n_target, 0);
    assert_eq!(n_keep, 1, "sibling must survive");
    assert_eq!(n_msg_target, 0);
    assert_eq!(n_part_target, 0);
    assert_eq!(perm_before, perm_after, "permission row must be intact");
}

// ---------- 3. OpenCode legacy file-tree -------------------------------------

#[test]
fn opencode_delete_wipes_legacy_file_tree() {
    let home = tempdir().unwrap();
    build_opencode_db(home.path(), &[("ses_target", 0)]); // DB row only
    let legacy = build_opencode_legacy_tree(home.path(), "ses_target", "proj_hash_a");

    ai_audit(home.path(), None)
        .args(["session", "delete", "ses_target"])
        .assert()
        .success();

    assert!(!legacy.dir_agents.exists());
    assert!(!legacy.session_json.exists());
    assert!(!legacy.message_dir.exists());
    assert!(!legacy.part_a_dir.exists());
    assert!(!legacy.part_b_dir.exists());
}

// ---------- 4. Pi top-level --------------------------------------------------

#[test]
fn pi_delete_top_level_removes_jsonl() {
    let home = tempdir().unwrap();
    let pi_dir = tempdir().unwrap();
    let path = build_pi_top_level(pi_dir.path(), "-tmp-proj", UUID_V7_PARENT, "/tmp/proj");
    assert!(path.exists());

    ai_audit(home.path(), Some(pi_dir.path()))
        .args(["session", "delete", UUID_V7_PARENT])
        .assert()
        .success();

    assert!(!path.exists());
}

// ---------- 5. Pi refuse-without-cascade -------------------------------------

#[test]
fn pi_delete_refuses_when_children_exist_without_cascade() {
    let home = tempdir().unwrap();
    let pi_dir = tempdir().unwrap();
    let parent = build_pi_top_level(pi_dir.path(), "-tmp-proj", UUID_V7_PARENT, "/tmp/proj");
    let child = build_pi_subagent(
        pi_dir.path(),
        "-tmp-proj",
        UUID_V7_PARENT,
        "entry_1",
        0,
        UUID_V7_CHILD,
        "/tmp/proj",
    );

    let out = ai_audit(home.path(), Some(pi_dir.path()))
        .args(["session", "delete", UUID_V7_PARENT])
        .assert()
        .failure();

    // Files untouched.
    assert!(parent.exists());
    assert!(child.exists());

    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains("--cascade") || stdout.contains("cascade"),
        "stdout: {}",
        stdout
    );
}

// ---------- 6. Pi cascade ----------------------------------------------------

#[test]
fn pi_delete_cascade_removes_parent_and_children() {
    let home = tempdir().unwrap();
    let pi_dir = tempdir().unwrap();
    let parent = build_pi_top_level(pi_dir.path(), "-tmp-proj", UUID_V7_PARENT, "/tmp/proj");
    let child = build_pi_subagent(
        pi_dir.path(),
        "-tmp-proj",
        UUID_V7_PARENT,
        "entry_1",
        0,
        UUID_V7_CHILD,
        "/tmp/proj",
    );

    ai_audit(home.path(), Some(pi_dir.path()))
        .args(["session", "delete", "--cascade", UUID_V7_PARENT])
        .assert()
        .success();

    assert!(!parent.exists());
    assert!(!child.exists());
}

// ---------- 7. Filter-based bulk delete --------------------------------------

#[test]
fn filter_delete_picks_up_only_matching_sessions() {
    // Two claudecode sessions; we delete only the one whose JSONL
    // contains a specific marker via `--search`.
    let home = tempdir().unwrap();
    let p1 = home.path().join(".claude/projects/-home-u-proj");
    fs::create_dir_all(&p1).unwrap();
    let target_jsonl = p1.join(format!("{}.jsonl", UUID_V4_AAA));
    fs::write(
        &target_jsonl,
        format!(
            "{{\"type\":\"user\",\"cwd\":\"/home/u/proj\",\"sessionId\":\"{}\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"message\":{{\"role\":\"user\",\"content\":\"MAGIC-MARKER-7Q\"}}}}\n",
            UUID_V4_AAA
        ),
    )
    .unwrap();
    let keep_jsonl = p1.join(format!("{}.jsonl", UUID_V4_BBB));
    fs::write(
        &keep_jsonl,
        format!(
            "{{\"type\":\"user\",\"cwd\":\"/home/u/proj\",\"sessionId\":\"{}\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"message\":{{\"role\":\"user\",\"content\":\"unrelated content\"}}}}\n",
            UUID_V4_BBB
        ),
    )
    .unwrap();

    ai_audit(home.path(), None)
        .args([
            "session",
            "delete",
            "--all",
            "--type",
            "claudecode",
            "--search",
            "MAGIC-MARKER-7Q",
        ])
        .assert()
        .success();

    assert!(!target_jsonl.exists(), "matching session should be deleted");
    assert!(keep_jsonl.exists(), "non-matching session must survive");
}

// ---------- 8. Dry-run is byte-identical ------------------------------------

#[test]
fn dry_run_performs_zero_writes() {
    let home = tempdir().unwrap();
    build_opencode_db(home.path(), &[("ses_target", 2)]);
    let legacy = build_opencode_legacy_tree(home.path(), "ses_target", "proj_hash_a");

    // Snapshot relevant files BEFORE.
    let db_path = home.path().join(".local/share/opencode/opencode.db");
    let db_bytes_before = fs::read(&db_path).unwrap();
    let dir_agents_bytes_before = fs::read(&legacy.dir_agents).unwrap();
    let session_json_bytes_before = fs::read(&legacy.session_json).unwrap();

    let out = ai_audit(home.path(), None)
        .args(["session", "delete", "--dry-run", "ses_target"])
        .assert()
        .success();

    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains("would-delete"),
        "expected would-delete prefix: {}",
        stdout
    );
    assert!(
        stdout.contains("Would-delete: 1."),
        "expected summary: {}",
        stdout
    );

    // Byte-identical guard.
    assert_eq!(fs::read(&db_path).unwrap(), db_bytes_before);
    assert_eq!(
        fs::read(&legacy.dir_agents).unwrap(),
        dir_agents_bytes_before
    );
    assert_eq!(
        fs::read(&legacy.session_json).unwrap(),
        session_json_bytes_before
    );
}

// ---------- 9. Stdin NUL input ----------------------------------------------

#[test]
fn ids_file_nul_separated_from_stdin() {
    let home = tempdir().unwrap();
    build_opencode_db(
        home.path(),
        &[("ses_aaa", 1), ("ses_bbb", 1), ("ses_keep", 1)],
    );

    ai_audit(home.path(), None)
        .args(["session", "delete", "--ids-file", "-"])
        .write_stdin("ses_aaa\0ses_bbb\0")
        .assert()
        .success();

    let db_path = home.path().join(".local/share/opencode/opencode.db");
    let conn = Connection::open(&db_path).unwrap();
    let n_aaa: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM session WHERE id = 'ses_aaa'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let n_bbb: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM session WHERE id = 'ses_bbb'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let n_keep: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM session WHERE id = 'ses_keep'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n_aaa, 0);
    assert_eq!(n_bbb, 0);
    assert_eq!(n_keep, 1);
}

// ---------- 10. Stdin NDJSON input ------------------------------------------

#[test]
fn ids_file_ndjson_from_stdin() {
    let home = tempdir().unwrap();
    build_opencode_db(
        home.path(),
        &[("ses_aaa", 1), ("ses_bbb", 1), ("ses_keep", 1)],
    );

    let ndjson = "{\"session_id\":\"ses_aaa\"}\n{\"id\":\"ses_bbb\",\"foo\":1}\n";

    ai_audit(home.path(), None)
        .args(["session", "delete", "--ids-file", "-"])
        .write_stdin(ndjson)
        .assert()
        .success();

    let db_path = home.path().join(".local/share/opencode/opencode.db");
    let conn = Connection::open(&db_path).unwrap();
    let n_keep: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM session WHERE id = 'ses_keep'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let total: i64 = conn
        .query_row("SELECT COUNT(*) FROM session", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n_keep, 1);
    assert_eq!(total, 1);
}

// ---------- 11. Conflict: positional + filter -------------------------------

#[test]
fn conflict_positional_with_filter_is_rejected() {
    let home = tempdir().unwrap();

    let out = ai_audit(home.path(), None)
        .args(["session", "delete", "ses_aaa", "--search", "foo"])
        .assert()
        .failure();

    let stderr = String::from_utf8(out.get_output().stderr.clone()).unwrap();
    assert!(
        stderr.contains("<SESSION-ID> cannot be combined") || stderr.contains("cannot be combined"),
        "stderr: {}",
        stderr
    );
}

// ---------- 12. Conflict: --ids-file + positional ---------------------------

#[test]
fn conflict_ids_file_with_positional_is_rejected_by_arggroup() {
    // ArgGroup (parser layer) catches this — clap's error wording.
    let home = tempdir().unwrap();

    let out = ai_audit(home.path(), None)
        .args(["session", "delete", "ses_aaa", "--ids-file", "/tmp/x"])
        .assert()
        .failure();

    let stderr = String::from_utf8(out.get_output().stderr.clone()).unwrap();
    assert!(
        stderr.contains("cannot be used with")
            || stderr.contains("conflict")
            || stderr.to_lowercase().contains("error"),
        "expected clap conflict error: {}",
        stderr
    );
}

// ---------- 13. Permission table byte-identical -----------------------------

#[test]
fn permission_table_is_byte_identical_after_delete() {
    let home = tempdir().unwrap();
    build_opencode_db(home.path(), &[("ses_target", 2)]);
    let db_path = home.path().join(".local/share/opencode/opencode.db");

    let conn = Connection::open(&db_path).unwrap();
    let rows_before: Vec<(String, String)> = conn
        .prepare("SELECT project_id, data FROM permission ORDER BY project_id")
        .unwrap()
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    drop(conn);

    ai_audit(home.path(), None)
        .args(["session", "delete", "ses_target"])
        .assert()
        .success();

    let conn = Connection::open(&db_path).unwrap();
    let rows_after: Vec<(String, String)> = conn
        .prepare("SELECT project_id, data FROM permission ORDER BY project_id")
        .unwrap()
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();

    assert_eq!(rows_before, rows_after);
}

// ---------- Additional: --all without filter is rejected --------------------

#[test]
fn all_without_any_filter_is_rejected() {
    let home = tempdir().unwrap();
    build_opencode_db(home.path(), &[("ses_target", 1)]);

    let out = ai_audit(home.path(), None)
        .args(["session", "delete", "--all"])
        .assert()
        .failure();

    let stderr = String::from_utf8(out.get_output().stderr.clone()).unwrap();
    assert!(
        stderr.contains("--all requires") || stderr.contains("filter"),
        "stderr: {}",
        stderr
    );

    // Target untouched.
    let conn = Connection::open(home.path().join(".local/share/opencode/opencode.db")).unwrap();
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM session WHERE id = 'ses_target'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 1);
}

// ---------- Additional: self-deletion guard via env var ---------------------

#[test]
fn self_deletion_via_opencode_session_id_env_is_refused() {
    let home = tempdir().unwrap();
    build_opencode_db(home.path(), &[("ses_target", 1)]);

    let out = Command::cargo_bin("ai-audit")
        .unwrap()
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path().join(".config"))
        .env("XDG_CACHE_HOME", home.path().join(".cache"))
        .env("OPENCODE_SESSION_ID", "ses_target")
        .env_remove("CLAUDE_SESSION_ID")
        .env_remove("PI_SESSION_ID")
        .args(["session", "delete", "ses_target"])
        .assert()
        .failure();

    let stderr = String::from_utf8(out.get_output().stderr.clone()).unwrap();
    assert!(
        stderr.contains("refuse to delete") || stderr.contains("OPENCODE_SESSION_ID"),
        "stderr: {}",
        stderr
    );

    // Target untouched.
    let conn = Connection::open(home.path().join(".local/share/opencode/opencode.db")).unwrap();
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM session WHERE id = 'ses_target'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 1);
}

// ---------- Additional: NUL `-0` output is parseable ------------------------

#[test]
fn nul_output_is_machine_parseable() {
    let home = tempdir().unwrap();
    build_opencode_db(home.path(), &[("ses_target", 1)]);

    let out = ai_audit(home.path(), None)
        .args(["session", "delete", "-0", "ses_target"])
        .assert()
        .success();

    let stdout = out.get_output().stdout.clone();
    // Fixed-field record: id\0harness\0result\0count\0err\0
    let parts: Vec<&[u8]> = stdout.split(|&b| b == 0).collect();
    // Expect 5 fields + trailing empty (after final NUL) = 6 elements
    assert_eq!(parts.len(), 6, "unexpected field count: {:?}", parts);
    assert_eq!(parts[0], b"ses_target");
    assert_eq!(parts[1], b"opencode");
    assert_eq!(parts[2], b"deleted");
    // parts[3] = count (numeric); parts[4] = error (empty); parts[5] = trailing
    assert!(!parts[3].is_empty(), "count field should be non-empty");
    assert!(
        parts[4].is_empty(),
        "error field should be empty on success"
    );
}

// ---------- Type-aware: cross-harness with mixed IDs ------------------------

#[test]
fn mixed_id_batch_from_stdin_works() {
    let home = tempdir().unwrap();
    let pi_dir = tempdir().unwrap();
    // OpenCode side.
    build_opencode_db(home.path(), &[("ses_aaa", 1)]);
    // Claude Code side.
    let claude_paths = build_claudecode_session(home.path(), "-home-u-proj", UUID_V4_AAA, false);
    // Pi side.
    let pi_path = build_pi_top_level(pi_dir.path(), "-tmp-proj", UUID_V7_PARENT, "/tmp/proj");

    let stdin = format!("ses_aaa\0{}\0{}\0", UUID_V4_AAA, UUID_V7_PARENT);

    ai_audit(home.path(), Some(pi_dir.path()))
        .args(["session", "delete", "--ids-file", "-"])
        .write_stdin(stdin)
        .assert()
        .success();

    // OpenCode row gone.
    let conn = Connection::open(home.path().join(".local/share/opencode/opencode.db")).unwrap();
    let n_aaa: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM session WHERE id = 'ses_aaa'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n_aaa, 0);

    // Claude JSONL gone.
    assert!(!claude_paths.jsonl.exists());

    // Pi JSONL gone.
    assert!(!pi_path.exists());
}

// Silence unused warning for ``TempDir`` when only used as a guard.
#[allow(dead_code)]
fn _force_tempdir_use(_: TempDir) {}

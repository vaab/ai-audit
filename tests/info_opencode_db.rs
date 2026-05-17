//! Integration test: `ai-audit session info <ses_*>` against an
//! OpenCode SQLite fixture.
//!
//! Builds a tempdir-rooted opencode.db, populates it with a single
//! session + a few messages and parts mirroring the real on-disk
//! shape, then invokes the binary with `HOME` redirected to the
//! tempdir and asserts the human output.

use assert_cmd::Command;
use rusqlite::{params, Connection};
use std::fs;
use std::path::Path;
use tempfile::tempdir;

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
    );";

fn build_opencode_db(home: &Path) {
    let db_dir = home.join(".local/share/opencode");
    fs::create_dir_all(&db_dir).unwrap();
    let conn = Connection::open(db_dir.join("opencode.db")).unwrap();
    // Match the journal mode opencode uses, so the read-only opener
    // doesn't need to perform a WAL conversion (which would require
    // write access).
    conn.pragma_update(None, "journal_mode", "WAL").unwrap();
    conn.execute_batch(OPENCODE_SCHEMA).unwrap();

    conn.execute(
        "INSERT INTO session (id, project_id, parent_id, directory, title, time_created, time_updated) \
         VALUES ('ses_test01', 'proj_1', NULL, '/home/u/proj', 'My session', 1700000000000, 1700000010000)",
        [],
    ).unwrap();

    // User message
    conn.execute(
        "INSERT INTO message (id, session_id, time_created, time_updated, data) \
         VALUES ('msg_u', 'ses_test01', 1700000001000, 1700000001000, ?1)",
        params![r#"{"role":"user","time":{"created":1700000001000}}"#],
    )
    .unwrap();

    // Assistant message — completed, no errors, with one tool call
    conn.execute(
        "INSERT INTO message (id, session_id, time_created, time_updated, data) \
         VALUES ('msg_a', 'ses_test01', 1700000002000, 1700000003000, ?1)",
        params![r#"{"role":"assistant","time":{"created":1700000002000,"completed":1700000003000},"agent":"build","providerID":"anthropic","modelID":"claude-opus-4-7"}"#],
    )
    .unwrap();

    conn.execute(
        "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) \
         VALUES ('prt_text', 'msg_a', 'ses_test01', 1700000002100, 1700000002100, ?1)",
        params![r#"{"type":"text","text":"hi there"}"#],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) \
         VALUES ('prt_tool', 'msg_a', 'ses_test01', 1700000002200, 1700000002200, ?1)",
        params![r#"{"type":"tool","tool":"bash","state":{"status":"completed"}}"#],
    )
    .unwrap();
}

#[test]
fn opencode_session_info_renders_full_human_block() {
    let home = tempdir().unwrap();
    build_opencode_db(home.path());

    let output = Command::cargo_bin("ai-audit")
        .unwrap()
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path().join(".config"))
        .env("XDG_CACHE_HOME", home.path().join(".cache"))
        .env_remove("OPENCODE_SESSION_ID")
        .env_remove("CLAUDE_SESSION_ID")
        .env_remove("PI_SESSION_ID")
        .args(["session", "info", "ses_test01", "--no-live"])
        .assert()
        .success();

    let stdout = String::from_utf8(output.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains("Session:        ses_test01"),
        "got:\n{}",
        stdout
    );
    assert!(
        stdout.contains("Type:           opencode"),
        "got:\n{}",
        stdout
    );
    assert!(
        stdout.contains("Project:        /home/u/proj"),
        "got:\n{}",
        stdout
    );
    assert!(
        stdout.contains("Title:          My session"),
        "got:\n{}",
        stdout
    );
    assert!(stdout.contains("Messages:       2"), "got:\n{}", stdout);
    assert!(stdout.contains("Tool calls:     1"), "got:\n{}", stdout);
    assert!(
        stdout.contains("Static status:  completed"),
        "got:\n{}",
        stdout
    );
    assert!(stdout.contains("Agent:          build"), "got:\n{}", stdout);
    assert!(
        stdout.contains("Model:          anthropic/claude-opus-4-7"),
        "got:\n{}",
        stdout
    );
    // No live-status line when --no-live
    assert!(!stdout.contains("Live status:"), "got:\n{}", stdout);
    // Footer hint
    assert!(stdout.contains("See also: ai-audit usage ses_test01"));
}

#[test]
fn opencode_session_info_marks_errored_session_as_aborted() {
    let home = tempdir().unwrap();
    let db_dir = home.path().join(".local/share/opencode");
    fs::create_dir_all(&db_dir).unwrap();
    let conn = Connection::open(db_dir.join("opencode.db")).unwrap();
    conn.pragma_update(None, "journal_mode", "WAL").unwrap();
    conn.execute_batch(OPENCODE_SCHEMA).unwrap();
    conn.execute(
        "INSERT INTO session (id, project_id, parent_id, directory, title, time_created, time_updated) \
         VALUES ('ses_err001', 'p', NULL, '/p', 't', 1700000000000, 1700000010000)",
        [],
    )
    .unwrap();
    // Assistant message that errored mid-stream
    conn.execute(
        "INSERT INTO message (id, session_id, time_created, time_updated, data) \
         VALUES ('msg_a', 'ses_err001', 1700000001000, 1700000002000, ?1)",
        params![r#"{"role":"assistant","time":{"created":1700000001000,"completed":1700000002000},"error":{"name":"APIError"}}"#],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) \
         VALUES ('prt_text', 'msg_a', 'ses_err001', 1700000001500, 1700000001500, ?1)",
        params![r#"{"type":"text","text":"partial"}"#],
    )
    .unwrap();

    let output = Command::cargo_bin("ai-audit")
        .unwrap()
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path().join(".config"))
        .env("XDG_CACHE_HOME", home.path().join(".cache"))
        .env_remove("OPENCODE_SESSION_ID")
        .env_remove("CLAUDE_SESSION_ID")
        .env_remove("PI_SESSION_ID")
        .args(["session", "info", "ses_err001", "--no-live"])
        .assert()
        .success();

    let stdout = String::from_utf8(output.get_output().stdout.clone()).unwrap();
    assert!(stdout.contains("(aborted)"), "got:\n{}", stdout);
    assert!(
        stdout.contains("Static status:  assistant-partial"),
        "got:\n{}",
        stdout
    );
}

//! Integration test: `ai-audit session info <unknown-id>` exits
//! non-zero with a clear "session not found" error.

use assert_cmd::Command;
use rusqlite::Connection;
use std::fs;
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

#[test]
fn unknown_opencode_session_exits_nonzero() {
    let home = tempdir().unwrap();
    let db_dir = home.path().join(".local/share/opencode");
    fs::create_dir_all(&db_dir).unwrap();
    let conn = Connection::open(db_dir.join("opencode.db")).unwrap();
    conn.pragma_update(None, "journal_mode", "WAL").unwrap();
    conn.execute_batch(OPENCODE_SCHEMA).unwrap();
    // No rows inserted.

    let output = Command::cargo_bin("ai-audit")
        .unwrap()
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path().join(".config"))
        .env("XDG_CACHE_HOME", home.path().join(".cache"))
        .env_remove("OPENCODE_SESSION_ID")
        .env_remove("CLAUDE_SESSION_ID")
        .env_remove("PI_SESSION_ID")
        .args(["session", "info", "ses_missing01", "--no-live"])
        .assert()
        .failure();

    let stderr = String::from_utf8(output.get_output().stderr.clone()).unwrap();
    assert!(
        stderr.contains("session not found"),
        "stderr should mention 'session not found', got:\n{}",
        stderr
    );
    assert!(
        stderr.contains("ses_missing01"),
        "stderr should mention the missing id, got:\n{}",
        stderr
    );
}

#[test]
fn unknown_claudecode_session_exits_nonzero() {
    let home = tempdir().unwrap();
    // Empty .claude/projects, valid UUIDv4 — no fixture file.
    fs::create_dir_all(home.path().join(".claude/projects")).unwrap();
    let session_id = "11111111-2222-4333-8444-555555555555";

    let output = Command::cargo_bin("ai-audit")
        .unwrap()
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path().join(".config"))
        .env("XDG_CACHE_HOME", home.path().join(".cache"))
        .env_remove("OPENCODE_SESSION_ID")
        .env_remove("CLAUDE_SESSION_ID")
        .env_remove("PI_SESSION_ID")
        .args(["session", "info", session_id])
        .assert()
        .failure();

    let stderr = String::from_utf8(output.get_output().stderr.clone()).unwrap();
    assert!(
        stderr.contains("session not found"),
        "stderr should mention 'session not found', got:\n{}",
        stderr
    );
}

#[test]
fn malformed_session_id_exits_nonzero() {
    let home = tempdir().unwrap();
    let output = Command::cargo_bin("ai-audit")
        .unwrap()
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path().join(".config"))
        .env("XDG_CACHE_HOME", home.path().join(".cache"))
        .env_remove("OPENCODE_SESSION_ID")
        .env_remove("CLAUDE_SESSION_ID")
        .env_remove("PI_SESSION_ID")
        .args(["session", "info", "not-a-uuid"])
        .assert()
        .failure();

    let stderr = String::from_utf8(output.get_output().stderr.clone()).unwrap();
    assert!(
        stderr.to_lowercase().contains("session id"),
        "stderr should mention the malformed id, got:\n{}",
        stderr
    );
}

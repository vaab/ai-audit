//! Integration test: `ai-audit session info <id> --json` emits a
//! stable JSON shape with the documented field set.

use assert_cmd::Command;
use rusqlite::{params, Connection};
use serde_json::Value;
use std::collections::HashSet;
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
fn json_output_has_stable_field_set() {
    let home = tempdir().unwrap();
    let db_dir = home.path().join(".local/share/opencode");
    fs::create_dir_all(&db_dir).unwrap();
    let conn = Connection::open(db_dir.join("opencode.db")).unwrap();
    conn.pragma_update(None, "journal_mode", "WAL").unwrap();
    conn.execute_batch(OPENCODE_SCHEMA).unwrap();
    conn.execute(
        "INSERT INTO session (id, project_id, parent_id, directory, title, time_created, time_updated) \
         VALUES ('ses_json01', 'p', NULL, '/proj', 'a title', 1700000000000, 1700000010000)",
        [],
    ).unwrap();
    conn.execute(
        "INSERT INTO message (id, session_id, time_created, time_updated, data) \
         VALUES ('msg_a', 'ses_json01', 1700000001000, 1700000002000, ?1)",
        params![r#"{"role":"assistant","time":{"created":1700000001000,"completed":1700000002000},"agent":"build","providerID":"anthropic","modelID":"claude-opus-4-7"}"#],
    ).unwrap();
    conn.execute(
        "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) \
         VALUES ('prt_text', 'msg_a', 'ses_json01', 1700000001500, 1700000001500, ?1)",
        params![r#"{"type":"text","text":"hello"}"#],
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
        .args(["session", "info", "ses_json01", "--json", "--no-live"])
        .assert()
        .success();

    let stdout = String::from_utf8(output.get_output().stdout.clone()).unwrap();
    // Single object, NOT NDJSON.
    let trimmed = stdout.trim();
    assert!(
        trimmed.lines().count() == 1,
        "expected single line of JSON, got:\n{}",
        stdout
    );

    let value: Value = serde_json::from_str(trimmed).expect("valid JSON");
    let obj = value.as_object().expect("object");

    let expected_keys: HashSet<&str> = [
        "session_id",
        "provider",
        "project_dir",
        "title",
        "started_at",
        "last_updated_at",
        "message_count",
        "tool_call_count",
        "static_status",
        "live_status",
        "aborted",
        "parent_session_id",
        "agent",
        "model",
    ]
    .iter()
    .copied()
    .collect();
    let actual_keys: HashSet<&str> = obj.keys().map(|s| s.as_str()).collect();
    assert_eq!(
        actual_keys, expected_keys,
        "JSON keys drifted from contract"
    );

    // Field-value spot checks.
    assert_eq!(obj["session_id"], "ses_json01");
    assert_eq!(obj["provider"], "opencode");
    assert_eq!(obj["project_dir"], "/proj");
    assert_eq!(obj["title"], "a title");
    assert_eq!(obj["message_count"], 1);
    assert_eq!(obj["tool_call_count"], 0);
    assert_eq!(obj["static_status"], "completed");
    // --no-live → live_status is null, not omitted
    assert!(obj["live_status"].is_null());
    assert_eq!(obj["aborted"], false);
    assert!(obj["parent_session_id"].is_null());
    assert_eq!(obj["agent"], "build");
    assert_eq!(obj["model"], "anthropic/claude-opus-4-7");

    // Timestamp shape: RFC3339 UTC with trailing Z.
    let started = obj["started_at"].as_str().expect("started_at string");
    assert!(
        started.ends_with('Z'),
        "started_at not RFC3339 UTC: {}",
        started
    );
}

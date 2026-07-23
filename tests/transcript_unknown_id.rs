use assert_cmd::Command;
use rusqlite::Connection;
use std::fs;
use tempfile::tempdir;

#[test]
fn unknown_opencode_session_reports_not_found() {
    let home = tempdir().unwrap();
    let db_dir = home.path().join(".local/share/opencode");
    fs::create_dir_all(&db_dir).unwrap();
    let conn = Connection::open(db_dir.join("opencode.db")).unwrap();
    conn.pragma_update(None, "journal_mode", "WAL").unwrap();
    conn.execute_batch(
        "CREATE TABLE session (id TEXT PRIMARY KEY);\
         CREATE TABLE message (\
             id TEXT PRIMARY KEY,\
             session_id TEXT NOT NULL,\
             time_created INTEGER NOT NULL,\
             time_updated INTEGER NOT NULL,\
             data TEXT NOT NULL\
         );",
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
        .args(["session", "transcript", "ses_incomplete"])
        .assert()
        .failure();

    assert_eq!(
        String::from_utf8(output.get_output().stderr.clone()).unwrap(),
        "Error: OpenCode session not found: ses_incomplete; check that the session ID is complete and exact\n"
    );
}

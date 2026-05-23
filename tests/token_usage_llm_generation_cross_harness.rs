//! Integration test: `ai-audit token-usage -j` must populate the
//! cross-harness-uniform `llm_generation_s` field correctly per
//! harness:
//!
//! - pi + claudecode: `llm_generation_s` equals `response_wall_clock_s`
//!   (their `Message.timestamp` is the end of the assistant turn, so
//!   wall-clock IS generation time \u2014 tools execute between turns).
//! - opencode: `llm_generation_s` is `null` (the message timestamp is
//!   the start, tools are bundled inside; the clean signal requires
//!   per-part walking, tracked as a follow-up in admin.org).
//!
//! This test is the user-visible contract for the field name promise:
//! "llm_generation_s means the same thing regardless of harness, or
//! null when not derivable."  If a future refactor breaks the
//! cross-harness uniformity (e.g. someone copies `response_wall_clock_s`
//! into `llm_generation_s` for opencode without implementing
//! part-walking), this test fires.

use assert_cmd::Command;
use rusqlite::{params, Connection};
use serde_json::Value;
use std::fs;
use std::path::Path;
use tempfile::tempdir;

fn write_pi_session(home: &Path, project_dir: &str, session_id: &str, jsonl: &str) {
    let encoded = project_dir.replace('/', "-");
    let dir = home
        .join(".pi/agent/sessions")
        .join(format!("-{}-", encoded));
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("2026-05-16T09-32-14Z_{}.jsonl", session_id));
    fs::write(&path, jsonl).unwrap();
}

#[test]
fn pi_records_have_llm_generation_s_equal_to_response_wall_clock_s() {
    // pi: Message.timestamp is the end of the assistant turn, so the
    // wall-clock IS the generation time.  llm_generation_s ==
    // response_wall_clock_s for every record.
    let home = tempdir().unwrap();
    let session_id = "019dddbf-6e66-7709-9a3b-b5a18736f890"; // UUIDv7
    let project_dir = "/home/u/pi-proj";
    let jsonl = format!(
        r#"{{"type":"session","version":3,"id":"{sid}","timestamp":"2026-05-16T09:32:14Z","cwd":"{cwd}"}}
{{"type":"message","id":"u1","timestamp":"2026-05-16T09:32:20Z","message":{{"role":"user","content":[{{"type":"text","text":"please refactor"}}]}}}}
{{"type":"message","id":"a1","timestamp":"2026-05-16T09:32:25Z","message":{{"role":"assistant","provider":"openai-codex","model":"gpt-5.5","content":[{{"type":"text","text":"done"}}],"usage":{{"input":10,"output":5}}}}}}
"#,
        sid = session_id,
        cwd = project_dir,
    );
    write_pi_session(home.path(), project_dir, session_id, &jsonl);

    let output = Command::cargo_bin("ai-audit")
        .unwrap()
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path().join(".config"))
        .env("XDG_CACHE_HOME", home.path().join(".cache"))
        .env_remove("OPENCODE_SESSION_ID")
        .env_remove("CLAUDE_SESSION_ID")
        .env_remove("PI_SESSION_ID")
        .env_remove("PI_CODING_AGENT_DIR")
        .args([
            "token-usage",
            "2026-05-16T09:32:00Z..2026-05-16T09:33:00Z",
            "-j",
        ])
        .assert()
        .success();

    let stdout = String::from_utf8(output.get_output().stdout.clone()).unwrap();
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(lines.len(), 1, "expected 1 record, got:\n{}", stdout);

    let rec: Value = serde_json::from_str(lines[0]).unwrap();
    let wall = rec.get("response_wall_clock_s").and_then(|v| v.as_f64());
    let gen = rec.get("llm_generation_s").and_then(|v| v.as_f64());

    assert_eq!(
        wall,
        Some(5.0),
        "pi response_wall_clock_s should be 5.0 (25 - 20 = 5):\n{}",
        rec
    );
    assert_eq!(
        gen, wall,
        "pi llm_generation_s must equal response_wall_clock_s \
         (pi's Message.timestamp is end-of-turn):\n{}",
        rec
    );
    // The wall-clock field must explicitly carry the harness-defined
    // name; old code wrote `llm_latency_s` \u2014 a regression would put
    // it back.
    assert!(
        rec.get("llm_latency_s").is_none(),
        "field was renamed; old `llm_latency_s` must not appear:\n{}",
        rec
    );
}

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

/// Build a minimal opencode SQLite fixture with one session, one
/// user message, one assistant message carrying token data, and one
/// completed tool part inside the assistant message.
fn build_opencode_db(home: &Path) {
    let db_dir = home.join(".local/share/opencode");
    fs::create_dir_all(&db_dir).unwrap();
    let conn = Connection::open(db_dir.join("opencode.db")).unwrap();
    conn.pragma_update(None, "journal_mode", "WAL").unwrap();
    conn.execute_batch(OPENCODE_SCHEMA).unwrap();

    conn.execute(
        "INSERT INTO session (id, project_id, parent_id, directory, title, time_created, time_updated) \
         VALUES ('ses_gentest1', 'proj_1', NULL, '/home/u/oc-proj', 'gen test', 1700000000000, 1700000010000)",
        [],
    )
    .unwrap();

    // User @ 1700000001000.  The opencode transcript parser only
    // emits user-text entries from `text` PARTS, not from the
    // message-level metadata, so we add one part to make this user
    // turn visible to derive_latencies as an input marker.
    conn.execute(
        "INSERT INTO message (id, session_id, time_created, time_updated, data) \
         VALUES ('msg_u', 'ses_gentest1', 1700000001000, 1700000001000, ?1)",
        params![r#"{"role":"user","time":{"created":1700000001000}}"#],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) \
         VALUES ('prt_user_text', 'msg_u', 'ses_gentest1', 1700000001000, 1700000001000, ?1)",
        params![r#"{"type":"text","text":"please run the tool","time":{"start":1700000001000,"end":1700000001000}}"#],
    )
    .unwrap();

    // Assistant @ 1700000002000 with tokens.input/output (required
    // for token-usage to emit a record).
    conn.execute(
        "INSERT INTO message (id, session_id, time_created, time_updated, data) \
         VALUES ('msg_a', 'ses_gentest1', 1700000002000, 1700000005000, ?1)",
        params![r#"{"role":"assistant","time":{"created":1700000002000,"completed":1700000005000},"agent":"build","providerID":"anthropic","modelID":"claude-opus-4-7","tokens":{"input":100,"output":50,"cache":{"read":0,"write":0}}}"#],
    )
    .unwrap();

    // One text part + one completed tool part inside the assistant
    // message.  Note: no part.time.start — the parser will fall
    // back to msg.time.created for the tool, which is exactly the
    // production shape the zero-pair filter is designed to skip.
    conn.execute(
        "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) \
         VALUES ('prt_text', 'msg_a', 'ses_gentest1', 1700000002100, 1700000002100, ?1)",
        params![r#"{"type":"text","text":"answer"}"#],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) \
         VALUES ('prt_tool', 'msg_a', 'ses_gentest1', 1700000002200, 1700000002200, ?1)",
        params![r#"{"type":"tool","tool":"bash","state":{"status":"completed","output":"ok"}}"#],
    )
    .unwrap();
}

#[test]
fn opencode_records_have_llm_generation_s_null() {
    // opencode: Message.timestamp = time.created (message START).
    // Tools execute as parts INSIDE the message, so
    // response_wall_clock_s mixes LLM + tool time and CANNOT be
    // reused as `llm_generation_s`.  The C-phase contract is:
    // opencode `llm_generation_s` is `null` (pending part-walking).
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
        // 1700000001000 ms = 2023-11-14T22:13:21Z; span generously.
        .args([
            "token-usage",
            "2023-11-14T22:13:00Z..2023-11-14T22:14:00Z",
            "-j",
        ])
        .assert()
        .success();

    let stdout = String::from_utf8(output.get_output().stdout.clone()).unwrap();
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(
        lines.len(),
        1,
        "expected 1 record for the opencode assistant message:\n{}",
        stdout
    );

    let rec: Value = serde_json::from_str(lines[0]).unwrap();

    // Both new keys must be present.
    assert!(
        rec.get("response_wall_clock_s").is_some(),
        "record must include `response_wall_clock_s` key:\n{}",
        rec
    );
    assert!(
        rec.get("llm_generation_s").is_some(),
        "record must include `llm_generation_s` key:\n{}",
        rec
    );

    // response_wall_clock_s = 1 s (user @ 1s → assistant @ 2s).  This
    // value mixes user-input → model-started; it is NOT clean LLM
    // generation time.
    assert_eq!(
        rec.get("response_wall_clock_s").and_then(|v| v.as_f64()),
        Some(1.0),
        "opencode response_wall_clock_s should be 1.0 (user @ 1 → asst @ 2):\n{}",
        rec
    );

    // llm_generation_s must be `null` for opencode in phase C — the
    // clean signal is not yet derivable through the unified
    // TranscriptEntry API.  This is the cross-harness contract: the
    // field name carries a uniform promise, and we honour it by
    // emitting `null` rather than a misleading copy of the wall-clock.
    assert!(
        rec.get("llm_generation_s").unwrap().is_null(),
        "opencode llm_generation_s MUST be null until part-walking ships:\n{}",
        rec
    );

    // Field was renamed: the obsolete `llm_latency_s` key must not
    // reappear in a future regression.
    assert!(
        rec.get("llm_latency_s").is_none(),
        "field renamed; `llm_latency_s` must not appear:\n{}",
        rec
    );
}

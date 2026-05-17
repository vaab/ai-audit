//! Integration test: `ai-audit session info <uuidv7>` against a pi
//! JSONL fixture.

use assert_cmd::Command;
use std::fs;
use std::path::Path;
use tempfile::tempdir;

fn write_pi_session(home: &Path, project_dir: &str, session_id: &str, jsonl: &str) {
    // Pi encodes cwd as `--<encoded>--` but the actual cwd is read
    // from the header line, so any encoded folder name works.
    let encoded = project_dir.replace('/', "-");
    let dir = home
        .join(".pi/agent/sessions")
        .join(format!("-{}-", encoded));
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("2026-05-16T09-32-14Z_{}.jsonl", session_id));
    fs::write(&path, jsonl).unwrap();
}

#[test]
fn pi_session_info_reads_header_for_project_dir() {
    let home = tempdir().unwrap();
    // UUIDv7 — pi session id format (note the `7` at index 14)
    let session_id = "019dddbf-6e66-7709-9a3b-b5a18736f890";
    let project_dir = "/home/u/pi-proj";
    let jsonl = format!(
        r#"{{"type":"session","version":3,"id":"{sid}","timestamp":"2026-05-16T09:32:14Z","cwd":"{cwd}"}}
{{"type":"message","id":"u1","timestamp":"2026-05-16T09:32:20Z","message":{{"role":"user","content":[{{"type":"text","text":"hello pi"}}]}}}}
{{"type":"message","id":"a1","timestamp":"2026-05-16T09:32:30Z","message":{{"role":"assistant","provider":"openai-codex","model":"gpt-5.5","content":[{{"type":"text","text":"hi"}},{{"type":"toolCall","name":"read","arguments":{{"path":"foo.txt"}}}}],"usage":{{"input":10,"output":5}}}}}}
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
        .args(["session", "info", session_id])
        .assert()
        .success();

    let stdout = String::from_utf8(output.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains(&format!("Session:        {}", session_id)),
        "got:\n{}",
        stdout
    );
    assert!(stdout.contains("Type:           pi"), "got:\n{}", stdout);
    // Project comes from the header `cwd`, authoritative source.
    assert!(
        stdout.contains(&format!("Project:        {}", project_dir)),
        "got:\n{}",
        stdout
    );
    // 2 messages (1 user + 1 assistant), 1 tool call
    assert!(stdout.contains("Messages:       2"), "got:\n{}", stdout);
    assert!(stdout.contains("Tool calls:     1"), "got:\n{}", stdout);
    assert!(
        stdout.contains("Model:          openai-codex/gpt-5.5"),
        "got:\n{}",
        stdout
    );
    // Pi has no static or live status
    assert!(!stdout.contains("Static status:"), "got:\n{}", stdout);
    assert!(!stdout.contains("Live status:"), "got:\n{}", stdout);
}

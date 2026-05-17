//! Integration test: `ai-audit session info <uuid>` against a
//! Claude Code JSONL fixture.

use assert_cmd::Command;
use std::fs;
use std::path::Path;
use tempfile::tempdir;

fn write_claude_session(home: &Path, project_dir: &str, session_id: &str, jsonl: &str) {
    let dir = home
        .join(".claude/projects")
        .join(format!("fixture-{}", session_id));
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{}.jsonl", session_id));
    let rewritten = jsonl.replace("{{CWD}}", project_dir);
    fs::write(&path, rewritten).unwrap();
}

#[test]
fn claudecode_session_info_counts_messages_and_tool_uses() {
    let home = tempdir().unwrap();
    // UUIDv4 — Claude Code session id format
    let session_id = "4f2a1b4c-1234-4abc-9def-1234567890ab";
    let project_dir = "/home/u/proj";
    let jsonl = r#"{"type":"user","timestamp":"2026-05-16T09:32:14Z","cwd":"{{CWD}}","message":{"role":"user","content":"Hello, build a thing"}}
{"type":"assistant","timestamp":"2026-05-16T09:32:20Z","cwd":"{{CWD}}","requestId":"r1","message":{"role":"assistant","model":"claude-opus-4-7","content":[{"type":"text","text":"sure"},{"type":"tool_use","name":"Read","input":{"file_path":"src/main.rs"}}],"usage":{"input_tokens":10,"output_tokens":5}}}
{"type":"user","timestamp":"2026-05-16T09:32:30Z","cwd":"{{CWD}}","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"file content"}]}}
{"type":"assistant","timestamp":"2026-05-16T09:34:42Z","cwd":"{{CWD}}","requestId":"r2","message":{"role":"assistant","model":"claude-opus-4-7","content":[{"type":"text","text":"done"}],"usage":{"input_tokens":20,"output_tokens":3}}}
"#;
    write_claude_session(home.path(), project_dir, session_id, jsonl);

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
        .success();

    let stdout = String::from_utf8(output.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains(&format!("Session:        {}", session_id)),
        "got:\n{}",
        stdout
    );
    assert!(
        stdout.contains("Type:           claudecode"),
        "got:\n{}",
        stdout
    );
    assert!(
        stdout.contains(&format!("Project:        {}", project_dir)),
        "got:\n{}",
        stdout
    );
    // 4 entries total — user, assistant, user(tool_result), assistant
    // Our counter increments on user+assistant entries.
    assert!(stdout.contains("Messages:       4"), "got:\n{}", stdout);
    assert!(stdout.contains("Tool calls:     1"), "got:\n{}", stdout);
    // No static_status / agent / live_status for claudecode
    assert!(!stdout.contains("Static status:"), "got:\n{}", stdout);
    assert!(!stdout.contains("Live status:"), "got:\n{}", stdout);
    assert!(
        stdout.contains("Model:          claude-opus-4-7"),
        "got:\n{}",
        stdout
    );
}

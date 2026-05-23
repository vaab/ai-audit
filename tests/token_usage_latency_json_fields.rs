//! Integration test: `ai-audit token-usage <span> -j` against a pi
//! JSONL fixture must include `response_wall_clock_s`,
//! `llm_generation_s`, and `tool_latency_s_before` in the emitted
//! JSON records.
//!
//! Scope of this test (vs. the unit tests in `src/cli/action/token_usage.rs`):
//! - Unit tests cover the pure `derive_latencies` algorithm against
//!   synthetic `TranscriptEntry` vectors (basic, first-message,
//!   multi-tool, thinking-transparent, error-clipping, tool-error,
//!   negative-gap, turn-boundary).
//! - This integration test verifies the END-TO-END wire plumbing
//!   through the CLI: `parse_transcript` runs against a real pi
//!   session file, latency values appear in `--json` output under the
//!   expected keys, with the right shape (`null` when not derivable,
//!   numeric otherwise).
//! - Cross-harness uniformity of `llm_generation_s` (pi/claudecode
//!   filled, opencode null) is tested separately in
//!   `tests/token_usage_llm_generation_cross_harness.rs`.

use assert_cmd::Command;
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

/// Pi session timeline (all timestamps UTC):
///   09:32:20  user.text  ("please refactor")
///   09:32:25  assistant.text + tool_use(read)  [FIRST ASSISTANT]
///             — assistant msg @ 09:32:25, tokens(in=10,out=5)
///             — response_wall_clock_s = 5.0 (gap to user.text @ 20)
///             — llm_generation_s = 5.0 (pi: equals wall-clock)
///             — tool_latency_s_before = null (no completed tool gap
///               landed in this message's window)
///   09:32:27  toolResult(read)
///   09:32:30  assistant.text  [SECOND ASSISTANT]
///             — tokens(in=20,out=8)
///             — response_wall_clock_s = 3.0 (gap to tool_result @ 27)
///             — llm_generation_s = 3.0 (pi: equals wall-clock)
///             — tool_latency_s_before = 2.0 (read: 27 − 25)
#[test]
fn token_usage_latency_fields_present_in_json_output() {
    let home = tempdir().unwrap();
    let session_id = "019dddbf-6e66-7709-9a3b-b5a18736f890"; // UUIDv7
    let project_dir = "/home/u/pi-proj";
    let jsonl = format!(
        r#"{{"type":"session","version":3,"id":"{sid}","timestamp":"2026-05-16T09:32:14Z","cwd":"{cwd}"}}
{{"type":"message","id":"u1","timestamp":"2026-05-16T09:32:20Z","message":{{"role":"user","content":[{{"type":"text","text":"please refactor"}}]}}}}
{{"type":"message","id":"a1","timestamp":"2026-05-16T09:32:25Z","message":{{"role":"assistant","provider":"openai-codex","model":"gpt-5.5","content":[{{"type":"text","text":"reading"}},{{"type":"toolCall","id":"tc1","name":"read","arguments":{{"path":"foo.txt"}}}}],"usage":{{"input":10,"output":5}}}}}}
{{"type":"message","id":"tr1","timestamp":"2026-05-16T09:32:27Z","message":{{"role":"toolResult","toolCallId":"tc1","toolName":"read","content":[{{"type":"text","text":"file contents"}}]}}}}
{{"type":"message","id":"a2","timestamp":"2026-05-16T09:32:30Z","message":{{"role":"assistant","provider":"openai-codex","model":"gpt-5.5","content":[{{"type":"text","text":"done"}}],"usage":{{"input":20,"output":8}}}}}}
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
        // Timespan covering the fixture's two assistant messages.
        // Use absolute ISO bounds so the test is stable across clock
        // drift / future-dated fixture timestamps.
        .args([
            "token-usage",
            // kal-time uses `..` as the start/end separator.
            "2026-05-16T09:32:00Z..2026-05-16T09:33:00Z",
            "-j",
        ])
        .assert()
        .success();

    let stdout = String::from_utf8(output.get_output().stdout.clone()).unwrap();
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(
        lines.len(),
        2,
        "expected exactly 2 JSON records (one per assistant message), got:\n{}",
        stdout
    );

    let recs: Vec<Value> = lines
        .iter()
        .map(|l| serde_json::from_str::<Value>(l).expect("each line must be valid JSON"))
        .collect();

    // All records carry all three latency keys (default JSON
    // includes all fields).
    for (i, r) in recs.iter().enumerate() {
        for key in [
            "response_wall_clock_s",
            "llm_generation_s",
            "tool_latency_s_before",
        ] {
            assert!(
                r.get(key).is_some(),
                "record {} missing key `{}`:\n{}",
                i,
                key,
                r
            );
        }
        // The renamed field must not reappear.
        assert!(
            r.get("llm_latency_s").is_none(),
            "record {} must not carry obsolete `llm_latency_s`:\n{}",
            i,
            r
        );
    }

    // Records are sorted by timestamp ascending (per
    // `events.sort_by_key(|e| e.timestamp)` in token_usage::run).
    let first = &recs[0];
    let second = &recs[1];

    // First assistant message: 5.0 s after the user.text @ 20.
    assert_eq!(
        first.get("response_wall_clock_s").and_then(|v| v.as_f64()),
        Some(5.0),
        "first record response_wall_clock_s:\n{}",
        first
    );
    // pi: llm_generation_s mirrors wall-clock (Message.timestamp is
    // end-of-turn, tools execute in a separate user-role turn).
    assert_eq!(
        first.get("llm_generation_s").and_then(|v| v.as_f64()),
        Some(5.0),
        "first record llm_generation_s (pi: equals wall-clock):\n{}",
        first
    );
    // No tool gap closed in this message's before-window.
    assert!(
        first.get("tool_latency_s_before").unwrap().is_null(),
        "first record tool_latency_s_before should be null:\n{}",
        first
    );

    // Second assistant message: tool_result @ 27, asst @ 30 → 3.0.
    assert_eq!(
        second.get("response_wall_clock_s").and_then(|v| v.as_f64()),
        Some(3.0),
        "second record response_wall_clock_s:\n{}",
        second
    );
    assert_eq!(
        second.get("llm_generation_s").and_then(|v| v.as_f64()),
        Some(3.0),
        "second record llm_generation_s (pi: equals wall-clock):\n{}",
        second
    );
    // tool_use(read) @ 25, tool_result @ 27 → gap = 2.0.
    assert_eq!(
        second.get("tool_latency_s_before").and_then(|v| v.as_f64()),
        Some(2.0),
        "second record tool_latency_s_before:\n{}",
        second
    );
}

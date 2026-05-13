//! Integration tests that reproduce the opencode session-resume bugs
//! against a real `opencode serve` daemon.
//!
//! These tests are gated with `#[ignore]` because they:
//!
//! - Spawn a real opencode daemon as a subprocess (slow startup ~2s).
//! - Some tests issue a real LLM call (consuming a small number of
//!   tokens against whichever provider the user has configured).
//!
//! Run them explicitly:
//!
//! ```sh
//! cargo test --test nudge_resume_real_opencode -- --ignored --test-threads=1
//! ```
//!
//! ## Bugs under test
//!
//! 1. **CWD hijack** — opencode resolves the per-request directory
//!    from the `x-opencode-directory` header (or `?directory=` query
//!    param), falling back to `process.cwd()` of the daemon itself if
//!    neither is sent.  ai-audit's `nudge` was sending neither, so
//!    every resumed assistant message had its `path.cwd` pinned to the
//!    daemon's startup directory rather than the session's actual
//!    project directory.
//!
//! 2. **Agent override** — when `prompt_async` is called without an
//!    explicit `agent`, opencode calls `agents.defaultAgent()` which
//!    reads `config.default_agent` from the live config.  ai-audit's
//!    `nudge` was omitting `agent`, so every resumed turn ran under
//!    the daemon's configured default agent rather than the session's
//!    original agent.

mod common;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use ai_audit::opencode::nudge::{execute_plan, NudgePlan, NudgeStrategy};
use ai_audit::opencode::server_client::{LiveStatus, ServerClient};
use ai_audit::opencode::status::{ResumeModel, StaticStatus};

use common::opencode_harness::{pick_distinct_agents_from, HarnessClient, OpencodeDaemon};

/// Test 1 (no LLM): reproduce bug #2 (agent override) via the
/// `UserPending` -> `CleanResume` path.
///
/// 1. Boot opencode in `/tmp/daemon-cwd` (NOT the session's project).
/// 2. Create a session bound to `/tmp/session-proj` with a deliberate
///    "original" agent that is NOT the daemon's default.
/// 3. Post a user message with `noReply=true` so no LLM is called.
///    The session is now in the `UserPending` shape.
/// 4. Build the ai-audit `NudgePlan` exactly as today's `resolve_strategy`
///    would (CleanResume with the user message id + text) and call
///    `execute_plan`.  This is the production wire path.
/// 5. Read the session's messages and assert:
///    - The original user message has agent = "<original>"
///    - The REPLAYED user message has agent = "<original>" too (which
///      is what we want after the fix; on the broken code this would
///      be the daemon's default agent).
///
/// On the current (broken) `nudge`, this test FAILS: the replayed user
/// message has agent = <daemon-default>, not <original>.  This is the
/// canonical reproduction of bug #2.
#[test]
#[ignore]
fn nudge_user_pending_preserves_original_agent() {
    let result = run_user_pending_agent_test();
    if let Err(error) = result {
        panic!("test failed: {:#}", error);
    }
}

fn run_user_pending_agent_test() -> anyhow::Result<()> {
    // The daemon's process.cwd() is intentionally distinct from the
    // session's project directory — that distinction is what makes
    // bug #1 observable (in the assistant-message test below).
    let daemon_cwd = tempfile::Builder::new()
        .prefix("ai-audit-daemon-cwd-")
        .tempdir()?;
    let session_proj = tempfile::Builder::new()
        .prefix("ai-audit-session-proj-")
        .tempdir()?;

    eprintln!("[harness] daemon cwd  = {}", daemon_cwd.path().display());
    eprintln!("[harness] session dir = {}", session_proj.path().display());

    let daemon = OpencodeDaemon::spawn(daemon_cwd.path())?;
    eprintln!("[harness] opencode listening on {}", daemon.base_url());

    let (original_agent, daemon_default_agent) = pick_distinct_agents_from(&daemon)?;
    eprintln!("[harness] original agent       = {:?}", original_agent);
    eprintln!(
        "[harness] daemon default agent = {:?}",
        daemon_default_agent
    );
    if original_agent == daemon_default_agent {
        anyhow::bail!("harness invariant violated: original == daemon_default");
    }

    let client = HarnessClient::new(daemon.base_url());

    // Step 1: create a session bound to session_proj.
    let session = client.create_session(session_proj.path())?;
    eprintln!(
        "[harness] created session {} at {}",
        session.id, session.directory
    );
    if !paths_equal(&session.directory, session_proj.path()) {
        anyhow::bail!(
            "session.directory mismatch: created with {} but server returned {}",
            session_proj.path().display(),
            session.directory
        );
    }

    // Step 2: post the original user message with explicit agent +
    // noReply via /prompt_async, then poll until it lands.  This
    // produces UserPending without spending tokens.
    let original_text = format!(
        "Hello from agent {} in {}",
        original_agent,
        session_proj.path().display()
    );
    client.post_user_message(
        &session.id,
        session_proj.path(),
        Some(&original_agent),
        None,
        &original_text,
        /*no_reply=*/ true,
    )?;
    let messages_before = client.wait_for_messages(
        &session.id,
        session_proj.path(),
        "user",
        1,
        Duration::from_secs(10),
    )?;
    let original_user = messages_before
        .iter()
        .find(|m| m["info"]["role"].as_str() == Some("user"))
        .ok_or_else(|| anyhow::anyhow!("no user message after initial post"))?;
    let original_user_id = original_user["info"]["id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("user message has no id"))?
        .to_string();
    let original_user_agent_actual = original_user["info"]["agent"]
        .as_str()
        .unwrap_or("<missing>");
    eprintln!(
        "[harness] original user message id={}, agent={}",
        original_user_id, original_user_agent_actual
    );
    if original_user_agent_actual != original_agent {
        anyhow::bail!(
            "harness setup invalid: posted with agent={} but message has agent={}",
            original_agent,
            original_user_agent_actual
        );
    }

    // Step 3: build the NudgePlan as ai-audit's production code does
    // for a UserPending shape, and run it through execute_plan.  This
    // is the exact wire path users hit when running
    //     ai-audit session nudge <session-id>
    //
    // After Phase A2, production extracts `agent` + `model` from the
    // user message in the local SQLite DB.  Here we extract them from
    // the wire response (same data, same fields).  Post-fix, both
    // sources are used to forward the original identity to opencode.
    let original_model = match (
        original_user["info"]["model"]["providerID"].as_str(),
        original_user["info"]["model"]["modelID"].as_str(),
    ) {
        (Some(p), Some(m)) => Some(ResumeModel {
            provider_id: p.to_string(),
            model_id: m.to_string(),
        }),
        _ => None,
    };
    let plan = NudgePlan {
        session_id: session.id.clone(),
        static_status: StaticStatus::UserPending,
        live_status: LiveStatus::Idle,
        project_dir: session_proj.path().to_string_lossy().into_owned(),
        orphan: false,
        forced: false,
        strategy: NudgeStrategy::CleanResume {
            user_msg_id: original_user_id.clone(),
            text: original_text.clone(),
            agent: Some(original_agent.clone()),
            model: original_model,
        },
        fork_first: false,
    };
    let server = ServerClient::new(daemon.server_credentials());
    let outcomes = execute_plan(&[plan], &server, /*concurrency=*/ 1);
    eprintln!("[harness] outcomes = {:?}", outcomes);
    for outcome in &outcomes {
        if let Err(error) = &outcome.result {
            anyhow::bail!("execute_plan failed for {}: {}", outcome.session_id, error);
        }
    }

    // Step 4: nudge's CleanResume reverts to the user message then
    // re-fires the same text.  After revert.cleanup() runs at the
    // start of the new prompt, the previous user message is gone and
    // a NEW user message with a fresh ID is created with the same
    // text.  Wait for any user message bearing original_text to land,
    // distinct from original_user_id.
    let start = std::time::Instant::now();
    let replayed = loop {
        let messages_after = client.get_messages(&session.id, session_proj.path())?;
        let candidates = messages_after
            .iter()
            .filter(|m| m["info"]["role"].as_str() == Some("user"))
            .cloned()
            .collect::<Vec<_>>();
        let replayed = candidates.iter().rev().find(|m| {
            let id = m["info"]["id"].as_str().unwrap_or("");
            let text = collect_text_parts(m);
            id != original_user_id && text == original_text
        });
        if let Some(replayed) = replayed {
            break replayed.clone();
        }
        if start.elapsed() > Duration::from_secs(20) {
            anyhow::bail!(
                "timed out waiting for replayed user message; current messages: {:#?}",
                candidates
            );
        }
        std::thread::sleep(Duration::from_millis(150));
    };
    let replayed = &replayed;
    let replayed_agent = replayed["info"]["agent"].as_str().unwrap_or("<missing>");
    let replayed_text = collect_text_parts(replayed);
    eprintln!(
        "[harness] replayed user message: agent={}, text={:?}",
        replayed_agent, replayed_text
    );

    // The text must match the original — this is the whole point of
    // CleanResume.
    if replayed_text != original_text {
        anyhow::bail!(
            "replayed text mismatch: expected {:?}, got {:?}",
            original_text,
            replayed_text
        );
    }

    // The replayed agent MUST match the original.  On the broken
    // pre-fix code, this assertion fails because opencode falls back to
    // the daemon's default_agent when the prompt_async body omits the
    // `agent` field.
    if replayed_agent != original_agent {
        anyhow::bail!(
            "BUG #2 reproduced: replayed user message has agent {:?}, expected {:?} \
             (opencode's defaultAgent() fallback fired because ai-audit's nudge omitted \
             the `agent` field in the prompt_async body)",
            replayed_agent,
            original_agent
        );
    }

    eprintln!("[harness] PASS: replayed user message preserved original agent");
    Ok(())
}

/// Test 2 (real LLM, slow, advisory): reproduce bug #1 (cwd hijack)
/// AND bug #2 (agent override) together by letting the resumed turn
/// produce an assistant message and inspecting its `path.cwd` and
/// `agent` fields.
///
/// **Why this test is advisory and not authoritative**: bug #1 (cwd
/// header) is already authoritatively covered by the mock-based unit
/// test `server_client::tests::nudge_204_is_ok_and_sends_directory_header`,
/// which asserts the exact wire payload.  This integration test
/// additionally exercises opencode's _consumption_ of that header end
/// to end, but it depends on the user's local LLM provider config,
/// the agent's permission rules, and provider latency — so it can be
/// flaky in environments where the chosen primary agent has heavy
/// startup or restrictive permissions.  Run when you want
/// end-to-end confidence; otherwise rely on test 1 plus the unit
/// tests.
///
/// Workflow:
///   1. Spawn opencode in `/tmp/daemon-cwd` (intentionally distinct).
///   2. Create a session in `/tmp/session-proj` with agent <original>.
///   3. Post a user message with `noReply=true` (no LLM call).
///   4. Run ai-audit's nudge.  After it lands, the resumed turn
///      produces an assistant message — its `path.cwd` and `agent`
///      reveal both bugs.
///
/// Asserts both bugs are gone after the fix.  Pre-fix, the new
/// assistant message would show daemon cwd + daemon default agent.
#[test]
#[ignore]
fn nudge_resumed_turn_preserves_cwd_and_agent() {
    let result = run_resumed_turn_test();
    if let Err(error) = result {
        panic!("test failed: {:#}", error);
    }
}

fn run_resumed_turn_test() -> anyhow::Result<()> {
    let daemon_cwd = tempfile::Builder::new()
        .prefix("ai-audit-daemon-cwd-")
        .tempdir()?;
    let session_proj = tempfile::Builder::new()
        .prefix("ai-audit-session-proj-")
        .tempdir()?;

    eprintln!("[harness] daemon cwd  = {}", daemon_cwd.path().display());
    eprintln!("[harness] session dir = {}", session_proj.path().display());

    let daemon = OpencodeDaemon::spawn(daemon_cwd.path())?;
    eprintln!("[harness] opencode listening on {}", daemon.base_url());

    let (original_agent, daemon_default_agent) = pick_distinct_agents_from(&daemon)?;
    eprintln!("[harness] original agent       = {:?}", original_agent);
    eprintln!(
        "[harness] daemon default agent = {:?}",
        daemon_default_agent
    );

    let client = HarnessClient::new(daemon.base_url());
    let session = client.create_session(session_proj.path())?;
    eprintln!("[harness] session {} created", session.id);

    // Put the session in UserPending shape: post a user message with
    // noReply=true (no LLM call yet) so the resumed turn — produced
    // by ai-audit's nudge — is the FIRST one to hit the LLM.  That
    // way we observe exactly the cwd + agent ai-audit's wire payload
    // produced, with no confounding earlier turns.
    let resume_text = "Reply with exactly the single word: resumed";
    client.post_user_message(
        &session.id,
        session_proj.path(),
        Some(&original_agent),
        None,
        resume_text,
        /*no_reply=*/ true,
    )?;
    let pre_resume = client.wait_for_messages(
        &session.id,
        session_proj.path(),
        "user",
        1,
        Duration::from_secs(10),
    )?;
    let last_user = pre_resume
        .iter()
        .rev()
        .find(|m| m["info"]["role"].as_str() == Some("user"))
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("no user message after post"))?;
    let user_msg_id = last_user["info"]["id"].as_str().unwrap().to_string();

    let original_model = match (
        last_user["info"]["model"]["providerID"].as_str(),
        last_user["info"]["model"]["modelID"].as_str(),
    ) {
        (Some(p), Some(m)) => Some(ResumeModel {
            provider_id: p.to_string(),
            model_id: m.to_string(),
        }),
        _ => None,
    };
    let plan = NudgePlan {
        session_id: session.id.clone(),
        static_status: StaticStatus::UserPending,
        live_status: LiveStatus::Idle,
        project_dir: session_proj.path().to_string_lossy().into_owned(),
        orphan: false,
        forced: false,
        strategy: NudgeStrategy::CleanResume {
            user_msg_id,
            text: resume_text.to_string(),
            agent: Some(original_agent.clone()),
            model: original_model,
        },
        fork_first: false,
    };
    let server = ServerClient::new(daemon.server_credentials());
    let outcomes = execute_plan(&[plan], &server, 1);
    eprintln!("[harness] outcomes = {:?}", outcomes);
    for outcome in &outcomes {
        if let Err(error) = &outcome.result {
            anyhow::bail!("execute_plan failed: {}", error);
        }
    }

    // Wait for the resumed assistant turn (real LLM call) to
    // produce its first text part.  Generous timeout — some agents
    // have heavy boot (skills, plugins).  If this times out, the
    // user's LLM provider config likely needs attention.
    let post_resume = client
        .wait_for_message_with_part(
            &session.id,
            session_proj.path(),
            "assistant",
            "text",
            Duration::from_secs(120),
        )
        .map_err(|e| anyhow::anyhow!("{e}; daemon logs:\n{}", daemon.drain_logs().join("\n")))?;
    let resumed_assistant = post_resume
        .iter()
        .filter(|m| m["info"]["role"].as_str() == Some("assistant"))
        .next_back()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("nudge produced no assistant message"))?;
    let resumed_cwd = resumed_assistant["info"]["path"]["cwd"]
        .as_str()
        .unwrap_or("<missing>");
    let resumed_agent = resumed_assistant["info"]["agent"]
        .as_str()
        .unwrap_or("<missing>");
    eprintln!(
        "[harness] resumed assistant: cwd={}, agent={}",
        resumed_cwd, resumed_agent
    );

    let mut bugs = Vec::new();
    if !paths_equal(resumed_cwd, session_proj.path()) {
        bugs.push(format!(
            "BUG #1 (cwd hijack): resumed assistant has cwd={} but expected {} \
             (opencode fell back to its own process.cwd() because ai-audit omitted \
             the x-opencode-directory header)",
            resumed_cwd,
            session_proj.path().display()
        ));
    }
    if resumed_agent != original_agent {
        bugs.push(format!(
            "BUG #2 (agent override): resumed assistant has agent={} but expected {} \
             (opencode's defaultAgent() fallback fired because ai-audit omitted the \
             `agent` field in the prompt_async body)",
            resumed_agent, original_agent
        ));
    }
    if !bugs.is_empty() {
        anyhow::bail!(
            "{} bug(s) reproduced:\n  - {}",
            bugs.len(),
            bugs.join("\n  - ")
        );
    }

    eprintln!("[harness] PASS: resumed turn preserved cwd and agent");
    Ok(())
}

/// Test 3 (no LLM, `--fork` semantics): when `fork_first=true`, the
/// nudge must NOT touch the original session — it forks first and
/// applies the strategy to the fork.  Asserts:
///
///   1. The original session's user message is unchanged after nudge
///      (same message id, same agent, same text).
///   2. A new session id appears in the outcome (the fork id).
///   3. The fork has the original history plus the replayed user
///      message with the correct agent.
#[test]
#[ignore]
fn nudge_fork_first_leaves_original_untouched() {
    let result = run_fork_first_test();
    if let Err(error) = result {
        panic!("test failed: {:#}", error);
    }
}

fn run_fork_first_test() -> anyhow::Result<()> {
    let daemon_cwd = tempfile::Builder::new()
        .prefix("ai-audit-daemon-cwd-")
        .tempdir()?;
    let session_proj = tempfile::Builder::new()
        .prefix("ai-audit-session-proj-")
        .tempdir()?;

    eprintln!("[harness] daemon cwd  = {}", daemon_cwd.path().display());
    eprintln!("[harness] session dir = {}", session_proj.path().display());

    let daemon = OpencodeDaemon::spawn(daemon_cwd.path())?;
    eprintln!("[harness] opencode listening on {}", daemon.base_url());

    let (original_agent, daemon_default_agent) = pick_distinct_agents_from(&daemon)?;
    eprintln!("[harness] original agent       = {:?}", original_agent);
    eprintln!(
        "[harness] daemon default agent = {:?}",
        daemon_default_agent
    );

    let client = HarnessClient::new(daemon.base_url());
    let session = client.create_session(session_proj.path())?;
    eprintln!("[harness] created session {}", session.id);

    // UserPending shape: post a user message with agent=<original>,
    // noReply=true.
    let original_text = "Original prompt that should remain on the original session";
    client.post_user_message(
        &session.id,
        session_proj.path(),
        Some(&original_agent),
        None,
        original_text,
        /*no_reply=*/ true,
    )?;
    let pre = client.wait_for_messages(
        &session.id,
        session_proj.path(),
        "user",
        1,
        Duration::from_secs(10),
    )?;
    let original_user = pre
        .iter()
        .find(|m| m["info"]["role"].as_str() == Some("user"))
        .ok_or_else(|| anyhow::anyhow!("no user message"))?
        .clone();
    let original_user_id = original_user["info"]["id"].as_str().unwrap().to_string();

    // Build a plan with fork_first=true.
    let plan = NudgePlan {
        session_id: session.id.clone(),
        static_status: StaticStatus::UserPending,
        live_status: LiveStatus::Idle,
        project_dir: session_proj.path().to_string_lossy().into_owned(),
        orphan: false,
        forced: false,
        strategy: NudgeStrategy::CleanResume {
            user_msg_id: original_user_id.clone(),
            text: original_text.to_string(),
            agent: Some(original_agent.clone()),
            model: None,
        },
        fork_first: true,
    };
    let server = ServerClient::new(daemon.server_credentials());
    let outcomes = execute_plan(&[plan], &server, 1);
    eprintln!("[harness] outcomes = {:?}", outcomes);
    for outcome in &outcomes {
        if let Err(error) = &outcome.result {
            anyhow::bail!("execute_plan failed: {}", error);
        }
    }

    let fork_id = outcomes
        .iter()
        .find(|o| o.session_id == session.id)
        .and_then(|o| o.fork_id.clone())
        .ok_or_else(|| anyhow::anyhow!("outcome did not carry a fork_id"))?;
    eprintln!("[harness] fork id = {}", fork_id);
    if fork_id == session.id {
        anyhow::bail!("fork id == original id; fork did not happen");
    }

    // The ORIGINAL session must be unchanged.  Same one user
    // message, same id, same agent, same text.
    let original_after = client.get_messages(&session.id, session_proj.path())?;
    let user_messages: Vec<_> = original_after
        .iter()
        .filter(|m| m["info"]["role"].as_str() == Some("user"))
        .collect();
    if user_messages.len() != 1 {
        anyhow::bail!(
            "expected original session to have exactly 1 user message after fork nudge, got {}: {:#?}",
            user_messages.len(),
            user_messages
        );
    }
    let preserved = user_messages[0];
    if preserved["info"]["id"].as_str() != Some(&original_user_id) {
        anyhow::bail!(
            "original user message id changed: was {:?}, now {:?}",
            original_user_id,
            preserved["info"]["id"]
        );
    }
    if preserved["info"]["agent"].as_str() != Some(&original_agent) {
        anyhow::bail!(
            "original user message agent changed: was {:?}, now {:?}",
            original_agent,
            preserved["info"]["agent"]
        );
    }

    // The FORK has its own user message — same text, possibly new id —
    // and the resumed agent must match the original.
    let fork_msgs = client.wait_for_messages(
        &fork_id,
        session_proj.path(),
        "user",
        1,
        Duration::from_secs(15),
    )?;
    let fork_user = fork_msgs
        .iter()
        .rev()
        .find(|m| {
            m["info"]["role"].as_str() == Some("user") && collect_text_parts(m) == original_text
        })
        .ok_or_else(|| {
            anyhow::anyhow!(
                "fork {} has no replayed user message with the original text; messages: {:#?}",
                fork_id,
                fork_msgs
            )
        })?;
    let fork_user_agent = fork_user["info"]["agent"].as_str().unwrap_or("<missing>");
    if fork_user_agent != original_agent {
        anyhow::bail!(
            "fork user message has agent={:?}, expected {:?}",
            fork_user_agent,
            original_agent
        );
    }

    eprintln!(
        "[harness] PASS: original session untouched; fork has replayed turn with correct agent"
    );
    Ok(())
}

// -- helpers --------------------------------------------------------

#[allow(dead_code)]
fn find_role<'a>(messages: &'a [Value], role: &str) -> Option<&'a Value> {
    messages
        .iter()
        .find(|m| m["info"]["role"].as_str() == Some(role))
}

fn collect_text_parts(message: &Value) -> String {
    let parts = match message["parts"].as_array() {
        Some(arr) => arr,
        None => return String::new(),
    };
    parts
        .iter()
        .filter(|p| p["type"].as_str() == Some("text"))
        .filter_map(|p| p["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

fn paths_equal(a: &str, b: impl Into<PathBuf>) -> bool {
    let a = std::path::Path::new(a).canonicalize();
    let b = b.into().canonicalize();
    match (a, b) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

// Silence unused-import lint when only Arc happens to disappear during
// a refactor.  Keeping the use line lets future extensions of the
// harness reach for Arc without re-importing.
#[allow(dead_code)]
fn _arc_used<T>(_: Arc<T>) {}

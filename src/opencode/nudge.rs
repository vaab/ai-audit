use std::sync::Arc;

use anyhow::Result;

use super::server_client::LiveStatus;
use super::server_client::ServerClient;
use super::status::{ResumeModel, StaticStatus};

/// How a single session is nudged. Selected per-shape:
///
/// * `CleanResume`: revert to the existing user message, then re-fire
///   it verbatim (`prompt_async` with the same text). Used for shapes
///   where there is a user message to replay and the broken assistant
///   tail is best deleted (`UserPending`, `AssistantEmpty`).
/// * `ContinuePrompt`: append a new user message containing the
///   continue-prompt. Used for shapes where partial assistant output
///   is real work the user wants to keep (`AssistantPartial`,
///   `AssistantToolStuck`).
///
/// Both variants carry an optional `agent` + `model` extracted from
/// the session's recent user message.  They are forwarded to opencode
/// in the `prompt_async` body so the resumed turn does NOT fall back
/// to the daemon's `default_agent`/default model.
#[derive(Debug, Clone)]
pub enum NudgeStrategy {
    CleanResume {
        /// Existing user message ID — sent as the revert cutoff.
        user_msg_id: String,
        /// Text payload of the existing user message — replayed verbatim.
        text: String,
        /// Original agent name (forwarded in prompt_async).
        agent: Option<String>,
        /// Original provider/model (forwarded in prompt_async).
        model: Option<ResumeModel>,
    },
    ContinuePrompt {
        /// The prompt text (default "continue") to append.
        prompt: String,
        /// Original agent name of the user message preceding the
        /// broken assistant turn (forwarded in prompt_async).
        agent: Option<String>,
        /// Original provider/model of that user message.
        model: Option<ResumeModel>,
    },
}

impl NudgeStrategy {
    /// Original agent name to forward to opencode's `prompt_async`,
    /// if known.  Both variants carry an `Option<String>` so this is
    /// just a uniform accessor.
    #[allow(dead_code)]
    pub fn agent(&self) -> Option<&str> {
        match self {
            Self::CleanResume { agent, .. } | Self::ContinuePrompt { agent, .. } => {
                agent.as_deref()
            }
        }
    }

    /// Original model (provider+id) to forward to opencode's
    /// `prompt_async`, if known.
    #[allow(dead_code)]
    pub fn model(&self) -> Option<&ResumeModel> {
        match self {
            Self::CleanResume { model, .. } | Self::ContinuePrompt { model, .. } => model.as_ref(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct NudgePlan {
    pub session_id: String,
    pub static_status: StaticStatus,
    pub live_status: LiveStatus,
    pub project_dir: String,
    pub orphan: bool,
    pub forced: bool,
    pub strategy: NudgeStrategy,
    /// When true, fork the session first and apply the strategy to
    /// the fork instead of the original.  Sets `outcome.fork_id` to
    /// the new fork's session id so the caller can surface it.
    pub fork_first: bool,
}

#[derive(Debug, Clone)]
pub struct NudgeOutcome {
    pub session_id: String,
    /// New fork session id, if `plan.fork_first` was true and the
    /// fork was created successfully.
    pub fork_id: Option<String>,
    pub result: Result<(), String>,
}

/// Execute one nudge plan against the server.
///
/// All paths first call `/abort` to guarantee the session is idle —
/// without this, a stale in-flight LLM call (a session whose SSE
/// connection died) would collide with the resumed turn and either
/// reject the revert (HTTP 400 BusyError) or produce phantom
/// duplicate output.
///
/// For `CleanResume`: POST `/abort`, POST `/revert` with the user
/// message ID, then POST `/prompt_async` with the original text +
/// the original agent + the original model.  All three must succeed
/// (revert's `assertNotBusy` will pass because we just aborted).
///
/// For `ContinuePrompt`: POST `/abort`, then POST `/prompt_async`
/// with the continue-text + the original agent + the original model.
///
/// In both cases every request also carries the
/// `x-opencode-directory` header so opencode does not fall back to
/// its own `process.cwd()`.
///
/// When `plan.fork_first` is true, this calls `/fork` first to
/// create an independent fork of the session, then runs the strategy
/// against the FORK.  The original session is left untouched.  The
/// fork's id is returned via the outer `NudgeOutcome.fork_id`.
fn execute_one(client: &ServerClient, plan: &NudgePlan) -> Result<Option<String>> {
    // Determine which session the strategy operates on.  When
    // forking, the abort + revert + prompt_async all target the
    // fork, NOT the original — that is the whole point of --fork.
    let (target_session, fork_id) = if plan.fork_first {
        let fork_message_id = match &plan.strategy {
            // CleanResume: fork BEFORE the user message that would
            // be revert-replayed.  That way the fork inherits all
            // history up to (not including) the user message; we'll
            // then re-fire that user message text on the fork.
            NudgeStrategy::CleanResume { user_msg_id, .. } => Some(user_msg_id.as_str()),
            // ContinuePrompt: fork at the head (no message_id).
            // The fork inherits the full session including the
            // broken assistant tail; we then post the continue
            // prompt to drive a new resumed turn.
            NudgeStrategy::ContinuePrompt { .. } => None,
        };
        let id = client.fork(&plan.session_id, &plan.project_dir, fork_message_id)?;
        (id.clone(), Some(id))
    } else {
        (plan.session_id.clone(), None)
    };

    // Always abort first.  Idempotent — returns ok when the session
    // is already idle.  Blocks until the in-flight fiber (if any) is
    // actually interrupted, so subsequent revert won't hit BusyError.
    client.abort(&target_session, &plan.project_dir)?;

    match &plan.strategy {
        NudgeStrategy::CleanResume {
            user_msg_id,
            text,
            agent,
            model,
        } => {
            if !plan.fork_first {
                // On the original session, revert the user message
                // so its broken-stub tail is cleared before replay.
                client.revert(&target_session, &plan.project_dir, user_msg_id)?;
            }
            // On a fork, the user message we'd revert is already
            // absent — fork() copied messages strictly before it —
            // so a revert is unnecessary.  Just replay the text.
            client.nudge(
                &target_session,
                &plan.project_dir,
                text,
                agent.as_deref(),
                model.as_ref(),
            )?;
            Ok(fork_id)
        }
        NudgeStrategy::ContinuePrompt {
            prompt,
            agent,
            model,
        } => {
            client.nudge(
                &target_session,
                &plan.project_dir,
                prompt,
                agent.as_deref(),
                model.as_ref(),
            )?;
            Ok(fork_id)
        }
    }
}

pub fn execute_plan(
    plans: &[NudgePlan],
    client: &ServerClient,
    concurrency: usize,
) -> Vec<NudgeOutcome> {
    let client = Arc::new(client.clone());
    let width = concurrency.max(1);
    let mut outcomes = Vec::new();

    for chunk in plans.chunks(width) {
        let mut handles = Vec::new();
        for plan in chunk.iter().cloned() {
            let client = Arc::clone(&client);
            handles.push(std::thread::spawn(move || {
                let session_id = plan.session_id.clone();
                match execute_one(&client, &plan) {
                    Ok(fork_id) => NudgeOutcome {
                        session_id,
                        fork_id,
                        result: Ok(()),
                    },
                    Err(error) => NudgeOutcome {
                        session_id,
                        fork_id: None,
                        result: Err(error.to_string()),
                    },
                }
            }));
        }
        outcomes.extend(handles.into_iter().filter_map(|handle| handle.join().ok()));
    }

    outcomes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::opencode::server_client::ServerCredentials;
    use httpmock::Method::POST;
    use httpmock::MockServer;

    fn make_plan(session_id: &str, strategy: NudgeStrategy) -> NudgePlan {
        NudgePlan {
            session_id: session_id.to_string(),
            static_status: StaticStatus::UserPending,
            live_status: LiveStatus::Idle,
            project_dir: "/tmp/p".to_string(),
            orphan: false,
            forced: false,
            strategy,
            fork_first: false,
        }
    }

    #[test]
    fn strategy_accessors_work() {
        let strategy = NudgeStrategy::CleanResume {
            user_msg_id: "msg_1".into(),
            text: "do stuff".into(),
            agent: Some("conductor".into()),
            model: Some(ResumeModel {
                provider_id: "anthropic".into(),
                model_id: "claude-opus-4-5".into(),
            }),
        };
        assert_eq!(strategy.agent(), Some("conductor"));
        assert_eq!(
            strategy.model().map(|m| m.provider_id.as_str()),
            Some("anthropic")
        );

        let strategy = NudgeStrategy::ContinuePrompt {
            prompt: "continue".into(),
            agent: None,
            model: None,
        };
        assert_eq!(strategy.agent(), None);
        assert!(strategy.model().is_none());
    }

    #[test]
    fn execute_one_clean_resume_calls_abort_then_revert_then_prompt_async() {
        let server = MockServer::start();

        let abort_mock = server.mock(|when, then| {
            when.method(POST)
                .path("/session/ses_1/abort")
                .header("x-opencode-directory", "/tmp/p");
            then.status(200).body("true");
        });
        let revert_mock = server.mock(|when, then| {
            when.method(POST)
                .path("/session/ses_1/revert")
                .header("x-opencode-directory", "/tmp/p")
                .json_body(serde_json::json!({ "messageID": "msg_user" }));
            then.status(200).body("{}");
        });
        let prompt_mock = server.mock(|when, then| {
            when.method(POST)
                .path("/session/ses_1/prompt_async")
                .header("x-opencode-directory", "/tmp/p")
                .json_body(serde_json::json!({
                    "parts": [{ "type": "text", "text": "do the original thing" }],
                    "agent": "conductor",
                    "model": { "providerID": "anthropic", "modelID": "claude-opus-4-5" },
                }));
            then.status(204);
        });

        let client = ServerClient::new(ServerCredentials {
            url: server.url(""),
            password: None,
        });
        let plan = make_plan(
            "ses_1",
            NudgeStrategy::CleanResume {
                user_msg_id: "msg_user".to_string(),
                text: "do the original thing".to_string(),
                agent: Some("conductor".to_string()),
                model: Some(ResumeModel {
                    provider_id: "anthropic".to_string(),
                    model_id: "claude-opus-4-5".to_string(),
                }),
            },
        );

        execute_one(&client, &plan).unwrap();

        abort_mock.assert();
        revert_mock.assert();
        prompt_mock.assert();
    }

    #[test]
    fn execute_one_clean_resume_aborts_when_revert_fails() {
        // If revert fails (e.g. session not found), we must NOT POST
        // prompt_async — the session would otherwise get a "do the
        // thing" message appended without the broken assistant tail
        // being cleared.
        let server = MockServer::start();

        let abort_mock = server.mock(|when, then| {
            when.method(POST).path("/session/ses_1/abort");
            then.status(200).body("true");
        });
        let revert_mock = server.mock(|when, then| {
            when.method(POST).path("/session/ses_1/revert");
            then.status(500).body("boom");
        });
        let prompt_mock = server.mock(|when, then| {
            when.method(POST).path("/session/ses_1/prompt_async");
            then.status(204);
        });

        let client = ServerClient::new(ServerCredentials {
            url: server.url(""),
            password: None,
        });
        let plan = make_plan(
            "ses_1",
            NudgeStrategy::CleanResume {
                user_msg_id: "msg_user".to_string(),
                text: "do thing".to_string(),
                agent: None,
                model: None,
            },
        );

        let error = execute_one(&client, &plan).unwrap_err().to_string();
        assert!(error.contains("boom"), "got: {error}");

        abort_mock.assert();
        revert_mock.assert(); // revert was tried
                              // prompt_async must NOT have been called.
        assert_eq!(
            prompt_mock.hits(),
            0,
            "prompt_async called after revert failed"
        );
    }

    #[test]
    fn execute_one_continue_prompt_calls_abort_then_prompt_async() {
        let server = MockServer::start();

        let abort_mock = server.mock(|when, then| {
            when.method(POST)
                .path("/session/ses_1/abort")
                .header("x-opencode-directory", "/tmp/p");
            then.status(200).body("true");
        });
        let revert_mock = server.mock(|when, then| {
            when.method(POST).path("/session/ses_1/revert");
            then.status(200).body("{}");
        });
        let prompt_mock = server.mock(|when, then| {
            when.method(POST)
                .path("/session/ses_1/prompt_async")
                .header("x-opencode-directory", "/tmp/p")
                .json_body(serde_json::json!({
                    "parts": [{ "type": "text", "text": "continue" }],
                    "agent": "conductor",
                }));
            then.status(204);
        });

        let client = ServerClient::new(ServerCredentials {
            url: server.url(""),
            password: None,
        });
        let plan = make_plan(
            "ses_1",
            NudgeStrategy::ContinuePrompt {
                prompt: "continue".to_string(),
                agent: Some("conductor".to_string()),
                model: None,
            },
        );

        execute_one(&client, &plan).unwrap();

        abort_mock.assert();
        // Revert MUST NOT have been called for ContinuePrompt.
        assert_eq!(
            revert_mock.hits(),
            0,
            "revert was called for ContinuePrompt strategy"
        );
        prompt_mock.assert();
    }

    #[test]
    fn execute_one_abort_failure_aborts_the_nudge() {
        // If abort itself fails (e.g. server returned 500 on the
        // abort endpoint), we must NOT proceed with revert/prompt.
        // Otherwise the in-flight call would still be running and
        // bug #3 would manifest.
        let server = MockServer::start();
        let abort_mock = server.mock(|when, then| {
            when.method(POST).path("/session/ses_1/abort");
            then.status(500).body("abort failed");
        });
        let revert_mock = server.mock(|when, then| {
            when.method(POST).path("/session/ses_1/revert");
            then.status(200).body("{}");
        });
        let prompt_mock = server.mock(|when, then| {
            when.method(POST).path("/session/ses_1/prompt_async");
            then.status(204);
        });

        let client = ServerClient::new(ServerCredentials {
            url: server.url(""),
            password: None,
        });
        let plan = make_plan(
            "ses_1",
            NudgeStrategy::ContinuePrompt {
                prompt: "continue".into(),
                agent: None,
                model: None,
            },
        );

        let error = execute_one(&client, &plan).unwrap_err().to_string();
        assert!(error.contains("abort failed"), "got: {error}");

        abort_mock.assert();
        assert_eq!(revert_mock.hits(), 0);
        assert_eq!(prompt_mock.hits(), 0);
    }
}

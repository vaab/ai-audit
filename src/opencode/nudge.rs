use std::sync::Arc;

use anyhow::Result;

use super::server_client::LiveStatus;
use super::server_client::ServerClient;
use super::status::StaticStatus;

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
#[derive(Debug, Clone)]
pub enum NudgeStrategy {
    CleanResume {
        /// Existing user message ID — sent as the revert cutoff.
        user_msg_id: String,
        /// Text payload of the existing user message — replayed verbatim.
        text: String,
    },
    ContinuePrompt {
        /// The prompt text (default "continue") to append.
        prompt: String,
    },
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
}

#[derive(Debug, Clone)]
pub struct NudgeOutcome {
    pub session_id: String,
    pub result: Result<(), String>,
}

/// Execute one nudge plan against the server.
///
/// For `CleanResume`: POST `/revert` with the user message ID, then
/// POST `/prompt_async` with the same text payload. Both must succeed.
/// For `ContinuePrompt`: POST `/prompt_async` with the prompt text.
fn execute_one(client: &ServerClient, plan: &NudgePlan) -> Result<()> {
    match &plan.strategy {
        NudgeStrategy::CleanResume { user_msg_id, text } => {
            client.revert(&plan.session_id, user_msg_id)?;
            client.nudge(&plan.session_id, text)?;
            Ok(())
        }
        NudgeStrategy::ContinuePrompt { prompt } => {
            client.nudge(&plan.session_id, prompt)?;
            Ok(())
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
            handles.push(std::thread::spawn(move || NudgeOutcome {
                session_id: plan.session_id.clone(),
                result: execute_one(&client, &plan).map_err(|error| error.to_string()),
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
        }
    }

    #[test]
    fn strategy_dispatch() {
        // Sanity: matching exhaustively over both variants compiles.
        let strategies = [
            NudgeStrategy::CleanResume {
                user_msg_id: "msg_1".into(),
                text: "do stuff".into(),
            },
            NudgeStrategy::ContinuePrompt {
                prompt: "continue".into(),
            },
        ];
        for strategy in strategies {
            match strategy {
                NudgeStrategy::CleanResume { user_msg_id, text } => {
                    assert_eq!(user_msg_id, "msg_1");
                    assert_eq!(text, "do stuff");
                }
                NudgeStrategy::ContinuePrompt { prompt } => {
                    assert_eq!(prompt, "continue");
                }
            }
        }
    }

    #[test]
    fn execute_one_clean_resume_calls_revert_then_prompt_async() {
        let server = MockServer::start();

        let revert_mock = server.mock(|when, then| {
            when.method(POST)
                .path("/session/ses_1/revert")
                .json_body(serde_json::json!({ "messageID": "msg_user" }));
            then.status(200).body("{}");
        });
        let prompt_mock = server.mock(|when, then| {
            when.method(POST)
                .path("/session/ses_1/prompt_async")
                .json_body(serde_json::json!({
                    "parts": [{ "type": "text", "text": "do the original thing" }],
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
            },
        );

        execute_one(&client, &plan).unwrap();

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
            },
        );

        let error = execute_one(&client, &plan).unwrap_err().to_string();
        assert!(error.contains("boom"), "got: {error}");

        revert_mock.assert(); // revert was tried
                              // prompt_async must NOT have been called.
        assert_eq!(
            prompt_mock.hits(),
            0,
            "prompt_async called after revert failed"
        );
    }

    #[test]
    fn execute_one_continue_prompt_only_calls_prompt_async() {
        let server = MockServer::start();

        let revert_mock = server.mock(|when, then| {
            when.method(POST).path("/session/ses_1/revert");
            then.status(200).body("{}");
        });
        let prompt_mock = server.mock(|when, then| {
            when.method(POST)
                .path("/session/ses_1/prompt_async")
                .json_body(serde_json::json!({
                    "parts": [{ "type": "text", "text": "continue" }],
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
            },
        );

        execute_one(&client, &plan).unwrap();

        // Revert MUST NOT have been called for ContinuePrompt.
        assert_eq!(
            revert_mock.hits(),
            0,
            "revert was called for ContinuePrompt strategy"
        );
        prompt_mock.assert();
    }
}

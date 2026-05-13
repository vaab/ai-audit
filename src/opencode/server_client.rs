use anyhow::{anyhow, Context, Result};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use reqwest::blocking::Client;
use reqwest::header::{HeaderName, AUTHORIZATION, CONTENT_TYPE};
use serde::Deserialize;
use std::collections::HashMap;

use super::config::OpencodeConfig;
use super::status::ResumeModel;

/// Header name opencode uses to resolve the per-request "working
/// directory" for a session.  Sending this on EVERY request that
/// touches a session is mandatory — without it, opencode falls back
/// to `process.cwd()` of the daemon, which corrupts the
/// `path.cwd` field stamped onto resumed assistant messages.
const X_OPENCODE_DIRECTORY: HeaderName = HeaderName::from_static("x-opencode-directory");

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerCredentials {
    pub url: String,
    pub password: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ServerClient {
    base_url: String,
    client: Client,
    auth_header: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveStatus {
    Running,
    Idle,
    ServerUnreachable,
}

impl LiveStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Idle => "idle",
            Self::ServerUnreachable => "server-unreachable",
        }
    }

    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "running" => Ok(Self::Running),
            "idle" => Ok(Self::Idle),
            "server-unreachable" => Ok(Self::ServerUnreachable),
            _ => Err(anyhow!(
                "invalid status; valid live values: running, idle, server-unreachable"
            )),
        }
    }

    pub fn all() -> Vec<Self> {
        vec![Self::Running, Self::Idle, Self::ServerUnreachable]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerBusyKind {
    Busy,
    Retry,
    Fallback,
}

#[derive(Deserialize)]
struct StatusPayload {
    #[serde(rename = "type")]
    kind: String,
}

impl ServerClient {
    pub fn new(creds: ServerCredentials) -> Self {
        let auth_header = creds
            .password
            .map(|password| format!("Basic {}", BASE64.encode(format!("opencode:{password}"))));
        Self {
            base_url: creds.url.trim_end_matches('/').to_string(),
            client: Client::new(),
            auth_header,
        }
    }

    pub fn session_status(&self) -> Result<Option<HashMap<String, ServerBusyKind>>> {
        let url = format!("{}/session/status", self.base_url);
        log::trace!("GET {}", url);
        // /session/status is project-agnostic and is consumed by the
        // bulk nudge command which acts across many directories.  We
        // do not need (and cannot meaningfully provide) a single
        // x-opencode-directory header here.
        let request = self.with_auth(self.client.get(&url));
        let response = match request.send() {
            Ok(response) => response,
            Err(error) if error.status().is_none() => return Ok(None),
            Err(error) => return Err(error).with_context(|| format!("GET {} failed", url)),
        };

        let status = response.status();
        let body = response.text().unwrap_or_default();
        if !status.is_success() {
            return Err(anyhow!("GET {} failed: {} {}", url, status, body.trim()));
        }

        let payload: HashMap<String, StatusPayload> = serde_json::from_str(&body)
            .with_context(|| format!("Failed to parse {} response", url))?;

        Ok(Some(
            payload
                .into_iter()
                .map(|(session_id, status)| {
                    let kind = match status.kind.as_str() {
                        "busy" => ServerBusyKind::Busy,
                        "retry" => ServerBusyKind::Retry,
                        "fallback" => ServerBusyKind::Fallback,
                        "idle" => ServerBusyKind::Busy,
                        other => {
                            log::warn!(
                                "Unknown opencode session status {:?}; treating as running",
                                other
                            );
                            ServerBusyKind::Busy
                        }
                    };
                    (session_id, kind)
                })
                .collect(),
        ))
    }

    /// Post a user message to `/session/<id>/prompt_async`, optionally
    /// forwarding the session's original `agent` and `model` so the
    /// resumed turn keeps the session's identity.
    ///
    /// **`directory` is mandatory** — opencode resolves the
    /// per-request working directory from this header.  Pass the
    /// session's stored project directory.
    ///
    /// **`agent` is strongly recommended** — without it, opencode
    /// falls back to its `default_agent` config, which is the wrong
    /// agent for any session that was driven by a different agent.
    pub fn nudge(
        &self,
        session_id: &str,
        directory: &str,
        prompt: &str,
        agent: Option<&str>,
        model: Option<&ResumeModel>,
    ) -> Result<()> {
        let url = format!("{}/session/{}/prompt_async", self.base_url, session_id);
        log::trace!(
            "POST {} (directory={:?} agent={:?} model={:?})",
            url,
            directory,
            agent,
            model
        );

        let mut body = serde_json::json!({
            "parts": [{ "type": "text", "text": prompt }],
        });
        if let Some(agent) = agent {
            body["agent"] = serde_json::Value::String(agent.to_string());
        }
        if let Some(model) = model {
            body["model"] = serde_json::json!({
                "providerID": model.provider_id,
                "modelID": model.model_id,
            });
        }

        let response = self
            .with_auth(
                self.client
                    .post(&url)
                    .header(CONTENT_TYPE, "application/json")
                    .header(X_OPENCODE_DIRECTORY, directory)
                    .json(&body),
            )
            .send()
            .with_context(|| format!("POST {} failed", url))?;

        if response.status() == reqwest::StatusCode::NO_CONTENT {
            return Ok(());
        }

        let status = response.status();
        let body = response.text().unwrap_or_default();
        Err(anyhow!("POST {} failed: {} {}", url, status, body.trim()))
    }

    /// Mark `message_id` as the revert cutoff. The next `nudge` (or
    /// `prompt_async`) on this session will delete the message at and
    /// after that cutoff before starting a new turn.
    ///
    /// This mirrors the TUI's "revert" action. Used to clean-resume
    /// stalled sessions whose last assistant message is an empty stub
    /// (or whose last message is the original user turn we want to
    /// re-fire identically).
    ///
    /// Expected response: HTTP 200 with the updated session JSON
    /// (which we discard).
    ///
    /// **`directory` is mandatory** — see `nudge()` above.
    pub fn revert(&self, session_id: &str, directory: &str, message_id: &str) -> Result<()> {
        let url = format!("{}/session/{}/revert", self.base_url, session_id);
        log::trace!(
            "POST {} (directory={:?} messageID={})",
            url,
            directory,
            message_id
        );
        let response = self
            .with_auth(
                self.client
                    .post(&url)
                    .header(CONTENT_TYPE, "application/json")
                    .header(X_OPENCODE_DIRECTORY, directory)
                    .json(&serde_json::json!({
                        "messageID": message_id,
                    })),
            )
            .send()
            .with_context(|| format!("POST {} failed", url))?;

        let status = response.status();
        if status.is_success() {
            return Ok(());
        }

        let body = response.text().unwrap_or_default();
        Err(anyhow!("POST {} failed: {} {}", url, status, body.trim()))
    }

    /// Fork a session at (optionally) a specific message id.
    ///
    /// Opencode's `POST /session/<id>/fork` creates a brand-new
    /// session (new id) populated with a deep copy of all messages
    /// strictly before `message_id` (or the full history if
    /// `message_id` is None).  Message and part IDs are remapped to
    /// fresh IDs inside the fork — it is a true clone, not a parent
    /// reference.
    ///
    /// **`directory` is mandatory** — fork inherits its `directory`
    /// from `InstanceState.directory` resolved from the request, NOT
    /// from the original session.  Always pass the session's project
    /// directory.
    ///
    /// Returns the new session id on success.
    pub fn fork(
        &self,
        session_id: &str,
        directory: &str,
        message_id: Option<&str>,
    ) -> Result<String> {
        let url = format!("{}/session/{}/fork", self.base_url, session_id);
        log::trace!(
            "POST {} (directory={:?} messageID={:?})",
            url,
            directory,
            message_id
        );
        let mut body = serde_json::Map::new();
        if let Some(message_id) = message_id {
            body.insert(
                "messageID".to_string(),
                serde_json::Value::String(message_id.to_string()),
            );
        }
        let response = self
            .with_auth(
                self.client
                    .post(&url)
                    .header(CONTENT_TYPE, "application/json")
                    .header(X_OPENCODE_DIRECTORY, directory)
                    .json(&serde_json::Value::Object(body)),
            )
            .send()
            .with_context(|| format!("POST {} failed", url))?;

        let status = response.status();
        let text = response.text().unwrap_or_default();
        if !status.is_success() {
            return Err(anyhow!("POST {} failed: {} {}", url, status, text.trim()));
        }
        let value: serde_json::Value = serde_json::from_str(&text)
            .with_context(|| format!("parsing /fork response: {text}"))?;
        let fork_id = value["id"]
            .as_str()
            .ok_or_else(|| anyhow!("/fork response missing `id`: {value}"))?;
        Ok(fork_id.to_string())
    }

    /// Cancel any in-flight LLM call on `session_id`.
    ///
    /// Idempotent: if the session is already idle, this returns
    /// successfully without effect.  If it is busy, opencode
    /// interrupts the running fiber via `Fiber.interrupt` and waits
    /// for it to finish (so when this call returns, the session is
    /// guaranteed to be idle).
    ///
    /// ai-audit's `nudge` always calls `abort` before
    /// `revert`/`prompt_async` so that no stale in-flight call can
    /// collide with the resumed turn.
    pub fn abort(&self, session_id: &str, directory: &str) -> Result<()> {
        let url = format!("{}/session/{}/abort", self.base_url, session_id);
        log::trace!("POST {} (directory={:?})", url, directory);
        let response = self
            .with_auth(
                self.client
                    .post(&url)
                    .header(CONTENT_TYPE, "application/json")
                    .header(X_OPENCODE_DIRECTORY, directory),
            )
            .send()
            .with_context(|| format!("POST {} failed", url))?;

        let status = response.status();
        if status.is_success() {
            return Ok(());
        }

        let body = response.text().unwrap_or_default();
        Err(anyhow!("POST {} failed: {} {}", url, status, body.trim()))
    }

    fn with_auth(
        &self,
        request: reqwest::blocking::RequestBuilder,
    ) -> reqwest::blocking::RequestBuilder {
        if let Some(header) = self.auth_header.as_ref() {
            return request.header(AUTHORIZATION, header);
        }
        request
    }
}

pub fn compute_live(
    session_id: &str,
    map: Option<&HashMap<String, ServerBusyKind>>,
    server_unreachable: bool,
) -> LiveStatus {
    if server_unreachable {
        return LiveStatus::ServerUnreachable;
    }
    let map = map.expect("compute_live requires a fetched session-status map");
    if map.contains_key(session_id) {
        return LiveStatus::Running;
    }
    LiveStatus::Idle
}

pub fn resolve_server_credentials(
    cli_url: Option<&str>,
    cli_password: Option<&str>,
    config: &crate::config::Config,
) -> ServerCredentials {
    let opencode = OpencodeConfig::from_generic(config);
    let url = cli_url
        .map(String::from)
        .or_else(|| std::env::var("OPENCODE_SERVER_URL").ok())
        .or(opencode.server.url)
        .unwrap_or_else(|| "http://127.0.0.1:4096".to_string());
    let password = cli_password
        .map(String::from)
        .or_else(|| std::env::var("OPENCODE_SERVER_PASSWORD").ok())
        .or(opencode.server.password);
    ServerCredentials { url, password }
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::Method::{GET, POST};
    use httpmock::MockServer;
    use indoc::indoc;

    fn test_config(yaml: &str) -> crate::config::Config {
        serde_yaml::from_str(yaml).unwrap()
    }

    #[test]
    fn compute_live_running() {
        let map = HashMap::from([("ses_1".to_string(), ServerBusyKind::Busy)]);
        assert_eq!(
            compute_live("ses_1", Some(&map), false),
            LiveStatus::Running
        );
    }

    #[test]
    fn compute_live_idle() {
        let map = HashMap::new();
        assert_eq!(compute_live("ses_1", Some(&map), false), LiveStatus::Idle);
    }

    #[test]
    fn compute_live_server_unreachable() {
        assert_eq!(
            compute_live("ses_1", None, true),
            LiveStatus::ServerUnreachable
        );
    }

    #[test]
    #[should_panic(expected = "compute_live requires a fetched session-status map")]
    fn compute_live_rejects_missing_map() {
        let _ = compute_live("ses_1", None, false);
    }

    #[test]
    fn session_status_parses_map() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/session/status");
            then.status(200).body(indoc! {r#"
                {
                  "ses_a": {"type": "busy"},
                  "ses_b": {"type": "retry", "attempt": 2},
                  "ses_c": {"type": "fallback", "from": "a", "to": "b"}
                }
            "#});
        });

        let client = ServerClient::new(ServerCredentials {
            url: server.url(""),
            password: None,
        });
        let map = client.session_status().unwrap().unwrap();

        mock.assert();
        assert_eq!(map.get("ses_a"), Some(&ServerBusyKind::Busy));
        assert_eq!(map.get("ses_b"), Some(&ServerBusyKind::Retry));
        assert_eq!(map.get("ses_c"), Some(&ServerBusyKind::Fallback));
    }

    #[test]
    fn session_status_connection_refused_is_none() {
        let client = ServerClient::new(ServerCredentials {
            url: "http://127.0.0.1:9".to_string(),
            password: None,
        });
        assert!(client.session_status().unwrap().is_none());
    }

    #[test]
    fn session_status_401_is_error() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/session/status");
            then.status(401).body("nope");
        });

        let client = ServerClient::new(ServerCredentials {
            url: server.url(""),
            password: None,
        });
        let error = client.session_status().unwrap_err().to_string();
        assert!(error.contains("401"));
    }

    #[test]
    fn nudge_204_is_ok_and_sends_directory_header() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/session/ses_1/prompt_async")
                .header("x-opencode-directory", "/tmp/proj-a")
                .json_body(serde_json::json!({
                    "parts": [{ "type": "text", "text": "continue" }],
                }));
            then.status(204);
        });

        let client = ServerClient::new(ServerCredentials {
            url: server.url(""),
            password: None,
        });
        client
            .nudge("ses_1", "/tmp/proj-a", "continue", None, None)
            .unwrap();
        mock.assert();
    }

    #[test]
    fn nudge_includes_agent_when_provided() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/session/ses_1/prompt_async")
                .header("x-opencode-directory", "/p")
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
        client
            .nudge("ses_1", "/p", "continue", Some("conductor"), None)
            .unwrap();
        mock.assert();
    }

    #[test]
    fn nudge_includes_agent_and_model_when_provided() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/session/ses_1/prompt_async")
                .header("x-opencode-directory", "/p")
                .json_body(serde_json::json!({
                    "parts": [{ "type": "text", "text": "go" }],
                    "agent": "conductor",
                    "model": { "providerID": "anthropic", "modelID": "claude-opus-4-5" },
                }));
            then.status(204);
        });

        let client = ServerClient::new(ServerCredentials {
            url: server.url(""),
            password: None,
        });
        client
            .nudge(
                "ses_1",
                "/p",
                "go",
                Some("conductor"),
                Some(&ResumeModel {
                    provider_id: "anthropic".to_string(),
                    model_id: "claude-opus-4-5".to_string(),
                }),
            )
            .unwrap();
        mock.assert();
    }

    #[test]
    fn nudge_error_includes_body() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/session/ses_1/prompt_async");
            then.status(500).body("boom");
        });

        let client = ServerClient::new(ServerCredentials {
            url: server.url(""),
            password: None,
        });
        let error = client
            .nudge("ses_1", "/p", "continue", None, None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("boom"));
    }

    #[test]
    fn nudge_sends_authorization_header() {
        let server = MockServer::start();
        let header = format!("Basic {}", BASE64.encode("opencode:testpw"));
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/session/ses_1/prompt_async")
                .header("authorization", &header);
            then.status(204);
        });

        let client = ServerClient::new(ServerCredentials {
            url: server.url(""),
            password: Some("testpw".to_string()),
        });
        client.nudge("ses_1", "/p", "continue", None, None).unwrap();
        mock.assert();
    }

    #[test]
    fn revert_200_is_ok_and_sends_directory_header() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/session/ses_1/revert")
                .header("x-opencode-directory", "/tmp/proj-a")
                .json_body(serde_json::json!({ "messageID": "msg_42" }));
            then.status(200).body("{}");
        });

        let client = ServerClient::new(ServerCredentials {
            url: server.url(""),
            password: None,
        });
        client.revert("ses_1", "/tmp/proj-a", "msg_42").unwrap();
        mock.assert();
    }

    #[test]
    fn revert_error_includes_body() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/session/ses_1/revert");
            then.status(404).body("not found");
        });

        let client = ServerClient::new(ServerCredentials {
            url: server.url(""),
            password: None,
        });
        let error = client
            .revert("ses_1", "/p", "msg_42")
            .unwrap_err()
            .to_string();
        assert!(error.contains("not found"), "got: {error}");
        assert!(error.contains("404"), "got: {error}");
    }

    #[test]
    fn revert_sends_authorization_header() {
        let server = MockServer::start();
        let header = format!("Basic {}", BASE64.encode("opencode:secret"));
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/session/ses_1/revert")
                .header("authorization", &header);
            then.status(200).body("{}");
        });

        let client = ServerClient::new(ServerCredentials {
            url: server.url(""),
            password: Some("secret".to_string()),
        });
        client.revert("ses_1", "/p", "msg_42").unwrap();
        mock.assert();
    }

    #[test]
    fn fork_returns_new_session_id_and_sends_directory_header() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/session/ses_orig/fork")
                .header("x-opencode-directory", "/p")
                .json_body(serde_json::json!({ "messageID": "msg_42" }));
            then.status(200)
                .body(r#"{"id":"ses_fork","directory":"/p","title":"orig (fork #1)"}"#);
        });

        let client = ServerClient::new(ServerCredentials {
            url: server.url(""),
            password: None,
        });
        let fork_id = client.fork("ses_orig", "/p", Some("msg_42")).unwrap();
        assert_eq!(fork_id, "ses_fork");
        mock.assert();
    }

    #[test]
    fn fork_without_message_id_sends_empty_body() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/session/ses_orig/fork")
                .header("x-opencode-directory", "/p")
                .json_body(serde_json::json!({}));
            then.status(200).body(r#"{"id":"ses_fork"}"#);
        });

        let client = ServerClient::new(ServerCredentials {
            url: server.url(""),
            password: None,
        });
        client.fork("ses_orig", "/p", None).unwrap();
        mock.assert();
    }

    #[test]
    fn fork_error_includes_body() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/session/ses_orig/fork");
            then.status(500).body("forked sideways");
        });

        let client = ServerClient::new(ServerCredentials {
            url: server.url(""),
            password: None,
        });
        let error = client.fork("ses_orig", "/p", None).unwrap_err().to_string();
        assert!(error.contains("forked sideways"), "got: {error}");
    }

    #[test]
    fn abort_200_is_ok_and_sends_directory_header() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/session/ses_1/abort")
                .header("x-opencode-directory", "/p");
            then.status(200).body("true");
        });

        let client = ServerClient::new(ServerCredentials {
            url: server.url(""),
            password: None,
        });
        client.abort("ses_1", "/p").unwrap();
        mock.assert();
    }

    #[test]
    fn abort_error_includes_body() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/session/ses_1/abort");
            then.status(500).body("boom");
        });

        let client = ServerClient::new(ServerCredentials {
            url: server.url(""),
            password: None,
        });
        let error = client.abort("ses_1", "/p").unwrap_err().to_string();
        assert!(error.contains("boom"));
    }

    #[test]
    fn resolve_server_credentials_priority() {
        let config = test_config(indoc! {"
            opencode-server:
              url: http://config
              password: configpw
        "});
        unsafe {
            std::env::set_var("OPENCODE_SERVER_URL", "http://env");
            std::env::set_var("OPENCODE_SERVER_PASSWORD", "envpw");
        }
        let resolved = resolve_server_credentials(Some("http://cli"), Some("clipw"), &config);
        unsafe {
            std::env::remove_var("OPENCODE_SERVER_URL");
            std::env::remove_var("OPENCODE_SERVER_PASSWORD");
        }
        assert_eq!(resolved.url, "http://cli");
        assert_eq!(resolved.password.as_deref(), Some("clipw"));
    }

    #[test]
    fn resolve_server_credentials_defaults() {
        let config = crate::config::Config::default();
        let resolved = resolve_server_credentials(None, None, &config);
        assert_eq!(resolved.url, "http://127.0.0.1:4096");
        assert!(resolved.password.is_none());
    }
}

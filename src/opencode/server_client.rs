use anyhow::{anyhow, Context, Result};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use reqwest::blocking::Client;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use serde::Deserialize;
use std::collections::HashMap;

use super::config::OpencodeConfig;

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

    pub fn nudge(&self, session_id: &str, prompt: &str) -> Result<()> {
        let url = format!("{}/session/{}/prompt_async", self.base_url, session_id);
        log::trace!("POST {}", url);
        let response = self
            .with_auth(
                self.client
                    .post(&url)
                    .header(CONTENT_TYPE, "application/json")
                    .json(&serde_json::json!({
                        "parts": [{ "type": "text", "text": prompt }],
                    })),
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
    pub fn revert(&self, session_id: &str, message_id: &str) -> Result<()> {
        let url = format!("{}/session/{}/revert", self.base_url, session_id);
        log::trace!("POST {} (messageID={})", url, message_id);
        let response = self
            .with_auth(
                self.client
                    .post(&url)
                    .header(CONTENT_TYPE, "application/json")
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
    fn nudge_204_is_ok() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
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
        client.nudge("ses_1", "continue").unwrap();
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
        let error = client.nudge("ses_1", "continue").unwrap_err().to_string();
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
        client.nudge("ses_1", "continue").unwrap();
        mock.assert();
    }

    #[test]
    fn revert_200_is_ok() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/session/ses_1/revert")
                .json_body(serde_json::json!({ "messageID": "msg_42" }));
            then.status(200).body("{}");
        });

        let client = ServerClient::new(ServerCredentials {
            url: server.url(""),
            password: None,
        });
        client.revert("ses_1", "msg_42").unwrap();
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
        let error = client.revert("ses_1", "msg_42").unwrap_err().to_string();
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
        client.revert("ses_1", "msg_42").unwrap();
        mock.assert();
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

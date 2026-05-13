//! Spawn a real `opencode serve` daemon for integration tests.
//!
//! The harness inherits the user's opencode auth/config automatically
//! (opencode reads `~/.config/opencode/opencode.json` and friends on
//! startup).  No credential plumbing is required — the same providers
//! the user normally uses are available to the test.
//!
//! ## What this harness is for
//!
//! These tests drive a real opencode daemon over its HTTP API so we can
//! reproduce real wire-level bugs:
//!
//! 1. CWD hijack (opencode falls back to `process.cwd()` of the daemon
//!    when no `x-opencode-directory` header is sent)
//! 2. Agent override (opencode calls `defaultAgent()` when `agent` is
//!    omitted from the prompt body, ignoring the session's history)
//! 3. Duplicate streams (opencode keeps in-flight LLM calls alive on
//!    SSE disconnect; no broken-pipe abort)
//!
//! The harness lets tests:
//!
//! - boot a fresh `opencode serve --port 0` per test (hermetic)
//! - create sessions in arbitrary directories
//! - post user messages with explicit `agent`/`model` and `noReply` to
//!   produce `UserPending` shapes without spending LLM tokens
//! - post real streaming prompts and abort mid-stream to produce
//!   `AssistantPartial` shapes (real LLM, ~3s of streaming, opt-in via
//!   `#[ignore]`)
//! - read back the session + messages to assert the wire behavior
//!
//! Drop kills the process and removes the workdir.

use anyhow::{anyhow, bail, Context, Result};
use reqwest::blocking::Client;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, CONTENT_TYPE};
use serde::Deserialize;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use tempfile::TempDir;

const READY_TIMEOUT: Duration = Duration::from_secs(15);

/// A running `opencode serve` instance, owned for the lifetime of the
/// test.  Drop terminates the process.
pub struct OpencodeDaemon {
    child: Option<Child>,
    base_url: String,
    /// Owned tempdir kept alive so its drop happens AFTER the child is
    /// killed.  Field is intentionally not read directly.
    #[allow(dead_code)]
    workdir: TempDir,
    /// Channel that yields any post-ready log lines from the child's
    /// stdout (for diagnostics on test failure).
    #[allow(dead_code)]
    log_rx: mpsc::Receiver<String>,
}

impl OpencodeDaemon {
    /// Drain any buffered daemon stdout/stderr lines.  Useful in
    /// test panics: include this in the assertion message to see
    /// what the daemon actually said while the test was running.
    pub fn drain_logs(&self) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(line) = self.log_rx.try_recv() {
            out.push(line);
        }
        out
    }

    /// Spawn a fresh opencode daemon listening on an ephemeral port.
    ///
    /// `daemon_cwd` controls the daemon's own `process.cwd()` — which
    /// is intentionally distinct from the directories of any sessions
    /// the test will create, so we can detect bug #1 (cwd hijack).
    pub fn spawn(daemon_cwd: &Path) -> Result<Self> {
        // Confirm the binary exists and is on PATH — otherwise the
        // failure mode is a cryptic ENOENT from Command::spawn().
        which::which("opencode").context(
            "`opencode` binary not found on PATH — install opencode before running integration tests",
        )?;

        let workdir = tempfile::Builder::new()
            .prefix("ai-audit-opencode-harness-")
            .tempdir()
            .context("creating opencode harness workdir")?;

        let mut child = Command::new("opencode")
            .arg("serve")
            .arg("--port")
            .arg("0")
            .arg("--hostname")
            .arg("127.0.0.1")
            .arg("--print-logs")
            .current_dir(daemon_cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("spawning `opencode serve`")?;

        // Read stdout AND stderr in parallel — opencode logs go to
        // both.  Look for the "listening on http://..." line.
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("no stdout pipe on opencode child"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("no stderr pipe on opencode child"))?;

        let (url_tx, url_rx) = mpsc::channel::<Result<String, String>>();
        let (log_tx, log_rx) = mpsc::channel::<String>();

        spawn_log_reader("opencode/stdout", stdout, url_tx.clone(), log_tx.clone());
        spawn_log_reader("opencode/stderr", stderr, url_tx, log_tx);

        let base_url = match url_rx.recv_timeout(READY_TIMEOUT) {
            Ok(Ok(url)) => url,
            Ok(Err(msg)) => {
                let _ = child.kill();
                bail!("opencode serve failed to come up: {}", msg);
            }
            Err(_) => {
                let _ = child.kill();
                bail!(
                    "timed out waiting {:?} for `opencode serve` to print its listen URL",
                    READY_TIMEOUT
                );
            }
        };

        // Quick TCP probe to confirm the server is actually
        // responding.  We retry a few times because in practice
        // there can be a short delay between the "listening" log
        // line and the HTTP socket actually accepting connections.
        let probe_url = format!("{}/session", base_url);
        let mut last_error: Option<reqwest::Error> = None;
        let probe_client = Client::new();
        let mut probed = false;
        for _ in 0..10 {
            match probe_client
                .get(&probe_url)
                .timeout(Duration::from_secs(3))
                .send()
            {
                Ok(_) => {
                    probed = true;
                    break;
                }
                Err(error) => {
                    last_error = Some(error);
                    thread::sleep(Duration::from_millis(250));
                }
            }
        }
        if !probed {
            let _ = child.kill();
            let detail = last_error
                .map(|e| e.to_string())
                .unwrap_or_else(|| "unknown error".to_string());
            bail!(
                "opencode serve advertised {} but probe to {} failed after retries: {}",
                base_url,
                probe_url,
                detail
            );
        }

        Ok(Self {
            child: Some(child),
            base_url,
            workdir,
            log_rx,
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Build a `ServerCredentials` pointing at this daemon.
    pub fn server_credentials(&self) -> ai_audit::opencode::server_client::ServerCredentials {
        ai_audit::opencode::server_client::ServerCredentials {
            url: self.base_url.clone(),
            password: None,
        }
    }
}

impl Drop for OpencodeDaemon {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn spawn_log_reader<R: std::io::Read + Send + 'static>(
    name: &'static str,
    reader: R,
    url_tx: mpsc::Sender<Result<String, String>>,
    log_tx: mpsc::Sender<String>,
) {
    let buf = BufReader::new(reader);
    thread::spawn(move || {
        for line in buf.lines() {
            let Ok(line) = line else { break };
            // Diagnostic: also push every line to the log channel so
            // failing tests can surface what opencode actually said.
            let _ = log_tx.send(format!("[{name}] {line}"));

            // Primary signal: "opencode server listening on http://..."
            if let Some(idx) = line.find("listening on http://") {
                let url = line[idx + "listening on ".len()..].trim().to_string();
                let _ = url_tx.send(Ok(url));
            }
        }
    });
}

/// HTTP client driving the spawned opencode daemon.
///
/// Sends `x-opencode-directory` on every request — without it,
/// opencode falls back to its own `process.cwd()`, which corrupts
/// every session field that is derived from the request's directory.
///
/// Several methods are not yet exercised by every test; they're kept
/// available for future test cases (assistant-partial mid-stream
/// abort, fork verification, etc).
#[allow(dead_code)]
pub struct HarnessClient {
    base_url: String,
    client: Client,
}

#[allow(dead_code)]
impl HarnessClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            client: Client::new(),
        }
    }

    fn directory_headers(directory: &Path) -> HeaderMap {
        let mut headers = HeaderMap::new();
        let value = directory.to_string_lossy().into_owned();
        if let Ok(header) = HeaderValue::from_str(&value) {
            headers.insert(HeaderName::from_static("x-opencode-directory"), header);
        }
        headers
    }

    pub fn create_session(&self, directory: &Path) -> Result<SessionInfo> {
        let url = format!("{}/session", self.base_url);
        let response = self
            .client
            .post(&url)
            .headers(Self::directory_headers(directory))
            .header(CONTENT_TYPE, "application/json")
            .json(&json!({}))
            .send()
            .with_context(|| format!("POST {} (create session)", url))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().unwrap_or_default();
            bail!("create_session failed: {} {}", status, body.trim());
        }

        let info: SessionInfo = response.json().context("parsing session info")?;
        Ok(info)
    }

    pub fn get_session(&self, session_id: &str, directory: &Path) -> Result<SessionInfo> {
        let url = format!("{}/session/{}", self.base_url, session_id);
        let response = self
            .client
            .get(&url)
            .headers(Self::directory_headers(directory))
            .send()
            .with_context(|| format!("GET {}", url))?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().unwrap_or_default();
            bail!("get_session failed: {} {}", status, body.trim());
        }
        let info: SessionInfo = response.json().context("parsing session info")?;
        Ok(info)
    }

    /// Post a user message via `/prompt_async`.
    ///
    /// `/prompt_async` is fire-and-forget: it returns HTTP 204
    /// immediately and runs `prompt()` in a void-promise.  After this
    /// call returns, the caller should poll [`wait_for_user_message`]
    /// or similar to see the message materialize.
    ///
    /// We do NOT use `/message` for fixtures because it streams via
    /// Hono with `void stream.write(...)`, which can drop the body
    /// when `noReply=true` returns immediately (we observed empty
    /// 200 responses in practice).
    ///
    /// `no_reply=true` makes opencode create the user message without
    /// streaming an assistant turn — useful for producing the
    /// `UserPending` shape without consuming LLM tokens.
    pub fn post_user_message(
        &self,
        session_id: &str,
        directory: &Path,
        agent: Option<&str>,
        model: Option<&Model>,
        text: &str,
        no_reply: bool,
    ) -> Result<()> {
        let url = format!("{}/session/{}/prompt_async", self.base_url, session_id);
        let mut body = json!({
            "parts": [{ "type": "text", "text": text }],
            "noReply": no_reply,
        });
        if let Some(agent) = agent {
            body["agent"] = json!(agent);
        }
        if let Some(model) = model {
            body["model"] = json!({
                "providerID": model.provider_id,
                "modelID": model.model_id,
            });
        }
        let response = self
            .client
            .post(&url)
            .headers(Self::directory_headers(directory))
            .header(CONTENT_TYPE, "application/json")
            .json(&body)
            .send()
            .with_context(|| format!("POST {}", url))?;
        if response.status() != reqwest::StatusCode::NO_CONTENT {
            let status = response.status();
            let body = response.text().unwrap_or_default();
            bail!(
                "post_user_message expected 204, got {} {}",
                status,
                body.trim()
            );
        }
        Ok(())
    }

    /// Poll `/session/<id>/message` until at least `min_count`
    /// messages of role `role` exist (or until `timeout` elapses).
    /// Returns the full message list at that moment.
    pub fn wait_for_messages(
        &self,
        session_id: &str,
        directory: &Path,
        role: &str,
        min_count: usize,
        timeout: Duration,
    ) -> Result<Vec<Value>> {
        let start = Instant::now();
        loop {
            let messages = self.get_messages(session_id, directory)?;
            let count = messages
                .iter()
                .filter(|m| m["info"]["role"].as_str() == Some(role))
                .count();
            if count >= min_count {
                return Ok(messages);
            }
            if start.elapsed() > timeout {
                bail!(
                    "timed out waiting {:?} for {} message(s) of role={} on session {} (found {})",
                    timeout,
                    min_count,
                    role,
                    session_id,
                    count
                );
            }
            thread::sleep(Duration::from_millis(150));
        }
    }

    /// Poll until a message of role `role` exists AND has at least one
    /// part of type `part_type`.  Useful for waiting on the assistant
    /// turn to actually produce content (not just an empty stub).
    pub fn wait_for_message_with_part(
        &self,
        session_id: &str,
        directory: &Path,
        role: &str,
        part_type: &str,
        timeout: Duration,
    ) -> Result<Vec<Value>> {
        let start = Instant::now();
        loop {
            let messages = self.get_messages(session_id, directory)?;
            let has_match = messages.iter().any(|m| {
                m["info"]["role"].as_str() == Some(role)
                    && m["parts"]
                        .as_array()
                        .map(|parts| parts.iter().any(|p| p["type"].as_str() == Some(part_type)))
                        .unwrap_or(false)
            });
            if has_match {
                return Ok(messages);
            }
            if start.elapsed() > timeout {
                bail!(
                    "timed out waiting {:?} for role={} message with part type={} on session {}",
                    timeout,
                    role,
                    part_type,
                    session_id
                );
            }
            thread::sleep(Duration::from_millis(150));
        }
    }

    pub fn abort(&self, session_id: &str, directory: &Path) -> Result<()> {
        let url = format!("{}/session/{}/abort", self.base_url, session_id);
        let response = self
            .client
            .post(&url)
            .headers(Self::directory_headers(directory))
            .header(CONTENT_TYPE, "application/json")
            .send()
            .with_context(|| format!("POST {}", url))?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().unwrap_or_default();
            bail!("abort failed: {} {}", status, body.trim());
        }
        Ok(())
    }

    pub fn get_messages(&self, session_id: &str, directory: &Path) -> Result<Vec<Value>> {
        let url = format!("{}/session/{}/message", self.base_url, session_id);
        let response = self
            .client
            .get(&url)
            .headers(Self::directory_headers(directory))
            .send()
            .with_context(|| format!("GET {}", url))?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().unwrap_or_default();
            bail!("get_messages failed: {} {}", status, body.trim());
        }
        let value: Value = response.json().context("parsing messages")?;
        // The endpoint returns an array of { info: { ... }, parts: [...] }.
        let arr = value
            .as_array()
            .ok_or_else(|| anyhow!("expected array of messages, got {}", value))?
            .clone();
        Ok(arr)
    }

    pub fn session_status(&self, directory: &Path) -> Result<Value> {
        let url = format!("{}/session/status", self.base_url);
        let response = self
            .client
            .get(&url)
            .headers(Self::directory_headers(directory))
            .send()
            .with_context(|| format!("GET {}", url))?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().unwrap_or_default();
            bail!("session_status failed: {} {}", status, body.trim());
        }
        let value: Value = response.json().context("parsing status")?;
        Ok(value)
    }

    /// Wait until a session's status flips from busy to idle, or until
    /// `timeout` elapses.  Returns the final status value.
    pub fn wait_until_idle(
        &self,
        session_id: &str,
        directory: &Path,
        timeout: Duration,
    ) -> Result<Value> {
        let start = Instant::now();
        loop {
            let status = self.session_status(directory)?;
            let busy = status.get(session_id).is_some();
            if !busy {
                return Ok(status);
            }
            if start.elapsed() > timeout {
                bail!(
                    "timed out waiting {:?} for session {} to go idle (status={})",
                    timeout,
                    session_id,
                    status
                );
            }
            thread::sleep(Duration::from_millis(150));
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct SessionInfo {
    pub id: String,
    pub directory: String,
    #[serde(default)]
    pub title: String,
}

#[derive(Debug, Clone)]
pub struct Model {
    pub provider_id: String,
    pub model_id: String,
}

/// Discover two agents that will let us reliably expose bug #2 on
/// this opencode daemon:
///
///   - `original_agent`: a primary agent the test will use when
///     creating the session and posting the first user message.
///   - `daemon_default_agent`: the agent opencode falls back to when
///     `prompt_async` is called WITHOUT an `agent` field.  This is
///     what ai-audit's broken `nudge` resumes under.
///
/// **The two MUST be different** — otherwise the daemon-default
/// fallback would silently agree with the original and bug #2 would
/// look "fixed" when it isn't.
///
/// Discovery procedure:
///
/// 1. Probe the daemon's default by creating a throwaway session and
///    posting a user message with NO `agent` field.  Opencode will
///    stamp it with `defaultAgent()` — the field on that message is
///    the authoritative daemon default.
/// 2. Enumerate primary visible agents via `GET /agent`.
/// 3. Pick `original_agent` = any primary agent ≠ daemon default.
///
/// Errors out if no two distinct primary agents exist.
pub fn pick_distinct_agents_from(daemon: &OpencodeDaemon) -> Result<(String, String)> {
    let client = HarnessClient::new(daemon.base_url());

    // Step 1: discover daemon default by posting an agent-less message.
    let probe_dir = tempfile::Builder::new()
        .prefix("ai-audit-default-agent-probe-")
        .tempdir()
        .context("creating probe workdir")?;
    let probe_session = client.create_session(probe_dir.path())?;
    client.post_user_message(
        &probe_session.id,
        probe_dir.path(),
        /* agent = */ None,
        /* model = */ None,
        "probe",
        /* no_reply = */ true,
    )?;
    let probe_messages = client.wait_for_messages(
        &probe_session.id,
        probe_dir.path(),
        "user",
        1,
        Duration::from_secs(15),
    )?;
    let daemon_default = probe_messages
        .iter()
        .find_map(|m| {
            if m["info"]["role"].as_str() == Some("user") {
                m["info"]["agent"].as_str().map(str::to_string)
            } else {
                None
            }
        })
        .ok_or_else(|| {
            anyhow!("default-agent probe produced no user message with `agent` field")
        })?;

    // Step 2: enumerate primary, visible, non-subagent agents.
    let url = format!("{}/agent", daemon.base_url());
    let value: Value = Client::new()
        .get(&url)
        .timeout(Duration::from_secs(5))
        .send()
        .with_context(|| format!("GET {}", url))?
        .json()
        .context("parsing /agent response")?;
    let arr = value
        .as_array()
        .ok_or_else(|| anyhow!("/agent returned non-array: {}", value))?;
    let mut primary: Vec<String> = arr
        .iter()
        .filter(|a| {
            let mode_ok = a["mode"].as_str().map(|m| m != "subagent").unwrap_or(true);
            let visible = !a["hidden"].as_bool().unwrap_or(false);
            mode_ok && visible
        })
        .filter_map(|a| a["name"].as_str().map(|s| s.to_string()))
        .collect();
    primary.sort();
    primary.dedup();

    // Step 3: pick an original_agent distinct from the daemon default.
    let original = primary
        .iter()
        .find(|name| name.as_str() != daemon_default.as_str())
        .cloned()
        .ok_or_else(|| {
            anyhow!(
                "test needs at least 2 distinct primary opencode agents \
                 (daemon default = {:?}, all primaries = {:?})",
                daemon_default,
                primary
            )
        })?;

    Ok((original, daemon_default))
}

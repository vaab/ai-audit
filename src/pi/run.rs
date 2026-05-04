//! Central AI task execution via `pi`.
//!
//! Drop-in replacement for the historical `opencode::run::run` used
//! by `ai_audit::cli::action::rate`.  Spawns
//! `pi --print --mode json ...` as a subprocess (built by
//! [`super::command::build_hermetic`] with the full set of `--no-*`
//! flags) and parses pi's streaming JSON event output.
//!
//! ## Hermetic guarantees
//!
//! Empirically verified at the insight-cli boundary (see commit
//! history for probe details).  No cwd-walked AGENTS.md, no
//! `~/.claude/CLAUDE.md`, no skill advertisement, no extension
//! prompts, no prompt templates.  Persistent session storage
//! disabled.  System prompt fully replaced by `--system-prompt`.
//!
//! ## Sanity tripwire
//!
//! After pi exits, the raw aggregated output is fed to the
//! caller-supplied [`super::sanity::AiTaskSpec`].  If the predicate
//! rejects it, this function returns
//! [`super::sanity::LlmOutputCutShort`] *before* the caller persists
//! anything.

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use wait_timeout::ChildExt;

use super::sanity::{check as sanity_check, AiTaskSpec};
use crate::provider::TokenUsage;

/// Default timeout for AI API calls (10 minutes).
pub const DEFAULT_AI_TIMEOUT: u64 = 10 * 60;

/// Result of running an AI task via pi.
pub struct AiTaskResult {
    /// Concatenated text content of the final assistant message.
    pub output: String,
    /// Pi session ID (UUID).
    pub session_id: Option<String>,
    /// Cumulative token usage across all assistant turns.
    pub tokens: TokenUsage,
}

/// Options for running a pi task.
///
/// Mirrors the shape of `opencode::run::RunOptions` so the call-site
/// migration is mechanical.  Fields with no pi equivalent (server
/// URLs, persistent session IDs) are gone.
#[derive(Debug, Clone, Default)]
pub struct RunOptions<'a> {
    /// Model identifier (e.g., "anthropic/claude-opus-4-7").
    pub model: Option<&'a str>,
    /// System prompt (replaces pi's default).  Caller-supplied
    /// because rate uses one prompt for the agent under test and a
    /// different one for the judge.
    pub system_prompt: Option<&'a str>,
    /// Timeout in seconds (default: [`DEFAULT_AI_TIMEOUT`]).
    pub timeout_secs: Option<u64>,
    /// Comma-separated tools allowlist passed to pi via `--tools`.
    /// `None` means use pi's default (all built-in tools).
    pub tools: Option<&'a str>,
    /// Verbose mode (currently advisory; pi always streams events).
    pub verbose: bool,
}

/// Run a pi task.
///
/// Hermetic by construction (see module docs).  Every caller MUST
/// supply an [`AiTaskSpec`] describing what a complete answer looks
/// like.
///
/// # Parameters
///
/// * `prompt` — the user message (final positional argv).
/// * `instruction_file` — Markdown file appended to the system
///   prompt via `--append-system-prompt <path>`.
/// * `options` — model, system prompt, timeout, tools allowlist.
/// * `spec` — sanity tripwire; rejects truncated outputs.
/// * `on_event` — optional callback for streaming JSON events.
///   Called for each parsed JSON line.  If `None`, events are
///   processed silently (still fed through the tripwire at the end).
///
/// # Errors
///
/// * Pi not in PATH → wrapped error.
/// * Pi exits non-zero → error containing stderr.
/// * Timeout exceeded → process is killed, error returned.
/// * Sanity tripwire rejects output → wrapped
///   [`super::sanity::LlmOutputCutShort`].
pub fn run<F>(
    prompt: &str,
    instruction_file: &Path,
    options: &RunOptions<'_>,
    spec: &AiTaskSpec<'_>,
    on_event: Option<F>,
) -> Result<AiTaskResult>
where
    F: Fn(&serde_json::Value),
{
    let instruction_str = instruction_file
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("instruction_file path is not valid UTF-8"))?;

    let model = options
        .model
        .ok_or_else(|| anyhow::anyhow!("RunOptions.model is required"))?;

    log::debug!("Calling pi with prompt: {}", prompt);
    log::debug!("Instruction file: {}", instruction_str);
    log::debug!("Using model: {}", model);

    let mut cmd = super::command::build_hermetic();
    cmd.args(["--model", model]);
    if let Some(sys) = options.system_prompt {
        cmd.args(["--system-prompt", sys]);
    }
    cmd.args(["--append-system-prompt", instruction_str]);
    if let Some(tools) = options.tools {
        cmd.args(["--tools", tools]);
    }
    cmd.arg(prompt);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    log::trace!("pi argv: {:?}", cmd);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            bail!("pi not found. Install it or add it to PATH.");
        }
        Err(e) => bail!("Failed to spawn pi: {}", e),
    };

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("pi stdout pipe missing"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow::anyhow!("pi stderr pipe missing"))?;

    let stderr_handle = std::thread::spawn(move || {
        let mut buf = String::new();
        let reader = BufReader::new(stderr);
        for line in reader.lines().map_while(std::result::Result::ok) {
            log::debug!("pi[stderr]: {}", line);
            buf.push_str(&line);
            buf.push('\n');
        }
        buf
    });

    let mut session_id: Option<String> = None;
    let mut final_output = String::new();
    let mut tokens = TokenUsage::default();
    let _ = options.verbose; // currently advisory

    let reader = BufReader::new(stdout);
    for line in reader.lines().map_while(std::result::Result::ok) {
        if line.is_empty() {
            continue;
        }
        let json: serde_json::Value = match serde_json::from_str(&line) {
            Ok(j) => j,
            Err(e) => {
                log::warn!("pi emitted non-JSON line: {} ({})", line, e);
                continue;
            }
        };

        if let Some(cb) = on_event.as_ref() {
            cb(&json);
        }

        match json.get("type").and_then(|v| v.as_str()) {
            Some("session") => {
                if let Some(id) = json.get("id").and_then(|v| v.as_str()) {
                    session_id = Some(id.to_string());
                }
            }
            Some("agent_end") => {
                if let Some(messages) = json.get("messages").and_then(|v| v.as_array()) {
                    let mut last_text = String::new();
                    for msg in messages {
                        if msg.get("role").and_then(|v| v.as_str()) == Some("assistant") {
                            if let Some(usage) = msg.get("usage") {
                                tokens.input +=
                                    usage.get("input").and_then(|v| v.as_u64()).unwrap_or(0);
                                tokens.output +=
                                    usage.get("output").and_then(|v| v.as_u64()).unwrap_or(0);
                                tokens.cache_read +=
                                    usage.get("cacheRead").and_then(|v| v.as_u64()).unwrap_or(0);
                                tokens.cache_write += usage
                                    .get("cacheWrite")
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0);
                            }
                            let mut buf = String::new();
                            if let Some(content) = msg.get("content").and_then(|v| v.as_array()) {
                                for c in content {
                                    if c.get("type").and_then(|v| v.as_str()) == Some("text") {
                                        if let Some(t) = c.get("text").and_then(|v| v.as_str()) {
                                            buf.push_str(t);
                                        }
                                    }
                                }
                            }
                            if !buf.is_empty() {
                                last_text = buf;
                            }
                        }
                    }
                    final_output = last_text;
                }
            }
            _ => {}
        }
    }

    let timeout_duration = Duration::from_secs(options.timeout_secs.unwrap_or(DEFAULT_AI_TIMEOUT));
    let start = Instant::now();
    let status = match child.wait_timeout(timeout_duration)? {
        Some(s) => s,
        None => {
            log::warn!(
                "pi timeout after {}s, killing process",
                timeout_duration.as_secs()
            );
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "Timeout: pi did not complete within {}s",
                timeout_duration.as_secs()
            );
        }
    };
    log::debug!(
        "pi exited in {:.1}s with status {:?}",
        start.elapsed().as_secs_f64(),
        status
    );

    let stderr_text = stderr_handle.join().unwrap_or_default();

    if !status.success() {
        bail!(
            "pi exited with code {}: {}",
            status.code().unwrap_or(-1),
            stderr_text.trim()
        );
    }

    log::debug!(
        "pi tokens: in={} out={} cache_read={} cache_write={}",
        tokens.input,
        tokens.output,
        tokens.cache_read,
        tokens.cache_write
    );

    sanity_check(spec, session_id.as_deref(), &final_output)
        .map_err(|e| anyhow::anyhow!("{}", e))?;

    Ok(AiTaskResult {
        output: final_output,
        session_id,
        tokens,
    })
}

/// Get the default model from `~/.pi/agent/settings.json`.
///
/// Reads `defaultProvider` and `defaultModel` and returns
/// `"<provider>/<model>"`.
pub fn get_default_model() -> Result<String> {
    let home = std::env::var("HOME").map_err(|_| anyhow::anyhow!("HOME not set"))?;
    let path = std::path::PathBuf::from(home).join(".pi/agent/settings.json");
    let content = std::fs::read_to_string(&path).map_err(|e| {
        anyhow::anyhow!(
            "Failed to read pi settings {:?}: {}\nIs pi installed and configured?",
            path,
            e
        )
    })?;
    let settings: serde_json::Value = serde_json::from_str(&content)
        .map_err(|e| anyhow::anyhow!("Failed to parse pi settings {:?}: {}", path, e))?;

    let provider = settings
        .get("defaultProvider")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("pi settings missing 'defaultProvider'"))?;
    let model = settings
        .get("defaultModel")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("pi settings missing 'defaultModel'"))?;

    Ok(format!("{}/{}", provider, model))
}

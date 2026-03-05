//! Opencode agent invocation with file-based instructions.
//!
//! Provides a unified interface for running opencode agents with instruction files.

use std::io::ErrorKind;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use wait_timeout::ChildExt;

/// Default timeout in seconds.
const DEFAULT_TIMEOUT_SECS: u64 = 300;

/// Get the default model from opencode config.
///
/// Runs `opencode debug config` and extracts the `model` field.
pub fn get_default_model() -> Result<String> {
    let output = Command::new("opencode")
        .args(["debug", "config"])
        .output()
        .context("Failed to run 'opencode debug config'. Is opencode installed?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "opencode debug config failed (exit {}): {}",
            output.status,
            stderr.trim()
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Parse JSON output to find top-level "model" field
    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("\"model\":") {
            if let Some(start) = trimmed.find(": \"") {
                let rest = &trimmed[start + 3..];
                if let Some(end) = rest.find('"') {
                    return Ok(rest[..end].to_string());
                }
            }
        }
    }

    bail!("Could not find default model in opencode config")
}

/// Get the default agent from opencode config.
///
/// Runs `opencode debug config` and extracts the first agent name
/// (which is the primary agent opencode uses by default).
pub fn get_default_agent() -> Result<Option<String>> {
    let output = Command::new("opencode")
        .args(["debug", "config"])
        .output()
        .context("Failed to run 'opencode debug config'. Is opencode installed?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "opencode debug config failed (exit {}): {}",
            output.status,
            stderr.trim()
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Parse JSON to find the first agent name in the "agent" section
    // The first agent is the default primary agent
    let mut in_agent_section = false;
    let mut brace_depth = 0;
    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("\"agent\":") {
            in_agent_section = true;
            continue;
        }
        if in_agent_section {
            // Look for first key that starts an agent definition (not "default")
            if brace_depth == 1 && trimmed.starts_with('"') && !trimmed.starts_with("\"default\"") {
                // Extract the agent name
                if let Some(end) = trimmed[1..].find('"') {
                    return Ok(Some(trimmed[1..end + 1].to_string()));
                }
            }
            brace_depth += trimmed.matches('{').count();
            brace_depth = brace_depth.saturating_sub(trimmed.matches('}').count());
            if brace_depth == 0 && trimmed.contains('}') {
                // Exited agent section
                break;
            }
        }
    }

    // No agents configured
    Ok(None)
}

/// Instruction prefix prepended to all prompts.
const INSTRUCTION_PREFIX: &str =
    "To answer, make sure you apply strictly the instruction provided in the attached file context.";

// Re-export TokenUsage from cache module
pub use super::cache::TokenUsage;

/// Result of an opencode agent invocation.
#[derive(Debug, Clone)]
pub struct RunResult {
    /// Raw output from the agent (stdout).
    pub output: String,
    /// Session ID if captured from JSON output.
    pub session_id: Option<String>,
    /// Agent name extracted from stderr logs.
    pub agent: Option<String>,
    /// Execution time in seconds.
    pub execution_time_secs: f64,
    /// Token usage.
    pub tokens: TokenUsage,
}

/// Options for running an opencode agent.
#[derive(Debug, Clone)]
pub struct RunOptions<'a> {
    /// Model identifier (e.g., "anthropic/claude-sonnet-4-20250514").
    /// If None, uses opencode's configured default.
    pub model: Option<&'a str>,
    /// Agent name for opencode.
    /// If None, uses opencode's default agent.
    pub agent: Option<&'a str>,
    /// Timeout in seconds (default: 300).
    pub timeout_secs: Option<u64>,
    /// Session ID to continue (optional).
    pub session_id: Option<&'a str>,
    /// Enable verbose mode (--print-logs, stderr streaming).
    pub verbose: bool,
    /// Use cache for reading results (default: true).
    /// Set to false to bypass cache read and recompute fresh.
    /// Note: Results are ALWAYS written to cache regardless of this flag.
    pub cache: bool,
}

impl<'a> Default for RunOptions<'a> {
    fn default() -> Self {
        Self {
            model: None,
            agent: None,
            timeout_secs: None,
            session_id: None,
            verbose: false,
            cache: true,
        }
    }
}

/// Run an opencode agent with the given prompt and instruction file.
///
/// # Arguments
/// * `prompt` - The user prompt to send
/// * `instruction_file` - Path to the instruction file (attached via -f)
/// * `options` - Run options (model, agent, timeout, session). All optional fields
///   will use opencode's configured defaults if not specified.
/// * `on_event` - Optional callback for streaming JSON events as they arrive.
///   Called for each JSON line from stdout. If None, events are accumulated silently.
///
/// # Returns
/// * `RunResult` containing the raw output and optional session ID
///
/// # Errors
/// * Returns error if opencode is not found
/// * Returns error if timeout is exceeded
/// * Returns error if agent exits with non-zero status
///
/// # Example
/// ```no_run
/// use ai_audit::opencode::run::{run, RunOptions};
/// use std::path::Path;
///
/// // Use all defaults from opencode config (no streaming callback)
/// let result = run(
///     "Process timespan 2025-01-01 08:00:00..09:00:00",
///     Path::new("/path/to/instructions.md"),
///     &RunOptions::default(),
///     None::<fn(&serde_json::Value)>,
/// )?;
/// println!("Output: {}", result.output);
/// # Ok::<(), anyhow::Error>(())
/// ```
pub fn run<F>(
    prompt: &str,
    instruction_file: &Path,
    options: &RunOptions,
    mut on_event: Option<F>,
) -> Result<RunResult>
where
    F: FnMut(&serde_json::Value),
{
    use super::cache;

    let instruction_str = instruction_file
        .to_str()
        .context("instruction path is not valid UTF-8")?;

    // Resolve defaults from opencode config
    let resolved_model: String;
    let model = match options.model {
        Some(m) => m,
        None => {
            resolved_model = get_default_model()?;
            &resolved_model
        }
    };

    let resolved_agent: Option<String>;
    let agent = match options.agent {
        Some(a) => Some(a),
        None => {
            resolved_agent = get_default_agent()?;
            resolved_agent.as_deref()
        }
    };

    let timeout_secs = options.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS);

    // Build full prompt with instruction prefix
    let full_prompt = format!("{}\n\n{}", INSTRUCTION_PREFIX, prompt);

    // Compute hashes for cache key
    let instruction_hash =
        cache::git_hash_file(instruction_file).context("Failed to hash instruction file")?;
    let prompt_hash = cache::git_hash_string(&full_prompt).context("Failed to hash prompt")?;

    // Check cache first (unless disabled or continuing a session)
    if options.cache && options.session_id.is_none() {
        if let Ok(Some(cached)) = cache::read_cache(agent, model, &instruction_hash, &prompt_hash) {
            log::info!(
                "Using cached result for {}-{}",
                model,
                &instruction_hash[..8]
            );
            return Ok(RunResult {
                output: cached.output,
                session_id: cached.session_id,
                agent: cached.agent,
                execution_time_secs: cached.execution_time_secs,
                tokens: cached.tokens,
            });
        }
    }

    log::debug!(
        "Invoking opencode with model '{}', agent '{:?}', timeout {}s",
        model,
        agent,
        timeout_secs
    );
    log::debug!("Instruction file: {}", instruction_str);
    log::debug!("Prompt: {}", full_prompt);

    // Build command args
    let model_flag = format!("--model={}", model);
    let file_flag = format!("-f={}", instruction_str);

    let mut args = vec!["run", &model_flag, &file_flag];

    // Add agent if specified
    let agent_flag: String;
    if let Some(a) = agent {
        agent_flag = format!("--agent={}", a);
        args.push(&agent_flag);
    }

    // Add session if specified
    let session_flag: String;
    if let Some(session) = options.session_id {
        session_flag = format!("--session={}", session);
        args.push(&session_flag);
    }

    // Add format=json to capture structured output
    args.push("--format=json");

    // Always add print-logs to capture agent name from stderr
    args.push("--print-logs");

    // Separator and prompt
    args.push("--");
    args.push(&full_prompt);

    log::trace!("opencode args: {:?}", args);

    // Spawn the opencode process
    // Always capture stderr to extract agent name
    let mut child = match Command::new("opencode")
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) if e.kind() == ErrorKind::NotFound => {
            bail!("opencode not found. Install it or add it to PATH.");
        }
        Err(e) => {
            bail!("Failed to spawn opencode: {}", e);
        }
    };

    // Track execution time
    let start_time = std::time::Instant::now();

    // Stream stdout line by line
    let stdout_handle = child.stdout.take().expect("stdout should be piped");
    let reader = std::io::BufReader::new(stdout_handle);

    // Capture stderr in a separate thread to extract agent name (and log at debug level)
    let stderr_handle = child.stderr.take().expect("stderr should be piped");
    let stderr_thread = std::thread::spawn(move || {
        let stderr_reader = std::io::BufReader::new(stderr_handle);
        let mut agent_name: Option<String> = None;
        use std::io::BufRead;
        for line in stderr_reader.lines() {
            if let Ok(line) = line {
                log::debug!("{}", line);
                // Parse agent from log lines like: service=llm ... agent=sisyphus mode=primary
                if line.contains("service=llm") && line.contains("mode=primary") {
                    if let Some(agent_start) = line.find("agent=") {
                        let rest = &line[agent_start + 6..];
                        if let Some(end) = rest.find(' ') {
                            let found = rest[..end].to_string();
                            // Skip "title" agent, we want the main agent
                            if found != "title" && agent_name.is_none() {
                                agent_name = Some(found);
                            }
                        }
                    }
                }
            }
        }
        agent_name
    });

    let mut stdout_lines = Vec::new();
    let mut session_id: Option<String> = None;
    let mut tokens = TokenUsage::default();

    use std::io::BufRead;
    for line_result in reader.lines() {
        let line = match line_result {
            Ok(l) => l,
            Err(e) => {
                log::warn!("Error reading stdout line: {}", e);
                continue;
            }
        };

        // Accumulate for cache/output
        stdout_lines.push(line.clone());

        // Parse as JSON and call callback if provided
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&line) {
            // Extract session ID from first event that has it
            if session_id.is_none() {
                if let Some(sid) = json.get("sessionID").and_then(|v| v.as_str()) {
                    session_id = Some(sid.to_string());
                    log::debug!("Captured session ID: {}", sid);
                }
            }

            // Extract token usage from step_finish events
            // Tokens are in part.tokens, with cache nested as part.tokens.cache.read/write
            if json.get("type").and_then(|v| v.as_str()) == Some("step_finish") {
                if let Some(part) = json.get("part") {
                    if let Some(token_usage) = part.get("tokens") {
                        if let Some(input) = token_usage.get("input").and_then(|v| v.as_u64()) {
                            tokens.input += input;
                        }
                        if let Some(output) = token_usage.get("output").and_then(|v| v.as_u64()) {
                            tokens.output += output;
                        }
                        if let Some(reasoning) =
                            token_usage.get("reasoning").and_then(|v| v.as_u64())
                        {
                            tokens.reasoning += reasoning;
                        }
                        // Cache tokens are nested under part.tokens.cache
                        if let Some(cache) = token_usage.get("cache") {
                            if let Some(read) = cache.get("read").and_then(|v| v.as_u64()) {
                                tokens.cache_read += read;
                            }
                            if let Some(write) = cache.get("write").and_then(|v| v.as_u64()) {
                                tokens.cache_write += write;
                            }
                        }
                    }
                }
            }

            // Call streaming callback if provided
            if let Some(ref mut callback) = on_event {
                callback(&json);
            }
        }
    }

    let execution_time_secs = start_time.elapsed().as_secs_f64();

    let stdout_content = stdout_lines.join("\n");

    // Wait for process to finish with timeout
    let timeout_duration = Duration::from_secs(timeout_secs);
    let status = match child.wait_timeout(timeout_duration)? {
        Some(status) => status,
        None => {
            // Timeout - kill the process
            log::warn!("Timeout after {}s, killing opencode process", timeout_secs);
            let _ = child.kill();
            let _ = child.wait(); // Reap the zombie
            bail!(
                "Timeout: opencode did not complete within {}s. \
                 The model '{}' may be slow or unresponsive.",
                timeout_secs,
                model
            );
        }
    };

    // Collect agent name from stderr thread
    let captured_agent = stderr_thread.join().ok().flatten();

    // Exit code 127 = command not found (from shell)
    if let Some(127) = status.code() {
        bail!("opencode not found. Install it or add it to PATH.");
    }

    // Check for non-zero exit
    if !status.success() {
        let code = status.code().unwrap_or(-1);
        bail!("opencode exited with code {}", code);
    }

    // Session ID already captured during streaming
    // Always write to cache (unless continuing a session)
    // cache: false means "bypass read" not "disable caching entirely"
    // Cache the extracted text content, not the raw JSON stream
    if options.session_id.is_none() {
        let text_content = extract_text_content(&stdout_content);
        if let Err(e) = cache::write_cache(
            captured_agent.as_deref(),
            model,
            &instruction_hash,
            &prompt_hash,
            &text_content,
            session_id.as_deref(),
            captured_agent.as_deref(),
            execution_time_secs,
            &tokens,
        ) {
            log::warn!("Failed to write cache: {}", e);
        }
    }

    // Return extracted text content (same format as cached results)
    let text_content = extract_text_content(&stdout_content);
    Ok(RunResult {
        output: text_content,
        session_id,
        agent: captured_agent,
        execution_time_secs,
        tokens,
    })
}

/// Extract text content from JSON output lines.
///
/// Parses the streaming JSON output and concatenates all text parts.
pub fn extract_text_content(output: &str) -> String {
    let mut text = String::new();

    for line in output.lines() {
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(line) {
            // Look for text parts in the output
            if let Some(part) = json.get("part") {
                if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
                    text.push_str(t);
                }
            }
        }
    }

    text
}

#[cfg(test)]
mod tests {
    use super::*;

    // Note: extract_session_id tests removed - session ID is now captured during streaming

    #[test]
    fn test_extract_text_content() {
        let output = r#"{"sessionID":"ses_abc123","type":"start"}
{"type":"text","part":{"text":"Hello "}}
{"type":"text","part":{"text":"World"}}
{"type":"end"}"#;

        let text = extract_text_content(output);
        assert_eq!(text, "Hello World");
    }

    #[test]
    fn test_extract_text_content_empty() {
        let output = r#"{"sessionID":"ses_abc123","type":"start"}
{"type":"end"}"#;

        let text = extract_text_content(output);
        assert_eq!(text, "");
    }

    #[test]
    fn test_run_result_clone() {
        let result = RunResult {
            output: "test output".to_string(),
            session_id: Some("ses_123".to_string()),
            agent: Some("sisyphus".to_string()),
            execution_time_secs: 1.5,
            tokens: TokenUsage {
                input: 100,
                output: 50,
                reasoning: 0,
                cache_read: 0,
                cache_write: 0,
            },
        };
        let cloned = result.clone();
        assert_eq!(cloned.output, result.output);
        assert_eq!(cloned.session_id, result.session_id);
        assert_eq!(cloned.execution_time_secs, result.execution_time_secs);
        assert_eq!(cloned.tokens.input, result.tokens.input);
    }

    #[test]
    fn test_run_options_default() {
        let opts = RunOptions::<'_>::default();
        assert_eq!(opts.model, None);
        assert_eq!(opts.agent, None);
        assert_eq!(opts.timeout_secs, None);
        assert_eq!(opts.session_id, None);
        assert!(!opts.verbose);
        assert!(opts.cache); // cache is true by default
    }

    #[test]
    fn test_instruction_prefix() {
        assert!(INSTRUCTION_PREFIX.contains("instruction"));
        assert!(INSTRUCTION_PREFIX.contains("attached file"));
    }
}

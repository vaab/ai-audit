//! Agent invocation for opencode.
//!
//! Provides functionality to invoke opencode agents with timeout handling.

use std::io::{ErrorKind, Read};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use wait_timeout::ChildExt;

/// Result of an agent invocation.
#[derive(Debug, Clone)]
pub struct AgentResult {
    /// Raw output from the agent (stdout).
    pub output: String,
    /// Session ID if captured from JSON output.
    pub session_id: Option<String>,
}

/// Invoke an opencode agent with the given instruction file.
///
/// # Arguments
/// * `instruction_path` - Path to the instruction file to pass to the agent
/// * `agent_name` - Name of the opencode agent to use
/// * `model` - Model identifier (e.g., "anthropic/claude-sonnet-4-20250514")
/// * `timeout_secs` - Maximum time to wait for the agent (in seconds)
///
/// # Returns
/// * `AgentResult` containing the raw output and optional session ID
///
/// # Errors
/// * Returns error if opencode is not found
/// * Returns error if timeout is exceeded
/// * Returns error if agent exits with non-zero status
pub fn invoke_agent(
    instruction_path: &Path,
    agent_name: &str,
    model: &str,
    timeout_secs: u64,
) -> Result<AgentResult> {
    let instruction_str = instruction_path
        .to_str()
        .context("instruction path is not valid UTF-8")?;

    log::debug!(
        "Invoking opencode agent '{}' with model '{}', timeout {}s",
        agent_name,
        model,
        timeout_secs
    );
    log::debug!("Instruction file: {}", instruction_str);

    // Build command args
    let model_flag = format!("--model={}", model);
    let args = vec![
        "run",
        "--agent",
        agent_name,
        "--format",
        "json",
        &model_flag,
        instruction_str,
    ];

    // Spawn the opencode process
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

    // Wait with timeout
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

    // Read stdout
    let mut stdout_content = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        stdout.read_to_string(&mut stdout_content)?;
    }

    // Read stderr
    let mut stderr_content = String::new();
    if let Some(mut stderr) = child.stderr.take() {
        stderr.read_to_string(&mut stderr_content)?;
    }

    // Exit code 127 = command not found (from shell)
    if let Some(127) = status.code() {
        bail!("opencode not found. Install it or add it to PATH.");
    }

    // Check for non-zero exit
    if !status.success() {
        let code = status.code().unwrap_or(-1);
        let mut error_msg = format!("opencode exited with code {}", code);
        if !stderr_content.is_empty() {
            error_msg.push_str(&format!(": {}", stderr_content.trim()));
        }
        bail!(error_msg);
    }

    // Try to extract session ID from JSON output
    let session_id = extract_session_id(&stdout_content);

    Ok(AgentResult {
        output: stdout_content,
        session_id,
    })
}

/// Extract session ID from JSON output lines.
fn extract_session_id(output: &str) -> Option<String> {
    for line in output.lines() {
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(line) {
            if let Some(sid) = json.get("sessionID").and_then(|v| v.as_str()) {
                return Some(sid.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_invoke_agent_command_not_found() {
        // Use a nonexistent command by temporarily modifying PATH
        // This test verifies error handling when opencode is not available

        // Create a temp file as instruction
        let temp_dir = std::env::temp_dir();
        let instruction_path = temp_dir.join("test_instruction.txt");
        std::fs::write(&instruction_path, "test instruction").unwrap();

        // We can't easily mock the command not found scenario without
        // modifying PATH, so we'll test with a very short timeout
        // and a real invocation that should fail quickly if opencode
        // is not installed
        let result = invoke_agent(
            &instruction_path,
            "nonexistent-agent",
            "fake-model",
            1, // 1 second timeout
        );

        // Clean up
        let _ = std::fs::remove_file(&instruction_path);

        // The result should be an error (either not found or timeout)
        assert!(result.is_err());
    }

    #[test]
    fn test_extract_session_id() {
        let output = r#"{"sessionID":"ses_abc123","type":"start"}
{"type":"text","part":{"text":"Hello"}}
{"type":"end"}"#;

        let session_id = extract_session_id(output);
        assert_eq!(session_id, Some("ses_abc123".to_string()));
    }

    #[test]
    fn test_extract_session_id_not_found() {
        let output = r#"{"type":"text","part":{"text":"Hello"}}
{"type":"end"}"#;

        let session_id = extract_session_id(output);
        assert_eq!(session_id, None);
    }

    #[test]
    fn test_agent_result_clone() {
        let result = AgentResult {
            output: "test output".to_string(),
            session_id: Some("ses_123".to_string()),
        };
        let cloned = result.clone();
        assert_eq!(cloned.output, result.output);
        assert_eq!(cloned.session_id, result.session_id);
    }
}

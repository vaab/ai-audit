//! Judge invocation and output parsing for AI rating.
//!
//! Provides functionality to invoke a judge agent that evaluates
//! another agent's output against a verification checklist.
//!
//! The judge runs through [`crate::pi::run`] in hermetic mode — the
//! judge sees ONLY the judge prompt + the agent output, never the
//! host's AGENTS.md / CLAUDE.md / skills.  This keeps ratings
//! reproducible across machines and over time.

use std::path::Path;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::pi::run::{run as pi_run, RunOptions};
use crate::pi::sanity::AiTaskSpec;

/// Result of a judge evaluation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JudgeRating {
    /// Number of checklist items that passed.
    pub points: u32,
    /// Total number of checklist items.
    pub max_points: u32,
    /// Name of the test being evaluated.
    pub test_name: String,
    /// Model used by the agent being evaluated.
    pub agent_model: String,
    /// Model used by the judge.
    pub judge_model: String,
    /// Timestamp of the evaluation.
    pub timestamp: DateTime<Utc>,
    /// Complete judge output including YAML frontmatter.
    pub full_report: String,
}

/// YAML frontmatter structure for parsing judge output.
#[derive(Debug, Deserialize)]
struct JudgeFrontmatter {
    points: u32,
    max_points: u32,
    test_name: String,
    agent_model: String,
    judge_model: String,
    timestamp: DateTime<Utc>,
}

/// Invoke a judge agent to evaluate agent output against a checklist.
///
/// # Arguments
/// * `judge_prompt_path` - Path to the judge prompt file
/// * `agent_output` - Output from the agent being evaluated
/// * `checklist` - Verification checklist items
/// * `test_name` - Name of the test being evaluated
/// * `agent_model` - Model identifier of the agent being evaluated
/// * `judge_model` - Model identifier for the judge (e.g., "anthropic/claude-opus-4")
/// * `timeout_secs` - Maximum time to wait for the judge (in seconds)
///
/// # Returns
/// * `JudgeRating` containing the parsed rating and full report
///
/// # Errors
/// * Returns error if judge prompt file cannot be read
/// * Returns error if opencode is not found
/// * Returns error if timeout is exceeded
/// * Returns error if judge output cannot be parsed
pub fn invoke_judge(
    judge_prompt_path: &Path,
    agent_output: &str,
    checklist: &[String],
    test_name: &str,
    agent_model: &str,
    judge_model: &str,
    timeout_secs: u64,
) -> Result<JudgeRating> {
    // Confirm judge prompt exists (pi will read it via
    // ``--append-system-prompt``).
    if !judge_prompt_path.exists() {
        bail!(
            "Judge prompt not found: {:?}.  Pass --judge-prompt or \
             create the default at \
             ~/.local/share/ai-audit/rate/judge-prompt.md.",
            judge_prompt_path
        );
    }

    // The user message contains the per-call dynamic data: test
    // metadata, checklist, and the agent output to be judged.  The
    // judge prompt itself (the static framing + YAML schema) is
    // delivered as the appended system prompt.
    let checklist_text = checklist
        .iter()
        .map(|item| format!("- [ ] {}", item))
        .collect::<Vec<_>>()
        .join("\n");

    let user_message = format!(
        r#"## Test Metadata

- Test name: {test_name}
- Agent model: {agent_model}
- Judge model: {judge_model}

## Verification Checklist

{checklist_text}

## Agent Output

{agent_output}

---

Now provide your rating in the specified YAML format."#
    );

    log::debug!(
        "Invoking judge with model '{}', timeout {}s",
        judge_model,
        timeout_secs
    );
    log::trace!("User message length: {} chars", user_message.len());

    // Sanity tripwire: a healthy judge response always opens with
    // YAML frontmatter (``---`` on the first non-blank line).
    // Cut-short / preamble-only outputs are rejected at the harness
    // boundary so they never reach ``parse_judge_output``.
    let spec = AiTaskSpec {
        shape: "YAML frontmatter starting with '---'",
        looks_complete: &|s: &str| {
            if s.trim_start().starts_with("---") {
                Ok(())
            } else {
                Err("output does not begin with YAML frontmatter '---'".into())
            }
        },
    };

    let options = RunOptions {
        model: Some(judge_model),
        // Judge runs without any extra system prompt: the appended
        // file IS the judge's full instruction set.  The host's
        // ``--no-context-files`` etc. ensure no AGENTS.md leakage.
        system_prompt: None,
        timeout_secs: Some(timeout_secs),
        // Judges are pure scoring: no shell/file tools.
        tools: None,
        verbose: false,
    };

    let result =
        pi_run::<fn(&serde_json::Value)>(&user_message, judge_prompt_path, &options, &spec, None)?;

    parse_judge_output(&result.output)
}

/// Extract text content from JSON-formatted opencode output.
#[allow(dead_code)]
fn extract_text_from_json(output: &str) -> String {
    let mut text_parts = Vec::new();
    for line in output.lines() {
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(line) {
            // Look for text parts in the JSON
            if let Some(part) = json.get("part") {
                if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                    text_parts.push(text.to_string());
                }
            }
        }
    }
    text_parts.join("")
}

/// Parse judge output containing YAML frontmatter.
///
/// # Arguments
/// * `output` - Raw output from the judge agent
///
/// # Returns
/// * `JudgeRating` with parsed fields and full report
///
/// # Errors
/// * Returns error if YAML frontmatter is missing
/// * Returns error if YAML parsing fails
/// * Returns error if required fields are missing
/// * Returns error if points > max_points
pub fn parse_judge_output(output: &str) -> Result<JudgeRating> {
    // Find YAML frontmatter between --- markers
    let trimmed = output.trim();

    // Must start with ---
    if !trimmed.starts_with("---") {
        bail!(
            "Judge output missing YAML frontmatter. Expected output to start with '---'. \
             Got: {}...",
            &trimmed.chars().take(50).collect::<String>()
        );
    }

    // Find the closing ---
    let rest = &trimmed[3..]; // Skip opening ---
    let end_marker = rest.find("\n---");
    let yaml_content = match end_marker {
        Some(pos) => &rest[..pos],
        None => {
            bail!(
                "Judge output has unclosed YAML frontmatter. \
                 Expected closing '---' marker."
            );
        }
    };

    // Parse YAML
    let frontmatter: JudgeFrontmatter = serde_yaml::from_str(yaml_content.trim())
        .with_context(|| format!("Failed to parse YAML frontmatter:\n{}", yaml_content))?;

    // Validate points <= max_points
    if frontmatter.points > frontmatter.max_points {
        bail!(
            "Invalid rating: points ({}) exceeds max_points ({})",
            frontmatter.points,
            frontmatter.max_points
        );
    }

    Ok(JudgeRating {
        points: frontmatter.points,
        max_points: frontmatter.max_points,
        test_name: frontmatter.test_name,
        agent_model: frontmatter.agent_model,
        judge_model: frontmatter.judge_model,
        timestamp: frontmatter.timestamp,
        full_report: output.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_judge_output_valid() {
        let output = r#"---
points: 8
max_points: 11
test_name: example-test
agent_model: claude-sonnet
judge_model: claude-opus
timestamp: 2025-01-29T10:30:00Z
---

# Rating Report

## Checklist Results

- [x] First item: PASS - Good work
- [ ] Second item: FAIL - Missing implementation
- [x] Third item: PASS - Works correctly

## Summary

The agent completed most requirements successfully.
"#;

        let rating = parse_judge_output(output).unwrap();
        assert_eq!(rating.points, 8);
        assert_eq!(rating.max_points, 11);
        assert_eq!(rating.test_name, "example-test");
        assert_eq!(rating.agent_model, "claude-sonnet");
        assert_eq!(rating.judge_model, "claude-opus");
        assert_eq!(rating.full_report, output);
    }

    #[test]
    fn test_parse_judge_output_missing_frontmatter() {
        let output = "# Rating Report\n\nNo YAML here.";

        let result = parse_judge_output(output);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("missing YAML frontmatter"));
    }

    #[test]
    fn test_parse_judge_output_unclosed_frontmatter() {
        let output = r#"---
points: 5
max_points: 10
# Missing closing ---
"#;

        let result = parse_judge_output(output);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("unclosed YAML frontmatter"));
    }

    #[test]
    fn test_parse_judge_output_invalid_yaml() {
        let output = r#"---
points: not_a_number
max_points: 10
---
"#;

        let result = parse_judge_output(output);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Failed to parse YAML"));
    }

    #[test]
    fn test_parse_judge_output_points_exceed_max() {
        let output = r#"---
points: 15
max_points: 10
test_name: test
agent_model: agent
judge_model: judge
timestamp: 2025-01-29T10:30:00Z
---
"#;

        let result = parse_judge_output(output);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("exceeds max_points"));
    }

    #[test]
    fn test_parse_judge_output_missing_required_field() {
        let output = r#"---
points: 5
max_points: 10
---
"#;

        let result = parse_judge_output(output);
        assert!(result.is_err());
        // serde_yaml will error on missing required field
    }

    #[test]
    fn test_parse_judge_output_zero_points() {
        let output = r#"---
points: 0
max_points: 5
test_name: zero-test
agent_model: agent
judge_model: judge
timestamp: 2025-01-29T10:30:00Z
---

# Report
All items failed.
"#;

        let rating = parse_judge_output(output).unwrap();
        assert_eq!(rating.points, 0);
        assert_eq!(rating.max_points, 5);
    }

    #[test]
    fn test_parse_judge_output_perfect_score() {
        let output = r#"---
points: 10
max_points: 10
test_name: perfect-test
agent_model: agent
judge_model: judge
timestamp: 2025-01-29T10:30:00Z
---

# Report
Perfect score!
"#;

        let rating = parse_judge_output(output).unwrap();
        assert_eq!(rating.points, 10);
        assert_eq!(rating.max_points, 10);
    }

    #[test]
    fn test_extract_text_from_json() {
        let json_output = r#"{"sessionID":"ses_abc123","type":"start"}
{"type":"text","part":{"text":"---\npoints: 5\n"}}
{"type":"text","part":{"text":"max_points: 10\n---"}}
{"type":"end"}"#;

        let text = extract_text_from_json(json_output);
        assert_eq!(text, "---\npoints: 5\nmax_points: 10\n---");
    }

    #[test]
    fn test_extract_text_from_json_empty() {
        let json_output = r#"{"sessionID":"ses_abc123","type":"start"}
{"type":"end"}"#;

        let text = extract_text_from_json(json_output);
        assert_eq!(text, "");
    }

    #[test]
    fn test_judge_rating_clone() {
        let rating = JudgeRating {
            points: 5,
            max_points: 10,
            test_name: "test".to_string(),
            agent_model: "agent".to_string(),
            judge_model: "judge".to_string(),
            timestamp: Utc::now(),
            full_report: "report".to_string(),
        };
        let cloned = rating.clone();
        assert_eq!(cloned.points, rating.points);
        assert_eq!(cloned.test_name, rating.test_name);
    }

    #[test]
    fn test_judge_rating_serialize() {
        let rating = JudgeRating {
            points: 5,
            max_points: 10,
            test_name: "test".to_string(),
            agent_model: "agent".to_string(),
            judge_model: "judge".to_string(),
            timestamp: chrono::DateTime::parse_from_rfc3339("2025-01-29T10:30:00Z")
                .unwrap()
                .with_timezone(&Utc),
            full_report: "report".to_string(),
        };

        let json = serde_json::to_string(&rating).unwrap();
        assert!(json.contains("\"points\":5"));
        assert!(json.contains("\"test_name\":\"test\""));
    }
}

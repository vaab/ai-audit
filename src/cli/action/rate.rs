//! Rate command handler.
//!
//! Runs the agent under test (via `pi`) with each requested model,
//! captures the output, and judges the output against the test's
//! checklist (also via `pi`, see [`crate::rate::invoke_judge`]).
//!
//! Both the agent and judge invocations go through
//! [`crate::pi::run`] in hermetic mode — see that module for the
//! sealing rationale (no AGENTS.md walking, no skills, no
//! `~/.claude/CLAUDE.md`, etc.).  The harness contract is therefore
//! independent of the host's `pi` configuration: the same
//! instruction file + test prompt + model produce the same context.

use anyhow::Result;
use std::path::Path;

use crate::pi::run::{get_default_model, run as pi_run, RunOptions};
use crate::pi::sanity::AiTaskSpec;
use crate::rate;

#[allow(clippy::too_many_arguments)]
pub fn run(
    instruction: &Path,
    test_path: &Path,
    agent_models: Option<&str>,
    judge_models: Option<&str>,
    timeout: Option<u64>,
    no_cache: bool,
    judge_prompt: Option<&Path>,
    quiet: bool,
) -> Result<()> {
    // Get default model from pi settings
    let default_model = get_default_model()?;

    // Parse models (CLI overrides pi default)
    let agent_models: Vec<String> = agent_models
        .map(|s| s.split(',').map(|m| m.trim().to_string()).collect())
        .unwrap_or_else(|| vec![default_model.clone()]);

    let judge_models: Vec<String> = judge_models
        .map(|s| s.split(',').map(|m| m.trim().to_string()).collect())
        .unwrap_or_else(|| vec![default_model.clone()]);

    let timeout_secs = timeout;

    // Determine judge prompt path
    let judge_prompt_path = judge_prompt.map(|p| p.to_path_buf()).unwrap_or_else(|| {
        dirs::data_local_dir()
            .unwrap()
            .join("ai-audit/rate/judge-prompt.md")
    });

    // Compute instruction hash for cache key
    let instruction_hash = rate::git_hash_file(instruction)?;

    // Parse the test file
    let test_case = rate::parse_test_file(test_path)?;

    // Substitute variables in the prompt
    let prompt = rate::substitute_variables(&test_case.prompt, &test_case.timespan);

    // Run through all agent/judge model combinations
    for agent_model in &agent_models {
        // Build report path for cache checking
        let report_dir = dirs::data_local_dir()
            .unwrap()
            .join("ai-audit/rate/reports");
        let report_path = report_dir.join(format!(
            "{}-{}-{}.json",
            test_case.name,
            agent_model.replace('/', "_"),
            instruction_hash
        ));

        // Check cache unless --no-cache
        if rate::should_skip_cached(&report_path, no_cache) {
            if !quiet {
                println!("Skipping {} (cached)", test_case.name);
            }
            continue;
        }

        // Invoke agent under test via pi.
        if !quiet {
            println!(
                "Running agent {} on test '{}'...",
                agent_model, test_case.name
            );
        }

        // The agent under test is a black box: we don't know what its
        // output should look like, so the sanity tripwire here is
        // intentionally permissive (any non-empty output passes).
        // The judge step (`invoke_judge`) is responsible for the
        // semantic verification against the test's checklist.
        let agent_spec = AiTaskSpec {
            shape: "any non-empty agent response",
            looks_complete: &|s: &str| {
                if s.trim().is_empty() {
                    Err("agent produced no text content".into())
                } else {
                    Ok(())
                }
            },
        };

        let options = RunOptions {
            model: Some(agent_model),
            // The instruction file IS the system prompt for rate
            // (passed via --append-system-prompt below).  We don't
            // pin a meta system prompt here — the agent under test
            // gets exactly what the test author wrote.
            system_prompt: None,
            timeout_secs,
            tools: Some("bash"),
            verbose: !quiet,
        };

        // Streaming callback for live display (when not quiet).
        let on_event = |json: &serde_json::Value| {
            if quiet {
                return;
            }
            let t = json.get("type").and_then(|v| v.as_str());
            match t {
                Some("message_update") => {
                    let ame = match json.get("assistantMessageEvent") {
                        Some(v) => v,
                        None => return,
                    };
                    if ame.get("type").and_then(|v| v.as_str()) == Some("text_delta") {
                        if let Some(delta) = ame.get("delta").and_then(|v| v.as_str()) {
                            for line in delta.lines() {
                                if !line.trim().is_empty() {
                                    let display = if line.len() > 120 {
                                        format!("    {}...", &line[..120])
                                    } else {
                                        format!("    {}", line)
                                    };
                                    println!("{}", display);
                                }
                            }
                        }
                    }
                }
                Some("tool_execution_start") => {
                    let tool = json
                        .get("toolName")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown");
                    let desc = json
                        .get("args")
                        .and_then(|a| {
                            a.get("command")
                                .or_else(|| a.get("description"))
                                .and_then(|v| v.as_str())
                        })
                        .map(|s| {
                            if s.len() > 80 {
                                format!("{}...", &s[..80])
                            } else {
                                s.to_string()
                            }
                        })
                        .unwrap_or_default();
                    println!("    [{}] {}", tool, desc);
                }
                _ => {}
            }
        };

        let agent_result = pi_run(&prompt, instruction, &options, &agent_spec, Some(on_event))?;

        // Judge with each judge model
        for judge_model in &judge_models {
            if !quiet {
                println!("  Judging with {}...", judge_model);
            }
            let rating = rate::invoke_judge(
                &judge_prompt_path,
                &agent_result.output,
                &test_case.checklist,
                &test_case.name,
                agent_model,
                judge_model,
                timeout_secs.unwrap_or(300),
            )?;

            if !quiet {
                println!("  Result: {}/{} points", rating.points, rating.max_points);
            }
        }
    }

    Ok(())
}

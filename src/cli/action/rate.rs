//! Rate command handler.

use anyhow::Result;
use std::path::Path;

use crate::opencode::run::{get_default_model, run as opencode_run, RunOptions};
use crate::rate;

#[allow(clippy::too_many_arguments)]
pub fn run(
    instruction: &Path,
    test_path: &Path,
    agent_models: Option<&str>,
    judge_models: Option<&str>,
    agent_name: Option<&str>,
    timeout: Option<u64>,
    no_cache: bool,
    judge_prompt: Option<&Path>,
    quiet: bool,
) -> Result<()> {
    // Get default model from opencode config
    let default_model = get_default_model()?;

    // Parse models (CLI overrides opencode default)
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

        // Invoke agent using new opencode::run interface
        if !quiet {
            println!(
                "Running agent {} on test '{}'...",
                agent_model, test_case.name
            );
        }

        let options = RunOptions {
            model: Some(agent_model),
            agent: agent_name,
            timeout_secs,
            session_id: None,
            verbose: false,
            cache: !no_cache,
        };

        // Streaming callback for live display (when not quiet)
        let on_event = |json: &serde_json::Value| {
            if quiet {
                return;
            }
            let event_type = json.get("type").and_then(|v| v.as_str());
            match event_type {
                Some("text") => {
                    if let Some(text) = json
                        .get("part")
                        .and_then(|p| p.get("text"))
                        .and_then(|t| t.as_str())
                    {
                        for line in text.lines() {
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
                Some("tool_use") => {
                    if let Some(tool) = json
                        .get("part")
                        .and_then(|p| p.get("tool"))
                        .and_then(|t| t.as_str())
                    {
                        let desc = json
                            .get("part")
                            .and_then(|p| p.get("state"))
                            .and_then(|s| s.get("input"))
                            .and_then(|i| {
                                i.get("command")
                                    .or_else(|| i.get("description"))
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
                }
                _ => {}
            }
        };

        let agent_result = opencode_run(&prompt, instruction, &options, Some(on_event))?;

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

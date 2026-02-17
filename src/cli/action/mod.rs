//! Command action handlers.

mod activity;
mod list_sessions;
mod permissions;
mod rate;
mod transcript;

use anyhow::Result;

use super::def::Commands;

/// Resolve a session ID: use explicit value if given, else auto-detect.
fn resolve_session(explicit: Option<String>) -> Result<String> {
    match explicit {
        Some(id) => Ok(id),
        None => {
            let detected = crate::session_detect::detect_current_session()?;
            eprintln!(
                "Auto-detected session: {} ({:?})",
                detected.session_id, detected.provider
            );
            Ok(detected.session_id)
        }
    }
}

pub fn dispatch(cmd: Commands, quiet: bool, _verbose: u8) -> Result<()> {
    match cmd {
        Commands::Permissions { session, output } => permissions::run(&session, output.format()),
        Commands::ListSessions {
            session_type,
            session_id,
            search,
            timespan,
            project,
            output,
        } => list_sessions::run(
            session_type,
            session_id.as_deref(),
            search.as_deref(),
            timespan.as_deref(),
            project.as_deref(),
            output.format(),
            quiet,
        ),
        Commands::Transcript {
            session,
            last,
            output,
        } => {
            let session_id = resolve_session(session)?;
            transcript::run(&session_id, last, output.format(), _verbose)
        }
        Commands::CurrentSession {
            r#match,
            pid,
            session_type,
            last_messages,
            project,
            output,
        } => {
            let provider_filter = session_type.map(|t| match t {
                super::def::SessionType::OpenCode => crate::session_detect::Provider::OpenCode,
                super::def::SessionType::ClaudeCode => crate::session_detect::Provider::ClaudeCode,
            });
            let detected = if let Some(needle) = r#match {
                // Match-based detection: search recent messages for the given text
                crate::session_detect::find_session_by_match(
                    &crate::session_detect::MatchOptions {
                        needle,
                        last_messages,
                        provider_filter,
                        project_dir: project,
                    },
                )?
            } else if let Some(target_pid) = pid {
                // PID-based detection: examine /proc/<pid>/
                crate::session_detect::find_session_by_pid(target_pid, provider_filter)?
            } else {
                // Standard auto-detection (env vars, process tree, fingerprint)
                crate::session_detect::detect_current_session()?
            };
            let format = output.format();
            match format {
                crate::OutputFormat::Json => {
                    let provider = match detected.provider {
                        crate::session_detect::Provider::OpenCode => "opencode",
                        crate::session_detect::Provider::ClaudeCode => "claudecode",
                    };
                    println!(
                        "{}",
                        serde_json::json!({
                            "session_id": detected.session_id,
                            "provider": provider,
                        })
                    );
                }
                crate::OutputFormat::Nul => {
                    print!("{}\0", detected.session_id);
                }
                crate::OutputFormat::Human => {
                    println!("{}", detected.session_id);
                }
            }
            Ok(())
        }
        Commands::Activity { action } => activity::run(action),
        Commands::Rate {
            instruction,
            test,
            agent_models,
            judge_models,
            agent_name,
            timeout,
            no_cache,
            judge_prompt,
        } => rate::run(
            &instruction,
            &test,
            agent_models.as_deref(),
            judge_models.as_deref(),
            agent_name.as_deref(),
            timeout,
            no_cache,
            judge_prompt.as_deref(),
            quiet,
        ),
    }
}

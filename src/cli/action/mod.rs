//! Command action handlers.

mod activity;
mod last_session;
mod list_sessions;
mod permissions;
mod rate;
mod transcript;
mod usage;

use anyhow::Result;

use super::def::Commands;

/// Resolve a session ID: use explicit value if given, else auto-detect.
fn resolve_session(explicit: Option<String>) -> Result<String> {
    match explicit {
        Some(id) => Ok(id),
        None => {
            let detected = crate::session_detect::detect_current_session()?;
            log::info!(
                "Auto-detected session: {} ({:?})",
                detected.session_id,
                detected.provider
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
            file,
            all,
            children_of,
            output,
        } => list_sessions::run(
            session_type,
            session_id.as_deref(),
            search.as_deref(),
            timespan.as_deref(),
            project.as_deref(),
            file.as_deref(),
            all,
            children_of.as_deref(),
            output.format(),
            quiet,
        ),
        Commands::Transcript {
            session,
            last,
            file,
            output,
        } => {
            let session_id = resolve_session(session)?;
            transcript::run(
                &session_id,
                last,
                file.as_deref(),
                output.format(),
                _verbose,
            )
        }
        Commands::CurrentSession {
            r#match,
            session_type,
            last_messages,
            project,
            output,
        } => {
            let provider_filter = session_type.map(|t| match t {
                super::def::SessionType::OpenCode => crate::provider::Provider::OpenCode,
                super::def::SessionType::ClaudeCode => crate::provider::Provider::ClaudeCode,
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
            } else {
                // Standard auto-detection (env vars, process tree, tmux pane matching)
                crate::session_detect::detect_current_session()?
            };
            let format = output.format();
            match format {
                crate::OutputFormat::Json => {
                    println!(
                        "{}",
                        serde_json::json!({
                            "session_id": detected.session_id,
                            "provider": detected.provider.as_str(),
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
        Commands::LastSession {
            session_type,
            output,
        } => last_session::run(session_type, output.format()),
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
        Commands::Usage {
            session,
            session_type,
            timespan,
            project,
            output,
        } => usage::run(
            session,
            session_type,
            timespan.as_deref(),
            project.as_deref(),
            output.format(),
            quiet,
        ),
    }
}

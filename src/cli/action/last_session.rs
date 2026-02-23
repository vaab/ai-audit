//! Last session detection handler.

use anyhow::Result;

use crate::session_detect;
use crate::OutputFormat;

pub fn run(
    session_type: Option<super::super::def::SessionType>,
    project: Option<String>,
    format: OutputFormat,
) -> Result<()> {
    let provider_filter = session_type.map(|t| match t {
        super::super::def::SessionType::OpenCode => crate::provider::Provider::OpenCode,
        super::super::def::SessionType::ClaudeCode => crate::provider::Provider::ClaudeCode,
    });

    let detected = session_detect::detect_last_session(&session_detect::LastSessionOptions {
        provider_filter,
        project_dir: project,
    })?;

    match format {
        OutputFormat::Json => {
            println!(
                "{}",
                serde_json::json!({
                    "session_id": detected.session_id,
                    "provider": detected.provider.as_str(),
                })
            );
        }
        OutputFormat::Nul => {
            print!("{}\0", detected.session_id);
        }
        OutputFormat::Human => {
            println!("{}", detected.session_id);
        }
    }

    Ok(())
}

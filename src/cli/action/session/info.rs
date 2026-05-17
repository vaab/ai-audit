//! `ai-audit session info <session-id>` — one-shot session metadata.
//!
//! Resolves the provider from the session id format, fetches the
//! detailed info via the provider's `info::fetch_info`, optionally
//! probes live status (OpenCode only) and renders human / JSON
//! output.
//!
//! Spec: `doc/admin.org § ai-audit / session info / single-shot session metadata`.

use anyhow::Result;
use chrono::{DateTime, SecondsFormat, Utc};
use serde::Serialize;

use crate::cli::def::SessionInfoArgs;
use crate::config::Config;
use crate::opencode::info::SessionDetailInfo;
use crate::opencode::server_client::{
    compute_live, resolve_server_credentials, LiveStatus, ServerClient,
};
use crate::provider::{detect_provider, Provider};
use crate::OutputFormat;

pub fn run(args: SessionInfoArgs) -> Result<()> {
    let session_id = resolve_session(args.session.clone())?;
    let provider = detect_provider(&session_id)?;

    let info = match provider {
        Provider::OpenCode => crate::opencode::info::fetch_info(&session_id)?,
        Provider::ClaudeCode => crate::claudecode::info::fetch_info(&session_id)?,
        Provider::Pi => crate::pi::info::fetch_info(&session_id)?,
    };

    let live = if matches!(provider, Provider::OpenCode) && !args.no_live {
        Some(probe_live_status(
            &session_id,
            args.server_url.as_deref(),
            args.server_password.as_deref(),
        ))
    } else {
        None
    };

    match args.output.format() {
        OutputFormat::Json => render_json(&info, live)?,
        OutputFormat::Human | OutputFormat::Nul => render_human(&info, live),
    }
    Ok(())
}

/// Resolve the session id, auto-detecting when omitted.
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

/// Probe the live opencode server for this session's status.
///
/// Errors and unreachable servers degrade to `ServerUnreachable`
/// (per the spec: live-status must never abort the whole command).
fn probe_live_status(
    session_id: &str,
    server_url: Option<&str>,
    server_password: Option<&str>,
) -> LiveStatus {
    let config = match Config::load() {
        Ok(c) => c,
        Err(_) => Config::default(),
    };
    let creds = resolve_server_credentials(server_url, server_password, &config);
    let client = ServerClient::new(creds);
    match client.session_status() {
        Ok(Some(map)) => compute_live(session_id, Some(&map), false),
        Ok(None) => LiveStatus::ServerUnreachable,
        Err(_) => LiveStatus::ServerUnreachable,
    }
}

fn render_human(info: &SessionDetailInfo, live: Option<LiveStatus>) {
    println!("Session:        {}", info.session_id);
    println!("Type:           {}", info.provider.as_str());
    println!(
        "Project:        {}",
        info.project_dir.as_deref().unwrap_or("(none)")
    );
    println!(
        "Title:          {}",
        info.title.as_deref().unwrap_or("(no title)")
    );
    println!("Started:        {}", fmt_ts(info.started_at));
    let updated = fmt_ts(info.last_updated_at);
    let aborted_suffix = if info.aborted { "  (aborted)" } else { "" };
    println!("Last updated:   {}{}", updated, aborted_suffix);
    println!("Messages:       {}", info.message_count);
    println!("Tool calls:     {}", info.tool_call_count);
    if let Some(status) = info.static_status {
        println!("Static status:  {}", status.as_str());
    }
    if let Some(live) = live {
        println!("Live status:    {}", live.as_str());
    }
    println!(
        "Parent:         {}",
        info.parent_session_id.as_deref().unwrap_or("(none)")
    );
    if matches!(info.provider, Provider::OpenCode) {
        println!(
            "Agent:          {}",
            info.agent.as_deref().unwrap_or("(none)")
        );
    }
    println!(
        "Model:          {}",
        info.model.as_deref().unwrap_or("(none)")
    );
    println!();
    println!("See also: ai-audit usage {}", info.session_id);
}

/// Stable JSON shape (acceptance-tested).  Field names are
/// `snake_case`; timestamps render as RFC3339 UTC.
#[derive(Serialize)]
struct JsonOutput<'a> {
    session_id: &'a str,
    provider: &'a str,
    project_dir: Option<&'a str>,
    title: Option<&'a str>,
    started_at: Option<String>,
    last_updated_at: Option<String>,
    message_count: usize,
    tool_call_count: usize,
    static_status: Option<&'static str>,
    live_status: Option<&'static str>,
    aborted: bool,
    parent_session_id: Option<&'a str>,
    agent: Option<&'a str>,
    model: Option<&'a str>,
}

fn render_json(info: &SessionDetailInfo, live: Option<LiveStatus>) -> Result<()> {
    let payload = JsonOutput {
        session_id: &info.session_id,
        provider: info.provider.as_str(),
        project_dir: info.project_dir.as_deref(),
        title: info.title.as_deref(),
        started_at: info.started_at.map(rfc3339_z),
        last_updated_at: info.last_updated_at.map(rfc3339_z),
        message_count: info.message_count,
        tool_call_count: info.tool_call_count,
        static_status: info.static_status.map(|s| s.as_str()),
        live_status: live.map(|s| s.as_str()),
        aborted: info.aborted,
        parent_session_id: info.parent_session_id.as_deref(),
        agent: info.agent.as_deref(),
        model: info.model.as_deref(),
    };
    println!("{}", serde_json::to_string(&payload)?);
    Ok(())
}

fn fmt_ts(ts: Option<DateTime<Utc>>) -> String {
    match ts {
        Some(dt) => dt.to_rfc3339_opts(SecondsFormat::Secs, true),
        None => "(unknown)".to_string(),
    }
}

fn rfc3339_z(dt: DateTime<Utc>) -> String {
    dt.to_rfc3339_opts(SecondsFormat::Secs, true)
}

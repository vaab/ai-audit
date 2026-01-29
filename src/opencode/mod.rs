pub mod permissions;

use anyhow::{Context, Result};
use chrono::{DateTime, TimeZone, Utc};
use serde::Deserialize;
use std::fs;
use std::path::PathBuf;

pub fn storage_dir() -> PathBuf {
    crate::opencode_data_dir().join("storage/directory-agents")
}

pub fn part_dir() -> PathBuf {
    crate::opencode_data_dir().join("storage/part")
}

pub fn log_dir() -> PathBuf {
    crate::opencode_data_dir().join("log")
}

pub fn get_session_info(session_id: &str) -> Result<SessionInfo> {
    let storage_dir = storage_dir();
    let session_file = storage_dir.join(format!("{}.json", session_id));

    let content = fs::read_to_string(&session_file)
        .with_context(|| format!("Session file not found: {}", session_file.display()))?;

    let session_data: SessionFile =
        serde_json::from_str(&content).context("Failed to parse session file")?;

    let timestamp = Utc
        .timestamp_millis_opt(session_data.updated_at)
        .single()
        .unwrap_or_else(Utc::now);

    Ok(SessionInfo {
        session_id: session_data.session_id,
        timestamp,
    })
}

#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub session_id: String,
    pub timestamp: DateTime<Utc>,
}

#[derive(Deserialize)]
struct SessionFile {
    #[serde(rename = "sessionID")]
    session_id: String,
    #[serde(rename = "updatedAt")]
    updated_at: i64,
}

pub fn list_sessions() -> Result<Vec<SessionInfo>> {
    let storage_dir = storage_dir();
    if !storage_dir.exists() {
        return Ok(Vec::new());
    }

    let mut sessions = Vec::new();

    for entry in fs::read_dir(&storage_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().map_or(false, |e| e == "json") {
            if let Ok(content) = fs::read_to_string(&path) {
                if let Ok(session_file) = serde_json::from_str::<SessionFile>(&content) {
                    let timestamp = Utc
                        .timestamp_millis_opt(session_file.updated_at)
                        .single()
                        .unwrap_or_else(Utc::now);
                    sessions.push(SessionInfo {
                        session_id: session_file.session_id,
                        timestamp,
                    });
                }
            }
        }
    }

    sessions.sort_by_key(|s| s.timestamp);
    Ok(sessions)
}

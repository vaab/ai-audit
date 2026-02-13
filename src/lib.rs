pub mod activity;
pub mod claudecode;
pub mod cli;
pub mod config;
pub mod opencode;
pub mod rate;
pub mod session_detect;
pub mod transcript;

use std::path::PathBuf;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OutputFormat {
    #[default]
    Human,
    Nul,
    Json,
}

/// Default Claude Code data directory
pub fn claudecode_data_dir() -> PathBuf {
    dirs::home_dir()
        .expect("Could not find home directory")
        .join(".claude")
}

/// Default OpenCode data directory
pub fn opencode_data_dir() -> PathBuf {
    dirs::home_dir()
        .expect("Could not find home directory")
        .join(".local/share/opencode")
}

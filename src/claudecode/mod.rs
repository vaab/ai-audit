pub mod permissions;
pub mod session;

use std::path::PathBuf;

pub fn debug_dir() -> PathBuf {
    crate::claudecode_data_dir().join("debug")
}

pub fn projects_dir() -> PathBuf {
    crate::claudecode_data_dir().join("projects")
}

pub fn resolve_debug_file(session_id: &str) -> PathBuf {
    debug_dir().join(format!("{}.txt", session_id))
}

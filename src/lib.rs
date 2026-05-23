pub mod activity;
pub mod claudecode;
pub mod cli;
pub mod config;
pub mod empty_segments;
pub mod format;
pub mod opencode;
pub mod pi;
pub mod project;
pub mod provider;
pub mod rate;
pub mod session_detect;
pub mod session_filter;
pub mod session_index;
pub mod transcript;

/// Crate-wide mutex serializing tests that mutate process-global
/// environment variables (``HOME``, ``XDG_CACHE_HOME``,
/// ``PI_CODING_AGENT_DIR``, ``*_SESSION_ID``).
///
/// Several test modules (claudecode / opencode / pi / activity /
/// session-delete) need to redirect ``HOME`` to a tempdir to exercise
/// data-dir helpers in isolation.  Without a SHARED lock, two tests
/// from different modules can race and end up reading each other's
/// HOME, corrupting fixtures.  This lock makes any HOME-mutating
/// test critical section globally serial, which is the same trade-off
/// the existing ``activity.rs`` ENV_LOCK was already making but in a
/// module-local way.
///
/// Acquire it as the FIRST line of any test that calls
/// ``unsafe { std::env::set_var("HOME", ...) }`` (or similar).
#[cfg(test)]
pub static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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

/// Check if a tool's file_path argument matches the target path.
///
/// - If `tool_path` is absolute: exact string equality with `target_path`
/// - If `tool_path` is relative: check if `target_path` ends with `tool_path`
///   using path component comparison (not string suffix)
pub fn file_path_matches(tool_path: &str, target_path: &str) -> bool {
    let tool = std::path::Path::new(tool_path);
    if tool.is_absolute() {
        tool_path == target_path
    } else {
        // Component-based suffix match: target must end with all components of tool_path
        let target_components: Vec<_> = std::path::Path::new(target_path).components().collect();
        let tool_components: Vec<_> = tool.components().collect();
        if tool_components.is_empty() || tool_components.len() > target_components.len() {
            return false;
        }
        let offset = target_components.len() - tool_components.len();
        target_components[offset..] == tool_components[..]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_file_path_matches_absolute_exact() {
        assert!(file_path_matches(
            "/home/user/src/main.rs",
            "/home/user/src/main.rs"
        ));
        assert!(!file_path_matches(
            "/home/user/src/main.rs",
            "/home/user/src/lib.rs"
        ));
    }

    #[test]
    fn test_file_path_matches_relative_suffix() {
        assert!(file_path_matches(
            "src/main.rs",
            "/home/user/project/src/main.rs"
        ));
        assert!(file_path_matches(
            "main.rs",
            "/home/user/project/src/main.rs"
        ));
        assert!(!file_path_matches(
            "src/lib.rs",
            "/home/user/project/src/main.rs"
        ));
    }

    #[test]
    fn test_file_path_matches_no_partial_component() {
        // "rs/main.rs" should NOT match because "rs" != "src"
        assert!(!file_path_matches(
            "rs/main.rs",
            "/home/user/project/src/main.rs"
        ));
        // But "src/main.rs" should match
        assert!(file_path_matches(
            "src/main.rs",
            "/home/user/project/src/main.rs"
        ));
    }

    #[test]
    fn test_file_path_matches_empty_tool_path() {
        assert!(!file_path_matches("", "/home/user/src/main.rs"));
    }

    #[test]
    fn test_file_path_matches_single_filename() {
        assert!(file_path_matches(
            "main.rs",
            "/home/user/project/src/main.rs"
        ));
        assert!(!file_path_matches(
            "lib.rs",
            "/home/user/project/src/main.rs"
        ));
    }
}

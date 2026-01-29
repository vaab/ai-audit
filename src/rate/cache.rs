use anyhow::Result;
use std::fs;
use std::path::Path;

/// Check if a cached report should be skipped.
///
/// Returns true if the report file exists and caching is enabled (no_cache=false).
/// Returns false if no_cache flag is set or file doesn't exist.
pub fn should_skip_cached(report_path: &Path, no_cache: bool) -> bool {
    if no_cache {
        // If no_cache flag is set, never skip (always recompute)
        return false;
    }

    // If file exists and caching is enabled, skip (return true)
    report_path.exists()
}

/// Ensure the parent directory of report_path exists.
///
/// Creates all parent directories if they don't exist.
pub fn ensure_report_dir(report_path: &Path) -> Result<()> {
    if let Some(parent) = report_path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use tempfile::TempDir;

    #[test]
    fn test_should_skip_cached_no_cache_flag() {
        let temp_dir = TempDir::new().unwrap();
        let report_path = temp_dir.path().join("report.json");

        // Create the file
        File::create(&report_path).unwrap();

        // With no_cache=true, should always return false (never skip)
        assert!(!should_skip_cached(&report_path, true));
    }

    #[test]
    fn test_should_skip_cached_file_exists() {
        let temp_dir = TempDir::new().unwrap();
        let report_path = temp_dir.path().join("report.json");

        // Create the file
        File::create(&report_path).unwrap();

        // With no_cache=false and file exists, should return true (skip)
        assert!(should_skip_cached(&report_path, false));
    }

    #[test]
    fn test_should_skip_cached_file_not_exists() {
        let temp_dir = TempDir::new().unwrap();
        let report_path = temp_dir.path().join("report.json");

        // File doesn't exist
        // With no_cache=false and file doesn't exist, should return false (don't skip)
        assert!(!should_skip_cached(&report_path, false));
    }

    #[test]
    fn test_ensure_report_dir() {
        let temp_dir = TempDir::new().unwrap();
        let nested_path = temp_dir
            .path()
            .join("a")
            .join("b")
            .join("c")
            .join("report.json");

        // Ensure directory creation succeeds
        ensure_report_dir(&nested_path).unwrap();

        // Verify parent directories were created
        assert!(nested_path.parent().unwrap().exists());
    }

    #[test]
    fn test_ensure_report_dir_already_exists() {
        let temp_dir = TempDir::new().unwrap();
        let report_path = temp_dir.path().join("report.json");

        // Call twice - should not fail
        ensure_report_dir(&report_path).unwrap();
        ensure_report_dir(&report_path).unwrap();

        assert!(report_path.parent().unwrap().exists());
    }
}

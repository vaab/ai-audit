use anyhow::{anyhow, Context, Result};
use std::path::Path;
use std::process::Command;

/// Compute git blob SHA1 hash for a file using `git hash-object`.
///
/// Returns a 40-character hexadecimal SHA1 hash compatible with `git cat-file -p`.
///
/// # Arguments
/// * `path` - Path to the file to hash
///
/// # Returns
/// * `Ok(String)` - 40-character hex SHA1 hash
/// * `Err` - If git is not found, file doesn't exist, or output is invalid
pub fn git_hash_file(path: &Path) -> Result<String> {
    // Convert path to string for error messages
    let path_str = path
        .to_str()
        .ok_or_else(|| anyhow!("Path contains invalid UTF-8: {:?}", path))?;

    // Run git hash-object
    let output = Command::new("git")
        .args(["hash-object", path_str])
        .output()
        .context("Failed to execute 'git hash-object' - is git installed?")?;

    // Check if command succeeded
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!(
            "git hash-object failed for {}: {}",
            path_str,
            stderr.trim()
        ));
    }

    // Parse stdout and validate format
    let hash = String::from_utf8(output.stdout)
        .context("git hash-object output is not valid UTF-8")?
        .trim()
        .to_string();

    // Validate: must be exactly 40 hex characters
    if hash.len() != 40 {
        return Err(anyhow!(
            "Invalid hash length: expected 40 chars, got {}",
            hash.len()
        ));
    }

    if !hash.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(anyhow!(
            "Invalid hash format: contains non-hex characters: {}",
            hash
        ));
    }

    Ok(hash)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_git_hash_format() -> Result<()> {
        // Create a temporary file with known content
        let mut temp_file = NamedTempFile::new()?;
        temp_file.write_all(b"test content")?;
        temp_file.flush()?;

        // Hash the file
        let hash = git_hash_file(temp_file.path())?;

        // Verify format: 40 hex characters
        assert_eq!(hash.len(), 40, "Hash should be 40 characters");
        assert!(
            hash.chars().all(|c| c.is_ascii_hexdigit()),
            "Hash should contain only hex characters"
        );

        Ok(())
    }

    #[test]
    fn test_git_hash_nonexistent_file() {
        let result = git_hash_file(Path::new("/nonexistent/path/to/file.txt"));
        assert!(result.is_err(), "Should fail for nonexistent file");
    }

    #[test]
    fn test_git_hash_consistency() -> Result<()> {
        // Create a temporary file
        let mut temp_file = NamedTempFile::new()?;
        temp_file.write_all(b"consistent content")?;
        temp_file.flush()?;

        // Hash the same file twice
        let hash1 = git_hash_file(temp_file.path())?;
        let hash2 = git_hash_file(temp_file.path())?;

        // Hashes should be identical
        assert_eq!(hash1, hash2, "Same file should produce same hash");

        Ok(())
    }
}

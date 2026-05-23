//! Cache for opencode run outputs.
//!
//! Caches raw opencode outputs keyed by agent, model, instruction hash, and prompt hash.
//! Cache location: `~/.cache/ai-audit/opencode/run/`

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

/// Token usage from an opencode run.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
    #[serde(default)]
    pub reasoning: u64,
    #[serde(default)]
    pub cache_read: u64,
    #[serde(default)]
    pub cache_write: u64,
}

/// Cached run result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedResult {
    /// Raw output from opencode (stdout).
    pub output: String,
    /// Session ID if captured.
    pub session_id: Option<String>,
    /// Agent name extracted from stderr.
    #[serde(default)]
    pub agent: Option<String>,
    /// Timestamp when cached.
    pub cached_at: String,
    /// Execution time in seconds.
    #[serde(default)]
    pub execution_time_secs: f64,
    /// Token usage.
    #[serde(default)]
    pub tokens: TokenUsage,
}

/// Get the cache directory for opencode run outputs.
pub fn cache_dir() -> Result<PathBuf> {
    let cache_base =
        dirs::cache_dir().ok_or_else(|| anyhow!("Could not determine cache directory"))?;
    Ok(cache_base.join("ai-audit/opencode/run"))
}

/// Compute git blob SHA1 hash for a string.
///
/// Uses `git hash-object --stdin` to compute the hash.
pub fn git_hash_string(content: &str) -> Result<String> {
    let mut child = Command::new("git")
        .args(["hash-object", "--stdin"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("Failed to execute 'git hash-object' - is git installed?")?;

    // Write content to stdin
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(content.as_bytes())
            .context("Failed to write to git hash-object stdin")?;
    }

    let output = child
        .wait_with_output()
        .context("Failed to wait for git hash-object")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("git hash-object failed: {}", stderr.trim()));
    }

    let hash = String::from_utf8(output.stdout)
        .context("git hash-object output is not valid UTF-8")?
        .trim()
        .to_string();

    // Validate: must be exactly 40 hex characters
    if hash.len() != 40 || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(anyhow!("Invalid hash format: {}", hash));
    }

    Ok(hash)
}

/// Compute git blob SHA1 hash for a file.
pub fn git_hash_file(path: &Path) -> Result<String> {
    let path_str = path
        .to_str()
        .ok_or_else(|| anyhow!("Path contains invalid UTF-8: {:?}", path))?;

    let output = Command::new("git")
        .args(["hash-object", path_str])
        .output()
        .context("Failed to execute 'git hash-object' - is git installed?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!(
            "git hash-object failed for {}: {}",
            path_str,
            stderr.trim()
        ));
    }

    let hash = String::from_utf8(output.stdout)
        .context("git hash-object output is not valid UTF-8")?
        .trim()
        .to_string();

    if hash.len() != 40 || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(anyhow!("Invalid hash format: {}", hash));
    }

    Ok(hash)
}

/// Sanitize a string for use in filenames.
fn sanitize_for_filename(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Build a cache key from run parameters.
///
/// Format: `{AGENT}-{MODEL}-{INSTRUCTION_HASH_8}-{PROMPT_HASH_8}.json`
/// When no agent is specified, uses "sisyphus" (opencode's default primary agent).
pub fn build_cache_key(
    agent: Option<&str>,
    model: &str,
    instruction_hash: &str,
    prompt_hash: &str,
) -> String {
    let agent_part = sanitize_for_filename(agent.unwrap_or("sisyphus"));
    let model_part = sanitize_for_filename(model);
    let instruction_short = &instruction_hash[..8.min(instruction_hash.len())];
    let prompt_short = &prompt_hash[..8.min(prompt_hash.len())];

    format!(
        "{}-{}-{}-{}.json",
        agent_part, model_part, instruction_short, prompt_short
    )
}

/// Get the full cache file path for given parameters.
pub fn cache_path(
    agent: Option<&str>,
    model: &str,
    instruction_hash: &str,
    prompt_hash: &str,
) -> Result<PathBuf> {
    let dir = cache_dir()?;
    let key = build_cache_key(agent, model, instruction_hash, prompt_hash);
    Ok(dir.join(key))
}

/// Read a cached result if it exists.
pub fn read_cache(
    agent: Option<&str>,
    model: &str,
    instruction_hash: &str,
    prompt_hash: &str,
) -> Result<Option<CachedResult>> {
    let path = cache_path(agent, model, instruction_hash, prompt_hash)?;

    if !path.exists() {
        return Ok(None);
    }

    let content = fs::read_to_string(&path)
        .with_context(|| format!("Failed to read cache file: {:?}", path))?;

    let cached: CachedResult = serde_json::from_str(&content)
        .with_context(|| format!("Failed to parse cache file: {:?}", path))?;

    log::debug!("Cache hit: {:?}", path);
    Ok(Some(cached))
}

/// Write a result to cache.
pub fn write_cache(
    agent: Option<&str>,
    model: &str,
    instruction_hash: &str,
    prompt_hash: &str,
    output: &str,
    session_id: Option<&str>,
    captured_agent: Option<&str>,
    execution_time_secs: f64,
    tokens: &TokenUsage,
) -> Result<()> {
    let path = cache_path(agent, model, instruction_hash, prompt_hash)?;

    // Ensure cache directory exists
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create cache directory: {:?}", parent))?;
    }

    let cached = CachedResult {
        output: output.to_string(),
        session_id: session_id.map(|s| s.to_string()),
        agent: captured_agent.map(|s| s.to_string()),
        cached_at: chrono::Utc::now().to_rfc3339(),
        execution_time_secs,
        tokens: tokens.clone(),
    };

    let content =
        serde_json::to_string_pretty(&cached).context("Failed to serialize cache content")?;

    // Atomic write: temp file + rename
    let temp_path = path.with_extension("json.tmp");
    {
        let mut file = fs::File::create(&temp_path)
            .with_context(|| format!("Failed to create temp cache file: {:?}", temp_path))?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
    }
    fs::rename(&temp_path, &path)
        .with_context(|| format!("Failed to rename cache file: {:?} -> {:?}", temp_path, path))?;

    log::debug!("Cached result to: {:?}", path);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ``git hash-object`` consults ``~/.gitconfig`` and the user's
    // git object cache; if another test concurrently redirects HOME,
    // those lookups fail.  Serialize against the shared lock.
    use crate::TEST_ENV_LOCK as ENV_LOCK;

    #[test]
    fn test_git_hash_string() {
        let _lock = ENV_LOCK.lock().unwrap();
        let hash = git_hash_string("test content").unwrap();
        assert_eq!(hash.len(), 40);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_git_hash_string_consistency() {
        let _lock = ENV_LOCK.lock().unwrap();
        let hash1 = git_hash_string("same content").unwrap();
        let hash2 = git_hash_string("same content").unwrap();
        assert_eq!(hash1, hash2);
    }

    #[test]
    fn test_git_hash_string_different() {
        let _lock = ENV_LOCK.lock().unwrap();
        let hash1 = git_hash_string("content a").unwrap();
        let hash2 = git_hash_string("content b").unwrap();
        assert_ne!(hash1, hash2);
    }

    #[test]
    fn test_sanitize_for_filename() {
        assert_eq!(sanitize_for_filename("hello-world"), "hello-world");
        assert_eq!(sanitize_for_filename("hello/world"), "hello_world");
        assert_eq!(sanitize_for_filename("a:b:c"), "a_b_c");
        assert_eq!(
            sanitize_for_filename("anthropic/claude-sonnet-4"),
            "anthropic_claude-sonnet-4"
        );
    }

    #[test]
    fn test_build_cache_key() {
        let key = build_cache_key(
            Some("my-agent"),
            "anthropic/claude-sonnet-4",
            "a1b2c3d4e5f6g7h8",
            "11223344aabbccdd",
        );
        assert_eq!(
            key,
            "my-agent-anthropic_claude-sonnet-4-a1b2c3d4-11223344.json"
        );
    }

    #[test]
    fn test_build_cache_key_no_agent() {
        let key = build_cache_key(None, "openai/gpt-4", "abcdef1234567890", "fedcba0987654321");
        assert_eq!(key, "sisyphus-openai_gpt-4-abcdef12-fedcba09.json");
    }
}

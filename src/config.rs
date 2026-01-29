use anyhow::{Context, Result};
use regex::Regex;
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

/// A compiled path simplification rule (regex pattern -> replacement)
#[derive(Debug)]
pub struct PathRule {
    pub pattern: Regex,
    pub replacement: String,
}

#[derive(Debug, Deserialize, Default)]
pub struct Config {
    /// Path simplification rules: regex pattern -> replacement
    /// Applied sequentially, re-applied from start until no more matches
    #[serde(rename = "path-rules", default)]
    path_rules_raw: Vec<HashMap<String, String>>,

    /// Compiled path rules
    #[serde(skip)]
    path_rules: Vec<PathRule>,
}

impl Config {
    /// Load config from ~/.config/ai-audit/config.yml
    /// Returns default config if file doesn't exist
    pub fn load() -> Result<Self> {
        let config_path = Self::config_path()?;

        if !config_path.exists() {
            return Ok(Self::default());
        }

        let content = fs::read_to_string(&config_path)
            .with_context(|| format!("Failed to read config file: {}", config_path.display()))?;

        let mut config: Config = serde_yaml::from_str(&content)
            .with_context(|| format!("Failed to parse config file: {}", config_path.display()))?;

        config.compile_rules()?;
        Ok(config)
    }

    /// Get the config file path
    pub fn config_path() -> Result<PathBuf> {
        let config_dir = dirs::config_dir().context("Could not find config directory")?;
        Ok(config_dir.join("ai-audit").join("config.yml"))
    }

    /// Compile regex rules from raw config
    fn compile_rules(&mut self) -> Result<()> {
        // Expand ~ in patterns and compile regexes
        let home = dirs::home_dir()
            .map(|h| h.to_string_lossy().to_string())
            .unwrap_or_default();
        let home_escaped = regex::escape(&home);

        let mut rules = Vec::new();
        for rule_map in &self.path_rules_raw {
            // Each map should have exactly one key-value pair
            for (pattern, replacement) in rule_map {
                // Expand ~ to home directory (escaped for regex)
                let expanded_pattern = pattern
                    .replace("^~/", &format!("^{}/", home_escaped))
                    .replace("~/", &format!("{}/", home_escaped));

                match Regex::new(&expanded_pattern) {
                    Ok(regex) => {
                        rules.push(PathRule {
                            pattern: regex,
                            replacement: replacement.clone(),
                        });
                    }
                    Err(e) => {
                        eprintln!("Warning: Invalid regex pattern '{}': {}", pattern, e);
                    }
                }
            }
        }

        self.path_rules = rules;
        Ok(())
    }

    /// Get compiled path rules
    pub fn path_rules(&self) -> &[PathRule] {
        &self.path_rules
    }

    /// Simplify a project path using configured rules
    /// Example: /home/user/dev/rs/ai-audit -> rs/ai-audit
    pub fn simplify_path(&self, path: &str) -> String {
        let mut result = path.to_string();

        // Apply rules sequentially, re-apply from start until no more matches
        let max_iterations = 100; // Safety limit
        for _ in 0..max_iterations {
            let mut matched = false;

            for rule in &self.path_rules {
                if rule.pattern.is_match(&result) {
                    let new_result = rule.pattern.replace_all(&result, &rule.replacement);
                    if new_result != result {
                        result = new_result.to_string();
                        matched = true;
                        break; // Re-apply from start
                    }
                }
            }

            if !matched {
                break;
            }
        }

        result
    }

    /// Create a config with specific rules (for testing)
    #[cfg(test)]
    pub fn with_rules(rules: Vec<(&str, &str)>) -> Result<Self> {
        let home = dirs::home_dir()
            .map(|h| h.to_string_lossy().to_string())
            .unwrap_or_default();
        let home_escaped = regex::escape(&home);

        let mut compiled_rules = Vec::new();
        for (pattern, replacement) in rules {
            let expanded_pattern = pattern
                .replace("^~/", &format!("^{}/", home_escaped))
                .replace("~/", &format!("{}/", home_escaped));

            let regex = Regex::new(&expanded_pattern)?;
            compiled_rules.push(PathRule {
                pattern: regex,
                replacement: replacement.to_string(),
            });
        }

        Ok(Self {
            path_rules_raw: Vec::new(),
            path_rules: compiled_rules,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simplify_path_chained_rules() {
        // Rules should re-apply from start after each match
        // Test with suffix removal that enables a prefix rule to match
        let config = Config::with_rules(vec![
            ("/src$", ""),      // Remove /src suffix first
            ("^/app/", "APP>"), // Then this can match
        ])
        .unwrap();

        assert_eq!(config.simplify_path("/app/project/src"), "APP>project");
    }

    #[test]
    fn test_simplify_path_single_rule() {
        let config = Config::with_rules(vec![("^/home/user/dev/", "DEV>")]).unwrap();
        assert_eq!(
            config.simplify_path("/home/user/dev/rust/project"),
            "DEV>rust/project"
        );
    }

    #[test]
    fn test_simplify_path_no_match() {
        let config = Config::with_rules(vec![("^/home/user/dev/", "DEV>")]).unwrap();
        assert_eq!(config.simplify_path("/other/path"), "/other/path");
    }

    #[test]
    fn test_simplify_path_multiple_rules() {
        let config = Config::with_rules(vec![
            ("^/home/user/dev/", "DEV>"),
            ("^/home/user/work/", "WORK>"),
        ])
        .unwrap();

        assert_eq!(
            config.simplify_path("/home/user/dev/project"),
            "DEV>project"
        );
        assert_eq!(
            config.simplify_path("/home/user/work/project"),
            "WORK>project"
        );
    }

    #[test]
    fn test_simplify_path_suffix_removal() {
        let config = Config::with_rules(vec![("/admin$", "")]).unwrap();
        assert_eq!(config.simplify_path("/some/path/admin"), "/some/path");
        assert_eq!(config.simplify_path("/some/admin/path"), "/some/admin/path");
    }

    #[test]
    fn test_simplify_path_with_home_expansion() {
        let home = dirs::home_dir().unwrap();
        let config = Config::with_rules(vec![("^~/dev/", "DEV>")]).unwrap();

        let path = format!("{}/dev/project", home.display());
        assert_eq!(config.simplify_path(&path), "DEV>project");
    }
}

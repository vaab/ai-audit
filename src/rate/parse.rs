//! Parser for test case .md files.

use anyhow::{bail, Context, Result};
use std::fs;
use std::path::Path;

/// Represents a timespan extracted from a test case header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Timespan {
    /// Date in "YYYY-MM-DD" format
    pub date: String,
    /// Start time in "HH:MM:SS" format
    pub start: String,
    /// End time in "HH:MM:SS" format
    pub end: String,
    /// Full timespan "YYYY-MM-DD HH:MM:SS..HH:MM:SS"
    pub full: String,
}

/// Represents a parsed test case from a .md file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestCase {
    /// Name of the test case
    pub name: String,
    /// Parsed timespan
    pub timespan: Timespan,
    /// Prompt template (with {{TIMESPAN}}, {{DATE}}, etc. variables)
    pub prompt: String,
    /// List of checklist items (checkbox text without the `- [ ]` prefix)
    pub checklist: Vec<String>,
}

/// Parse a test file and extract the test case information.
///
/// # Arguments
/// * `path` - Path to the .md test file
///
/// # Errors
/// Returns an error if:
/// - File not found
/// - Missing Test Case header
/// - Malformed timespan
/// - Missing Input Parameters section
/// - Missing Verification Checklist
pub fn parse_test_file(path: &Path) -> Result<TestCase> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("Failed to read test file: {}", path.display()))?;

    parse_test_content(&content)
}

/// Parse test content string (used by parse_test_file and tests).
fn parse_test_content(content: &str) -> Result<TestCase> {
    let (name, timespan) = parse_header(content)?;
    let prompt = parse_prompt(content)?;
    let checklist = parse_checklist(content)?;

    Ok(TestCase {
        name,
        timespan,
        prompt,
        checklist,
    })
}

/// Parse the test case header line.
///
/// Format: `## Test Case: NAME | DATE TIME..TIME`
fn parse_header(content: &str) -> Result<(String, Timespan)> {
    for line in content.lines() {
        if line.starts_with("## Test Case:") {
            // Line format: "## Test Case: NAME | DATE TIME..TIME"
            let rest = line.strip_prefix("## Test Case:").unwrap().trim();

            let parts: Vec<&str> = rest.splitn(2, '|').collect();
            if parts.len() != 2 {
                bail!("Malformed Test Case header: missing '|' separator");
            }

            let name = parts[0].trim().to_string();
            let timespan_str = parts[1].trim();

            // Parse timespan: "DATE TIME..TIME"
            let timespan = parse_timespan(timespan_str)?;

            return Ok((name, timespan));
        }
    }

    bail!("Missing Test Case header (expected '## Test Case: NAME | DATE TIME..TIME')")
}

/// Parse a timespan string.
///
/// Format: `DATE TIME..TIME` (e.g., "2025-06-10 08:04:53..08:16:12")
fn parse_timespan(s: &str) -> Result<Timespan> {
    // Split by ".."
    let parts: Vec<&str> = s.splitn(2, "..").collect();
    if parts.len() != 2 {
        bail!("Malformed timespan: missing '..' separator in '{}'", s);
    }

    let start_full = parts[0].trim();
    let end_time = parts[1].trim();

    // Split start_full into date and time
    let start_parts: Vec<&str> = start_full.splitn(2, ' ').collect();
    if start_parts.len() != 2 {
        bail!(
            "Malformed timespan: expected 'DATE TIME' format in '{}'",
            start_full
        );
    }

    let date = start_parts[0].to_string();
    let start = start_parts[1].to_string();
    let end = end_time.to_string();
    let full = format!("{} {}..{}", date, start, end);

    Ok(Timespan {
        date,
        start,
        end,
        full,
    })
}

/// Parse the Prompt section to extract the prompt template.
fn parse_prompt(content: &str) -> Result<String> {
    let lines: Vec<&str> = content.lines().collect();

    // Find Prompt section
    let prompt_idx = lines
        .iter()
        .position(|l| l.starts_with("### Prompt"))
        .ok_or_else(|| anyhow::anyhow!("Missing '### Prompt' section"))?;

    // Extract the code block after the Prompt header
    let prompt = extract_code_block(&lines, prompt_idx + 1)?;

    if prompt.is_empty() {
        bail!("Empty prompt in '### Prompt' section");
    }

    Ok(prompt)
}

/// Extract content from a code block starting at the given line index.
///
/// Looks for ``` markers and extracts content between them.
fn extract_code_block(lines: &[&str], start_idx: usize) -> Result<String> {
    // Find the opening ```
    let mut block_start = None;
    for (i, line) in lines.iter().enumerate().skip(start_idx) {
        let line = line.trim();
        if line.starts_with("```") {
            block_start = Some(i + 1);
            break;
        }
        // Stop if we hit another section marker
        if line.starts_with("**") || line.starts_with("###") || line.starts_with("##") {
            break;
        }
    }

    let block_start = match block_start {
        Some(idx) => idx,
        None => return Ok(String::new()),
    };

    // Find the closing ```
    let mut block_end = None;
    for (i, line) in lines.iter().enumerate().skip(block_start) {
        if line.trim().starts_with("```") {
            block_end = Some(i);
            break;
        }
    }

    let block_end = match block_end {
        Some(idx) => idx,
        None => bail!("Unclosed code block"),
    };

    // Extract and join lines
    let block_content: Vec<&str> = lines[block_start..block_end].to_vec();
    Ok(block_content.join("\n"))
}

/// Parse the Verification Checklist section.
fn parse_checklist(content: &str) -> Result<Vec<String>> {
    let lines: Vec<&str> = content.lines().collect();

    // Find Verification Checklist section
    let checklist_idx = lines
        .iter()
        .position(|l| l.starts_with("### Verification Checklist"))
        .ok_or_else(|| anyhow::anyhow!("Missing '### Verification Checklist' section"))?;

    let mut checklist = Vec::new();

    // Collect all checkbox items after the checklist header
    for line in lines.iter().skip(checklist_idx + 1) {
        let trimmed = line.trim();

        // Stop at next major section
        if trimmed.starts_with("## ") && !trimmed.starts_with("## Work:") {
            break;
        }

        // Look for checkbox items
        if trimmed.starts_with("- [ ] ") {
            let item = trimmed.strip_prefix("- [ ] ").unwrap().to_string();
            checklist.push(item);
        }
    }

    if checklist.is_empty() {
        bail!("Empty checklist in Verification Checklist section");
    }

    Ok(checklist)
}

/// Substitute variables in text with timespan values.
///
/// Variables:
/// - `{{TIMESPAN}}` → full timespan
/// - `{{DATE}}` → date only
/// - `{{START}}` → start time
/// - `{{END}}` → end time
pub fn substitute_variables(text: &str, timespan: &Timespan) -> String {
    text.replace("{{TIMESPAN}}", &timespan.full)
        .replace("{{DATE}}", &timespan.date)
        .replace("{{START}}", &timespan.start)
        .replace("{{END}}", &timespan.end)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_TEST_FILE: &str = r#"# Test: Work Deamalgamation

Verify that deep-segmentation correctly separates different projects.

## Test Case: Mixed Projects | 2025-06-10 08:04:53..08:16:12

### Prompt

```
Perform deep segmentation for timespan: {{TIMESPAN}}
Use date: {{DATE}} start: {{START}} end: {{END}}
```

### Verification Checklist

Count passing checkboxes to rate.

#### Section A

- [ ] First checkbox item
- [ ] Second checkbox item

#### Section B

- [ ] Third checkbox item

**Total: 3 checkboxes**
"#;

    #[test]
    fn test_parse_header() {
        let (name, timespan) = parse_header(SAMPLE_TEST_FILE).unwrap();

        assert_eq!(name, "Mixed Projects");
        assert_eq!(timespan.date, "2025-06-10");
        assert_eq!(timespan.start, "08:04:53");
        assert_eq!(timespan.end, "08:16:12");
        assert_eq!(timespan.full, "2025-06-10 08:04:53..08:16:12");
    }

    #[test]
    fn test_parse_header_missing() {
        let content = "# Just a title\n\nNo test case header here.";
        let result = parse_header(content);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Missing Test Case header"));
    }

    #[test]
    fn test_parse_timespan() {
        let ts = parse_timespan("2025-06-10 08:04:53..08:16:12").unwrap();

        assert_eq!(ts.date, "2025-06-10");
        assert_eq!(ts.start, "08:04:53");
        assert_eq!(ts.end, "08:16:12");
        assert_eq!(ts.full, "2025-06-10 08:04:53..08:16:12");
    }

    #[test]
    fn test_parse_timespan_malformed_no_dots() {
        let result = parse_timespan("2025-06-10 08:04:53 08:16:12");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains(".."));
    }

    #[test]
    fn test_parse_timespan_malformed_no_space() {
        let result = parse_timespan("2025-06-1008:04:53..08:16:12");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_prompt() {
        let prompt = parse_prompt(SAMPLE_TEST_FILE).unwrap();

        assert!(prompt.contains("{{TIMESPAN}}"));
        assert!(prompt.contains("{{DATE}}"));
        assert!(prompt.contains("{{START}}"));
        assert!(prompt.contains("{{END}}"));
    }

    #[test]
    fn test_parse_prompt_empty() {
        let content = r#"
### Prompt

```
```
"#;
        let result = parse_prompt(content);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Empty prompt"));
    }

    #[test]
    fn test_parse_prompt_missing() {
        let content = r#"
### Something Else

```
content
```
"#;
        let result = parse_prompt(content);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Missing"));
    }

    #[test]
    fn test_parse_checklist() {
        let checklist = parse_checklist(SAMPLE_TEST_FILE).unwrap();

        assert_eq!(checklist.len(), 3);
        assert_eq!(checklist[0], "First checkbox item");
        assert_eq!(checklist[1], "Second checkbox item");
        assert_eq!(checklist[2], "Third checkbox item");
    }

    #[test]
    fn test_parse_checklist_empty() {
        let content = r#"
### Verification Checklist

No checkboxes here, just text.
"#;
        let result = parse_checklist(content);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Empty checklist"));
    }

    #[test]
    fn test_parse_test_content_full() {
        let test_case = parse_test_content(SAMPLE_TEST_FILE).unwrap();

        assert_eq!(test_case.name, "Mixed Projects");
        assert_eq!(test_case.timespan.date, "2025-06-10");
        assert_eq!(test_case.timespan.start, "08:04:53");
        assert_eq!(test_case.timespan.end, "08:16:12");
        assert!(test_case.prompt.contains("{{TIMESPAN}}"));
        assert!(test_case.prompt.contains("{{DATE}}"));
        assert_eq!(test_case.checklist.len(), 3);
    }

    #[test]
    fn test_substitute_variables() {
        let timespan = Timespan {
            date: "2025-06-10".to_string(),
            start: "08:04:53".to_string(),
            end: "08:16:12".to_string(),
            full: "2025-06-10 08:04:53..08:16:12".to_string(),
        };

        let text = "Process {{TIMESPAN}} on {{DATE}} from {{START}} to {{END}}";
        let result = substitute_variables(text, &timespan);

        assert_eq!(
            result,
            "Process 2025-06-10 08:04:53..08:16:12 on 2025-06-10 from 08:04:53 to 08:16:12"
        );
    }

    #[test]
    fn test_substitute_variables_no_placeholders() {
        let timespan = Timespan {
            date: "2025-06-10".to_string(),
            start: "08:04:53".to_string(),
            end: "08:16:12".to_string(),
            full: "2025-06-10 08:04:53..08:16:12".to_string(),
        };

        let text = "No placeholders here";
        let result = substitute_variables(text, &timespan);

        assert_eq!(result, "No placeholders here");
    }

    #[test]
    fn test_substitute_variables_multiple_occurrences() {
        let timespan = Timespan {
            date: "2025-01-01".to_string(),
            start: "00:00:00".to_string(),
            end: "23:59:59".to_string(),
            full: "2025-01-01 00:00:00..23:59:59".to_string(),
        };

        let text = "Date: {{DATE}}, again: {{DATE}}";
        let result = substitute_variables(text, &timespan);

        assert_eq!(result, "Date: 2025-01-01, again: 2025-01-01");
    }
}

use anyhow::Result;
use std::io::{self, Write};

use crate::activity::format_timestamp_display;
use crate::transcript::{EntryType, Role, TranscriptEntry};
use crate::OutputFormat;

pub fn run(session: &str, last: Option<usize>, format: OutputFormat, verbose: u8) -> Result<()> {
    // Auto-detect provider
    let mut entries = if session.starts_with("ses_") {
        crate::opencode::transcript::parse_transcript(session)?
    } else {
        crate::claudecode::transcript::parse_transcript(session)?
    };

    // Filter thinking entries unless verbose
    if verbose == 0 {
        entries.retain(|e| !matches!(e.entry_type, EntryType::Thinking));
    }

    // Apply --last N
    if let Some(n) = last {
        if entries.len() > n {
            entries = entries.split_off(entries.len() - n);
        }
    }

    let stdout = io::stdout();
    let mut handle = stdout.lock();

    match format {
        OutputFormat::Human => {
            for entry in &entries {
                let ts = format_timestamp_display(entry.timestamp.timestamp());
                let label = format_human_label(entry);
                let content = format_human_content(entry);
                writeln!(handle, "{} {} {}", ts, label, content)?;
            }
        }
        OutputFormat::Json => {
            for entry in &entries {
                writeln!(handle, "{}", serde_json::to_string(entry)?)?;
            }
        }
        OutputFormat::Nul => {
            for entry in &entries {
                write!(
                    handle,
                    "{}\0{}\0{}\0{}\0",
                    entry.timestamp.to_rfc3339(),
                    entry.role.as_str(),
                    entry.entry_type.as_str(),
                    entry.content,
                )?;
            }
        }
    }

    Ok(())
}

/// Format the label for human output, e.g. `[user]`, `[assistant/tool_use]`.
fn format_human_label(entry: &TranscriptEntry) -> String {
    match (&entry.role, &entry.entry_type) {
        (Role::User, EntryType::Text) => "[user]".to_string(),
        (Role::Assistant, EntryType::Text) => "[assistant]".to_string(),
        (Role::Assistant, EntryType::ToolUse) => "[assistant/tool_use]".to_string(),
        (Role::Assistant, EntryType::Thinking) => "[assistant/thinking]".to_string(),
        (_, EntryType::ToolResult) => "[tool_result]".to_string(),
        (role, entry_type) => format!("[{}/{}]", role.as_str(), entry_type.as_str()),
    }
}

/// Format content for human single-line output.
///
/// For tool_use: show tool name and JSON input.
/// For text/thinking/tool_result: replace newlines, truncate to 200 chars.
fn format_human_content(entry: &TranscriptEntry) -> String {
    match entry.entry_type {
        EntryType::ToolUse => {
            let name = entry.tool_name.as_deref().unwrap_or("unknown");
            let input_str = entry
                .tool_input
                .as_ref()
                .map(|v| serde_json::to_string(v).unwrap_or_default())
                .unwrap_or_default();
            let raw = format!("{} {}", name, input_str);
            truncate_line(&raw, 200)
        }
        _ => truncate_line(&entry.content, 200),
    }
}

/// Replace newlines with literal `\n` and truncate to max_chars with `...`.
fn truncate_line(s: &str, max_chars: usize) -> String {
    let single_line = s.replace('\n', "\\n");
    if single_line.chars().count() > max_chars {
        let truncated: String = single_line.chars().take(max_chars).collect();
        format!("{}...", truncated)
    } else {
        single_line
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_truncate_line_short() {
        assert_eq!(truncate_line("hello", 200), "hello");
    }

    #[test]
    fn test_truncate_line_long() {
        let long = "a".repeat(250);
        let result = truncate_line(&long, 200);
        assert_eq!(result.len(), 203); // 200 + "..."
        assert!(result.ends_with("..."));
    }

    #[test]
    fn test_truncate_line_newlines() {
        assert_eq!(
            truncate_line("line1\nline2\nline3", 200),
            "line1\\nline2\\nline3"
        );
    }

    #[test]
    fn test_format_human_label_user() {
        let entry = TranscriptEntry {
            timestamp: chrono::Utc::now(),
            role: Role::User,
            entry_type: EntryType::Text,
            content: String::new(),
            tool_name: None,
            tool_input: None,
        };
        assert_eq!(format_human_label(&entry), "[user]");
    }

    #[test]
    fn test_format_human_label_tool_use() {
        let entry = TranscriptEntry {
            timestamp: chrono::Utc::now(),
            role: Role::Assistant,
            entry_type: EntryType::ToolUse,
            content: String::new(),
            tool_name: Some("Bash".to_string()),
            tool_input: None,
        };
        assert_eq!(format_human_label(&entry), "[assistant/tool_use]");
    }

    #[test]
    fn test_format_human_label_tool_result() {
        let entry = TranscriptEntry {
            timestamp: chrono::Utc::now(),
            role: Role::User,
            entry_type: EntryType::ToolResult,
            content: String::new(),
            tool_name: None,
            tool_input: None,
        };
        assert_eq!(format_human_label(&entry), "[tool_result]");
    }
}

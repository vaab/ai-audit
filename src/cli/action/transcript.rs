use anyhow::Result;
use colored::{ColoredString, Colorize};
use std::io::{self, Write};
use std::path::PathBuf;

use crate::activity::format_timestamp_display;
use crate::transcript::{EntryType, Role, TranscriptEntry};
use crate::OutputFormat;

/// Tool names (case-insensitive) that write or edit files.
const WRITE_TOOL_NAMES: &[&str] = &[
    "write",
    "edit",
    "multiedit",
    "createfile",
    "multi_edit",
    "create",
];

pub fn run(
    session: &str,
    last: Option<usize>,
    file: Option<&str>,
    format: OutputFormat,
    verbose: u8,
) -> Result<()> {
    // Auto-detect provider
    let provider = crate::provider::provider_for_session(session)?;
    let mut entries = provider.parse_transcript(session)?;

    // Filter thinking entries unless verbose
    if verbose == 0 {
        entries.retain(|e| !matches!(e.entry_type, EntryType::Thinking));
    }

    // Filter to only tool_use entries targeting the given file
    if let Some(f) = file {
        let path = PathBuf::from(f);
        let abs = if path.is_absolute() {
            path
        } else {
            std::env::current_dir().unwrap_or_default().join(path)
        };
        let resolved = abs.canonicalize().unwrap_or(abs);
        let target = resolved.to_string_lossy().to_string();
        entries.retain(|e| entry_targets_file(e, &target));
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
                let label = format_human_label_colored(entry);
                let content = format_human_content(entry);
                // The `colored` crate gates ANSI emission via a global override
                // set by `cli::color::init()`. When colors are off, `ts.cyan()`
                // and the `ColoredString` from `format_human_label_colored`
                // both render as plain text.
                writeln!(handle, "{} {} {}", ts.cyan(), label, content)?;
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
        (_, EntryType::ToolError) => "[tool_result/error]".to_string(),
        (_, EntryType::Error) => "[error]".to_string(),
        (role, entry_type) => format!("[{}/{}]", role.as_str(), entry_type.as_str()),
    }
}

/// Color-aware label for human output.
///
/// Returns a `ColoredString`; when the `colored` global override is off
/// (set by `cli::color::init()`), the styling is a no-op and the output
/// renders as plain ASCII identical to [`format_human_label`].
///
/// Palette (ANSI-16, consistent with `list_sessions.rs`):
///
/// | Label                  | Style                  |
/// |------------------------|------------------------|
/// | `[user]`               | green, bold            |
/// | `[assistant]`          | blue, bold             |
/// | `[assistant/tool_use]` | magenta, bold          |
/// | `[assistant/thinking]` | bright_black (dim)     |
/// | `[tool_result]`        | yellow                 |
/// | `[tool_result/error]`  | red, bold              |
/// | `[error]`              | red, bold              |
/// | catch-all              | white (neutral)        |
fn format_human_label_colored(entry: &TranscriptEntry) -> ColoredString {
    let plain = format_human_label(entry);
    match (&entry.role, &entry.entry_type) {
        (Role::User, EntryType::Text) => plain.green().bold(),
        (Role::Assistant, EntryType::Text) => plain.blue().bold(),
        (Role::Assistant, EntryType::ToolUse) => plain.magenta().bold(),
        (Role::Assistant, EntryType::Thinking) => plain.bright_black(),
        (_, EntryType::ToolResult) => plain.yellow(),
        (_, EntryType::ToolError) => plain.red().bold(),
        (_, EntryType::Error) => plain.red().bold(),
        (_, _) => plain.white(),
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

/// Check if a transcript entry is a tool_use that writes/edits the given file.
///
/// Handles both Claude Code (`file_path` key) and OpenCode (`filePath` / `file_path` keys)
/// tool input formats. Tool names are matched case-insensitively to handle both providers.
fn entry_targets_file(entry: &TranscriptEntry, target_path: &str) -> bool {
    if !matches!(entry.entry_type, EntryType::ToolUse) {
        return false;
    }
    let tool_name = match &entry.tool_name {
        Some(n) => n,
        None => return false,
    };
    if !WRITE_TOOL_NAMES.contains(&tool_name.to_ascii_lowercase().as_str()) {
        return false;
    }
    let input = match &entry.tool_input {
        Some(v) => v,
        None => return false,
    };
    let tool_path = input
        .get("file_path")
        .or_else(|| input.get("filePath"))
        .and_then(|p| p.as_str());
    match tool_path {
        Some(p) => crate::file_path_matches(p, target_path),
        None => false,
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
    fn test_entry_targets_file_write_match() {
        let entry = TranscriptEntry {
            timestamp: chrono::Utc::now(),
            role: Role::Assistant,
            entry_type: EntryType::ToolUse,
            content: String::new(),
            tool_name: Some("Write".to_string()),
            tool_input: Some(
                serde_json::json!({"file_path": "/home/user/src/main.rs", "content": "fn main() {}"}),
            ),
        };
        assert!(entry_targets_file(&entry, "/home/user/src/main.rs"));
        assert!(!entry_targets_file(&entry, "/home/user/src/lib.rs"));
    }

    #[test]
    fn test_entry_targets_file_opencode_camel_case() {
        let entry = TranscriptEntry {
            timestamp: chrono::Utc::now(),
            role: Role::Assistant,
            entry_type: EntryType::ToolUse,
            content: String::new(),
            tool_name: Some("edit".to_string()),
            tool_input: Some(serde_json::json!({"filePath": "/home/user/src/main.rs"})),
        };
        assert!(entry_targets_file(&entry, "/home/user/src/main.rs"));
    }

    #[test]
    fn test_entry_targets_file_relative_path_in_tool() {
        let entry = TranscriptEntry {
            timestamp: chrono::Utc::now(),
            role: Role::Assistant,
            entry_type: EntryType::ToolUse,
            content: String::new(),
            tool_name: Some("Edit".to_string()),
            tool_input: Some(serde_json::json!({"file_path": "src/main.rs"})),
        };
        assert!(entry_targets_file(&entry, "/home/user/project/src/main.rs"));
        assert!(!entry_targets_file(&entry, "/home/user/project/src/lib.rs"));
    }

    #[test]
    fn test_entry_targets_file_ignores_non_write_tools() {
        let entry = TranscriptEntry {
            timestamp: chrono::Utc::now(),
            role: Role::Assistant,
            entry_type: EntryType::ToolUse,
            content: String::new(),
            tool_name: Some("Read".to_string()),
            tool_input: Some(serde_json::json!({"file_path": "/home/user/src/main.rs"})),
        };
        assert!(!entry_targets_file(&entry, "/home/user/src/main.rs"));
    }

    #[test]
    fn test_entry_targets_file_ignores_text_entries() {
        let entry = TranscriptEntry {
            timestamp: chrono::Utc::now(),
            role: Role::Assistant,
            entry_type: EntryType::Text,
            content: "editing src/main.rs".to_string(),
            tool_name: None,
            tool_input: None,
        };
        assert!(!entry_targets_file(&entry, "/home/user/src/main.rs"));
    }

    #[test]
    fn test_entry_targets_file_ignores_tool_result() {
        let entry = TranscriptEntry {
            timestamp: chrono::Utc::now(),
            role: Role::User,
            entry_type: EntryType::ToolResult,
            content: "file written".to_string(),
            tool_name: None,
            tool_input: None,
        };
        assert!(!entry_targets_file(&entry, "/home/user/src/main.rs"));
    }

    #[test]
    fn test_entry_targets_file_all_write_tools() {
        for tool in &[
            "Write",
            "Edit",
            "MultiEdit",
            "CreateFile",
            "write",
            "edit",
            "multi_edit",
            "create",
        ] {
            let entry = TranscriptEntry {
                timestamp: chrono::Utc::now(),
                role: Role::Assistant,
                entry_type: EntryType::ToolUse,
                content: String::new(),
                tool_name: Some(tool.to_string()),
                tool_input: Some(serde_json::json!({"file_path": "/home/user/src/main.rs"})),
            };
            assert!(
                entry_targets_file(&entry, "/home/user/src/main.rs"),
                "tool '{}' should be recognized as a write tool",
                tool
            );
        }
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

    #[test]
    fn test_format_human_label_tool_error() {
        let entry = TranscriptEntry {
            timestamp: chrono::Utc::now(),
            role: Role::User,
            entry_type: EntryType::ToolError,
            content: "Error: The user dismissed this question".to_string(),
            tool_name: None,
            tool_input: None,
        };
        assert_eq!(format_human_label(&entry), "[tool_result/error]");
    }

    #[test]
    fn test_format_human_content_tool_error_uses_content_string() {
        // ToolError content is the error/refusal message, NOT a tool name+input.
        let entry = TranscriptEntry {
            timestamp: chrono::Utc::now(),
            role: Role::User,
            entry_type: EntryType::ToolError,
            content: "No Chrome extension connected.".to_string(),
            tool_name: None,
            tool_input: None,
        };
        assert_eq!(
            format_human_content(&entry),
            "No Chrome extension connected."
        );
    }

    // ---- Color tests ---------------------------------------------------
    //
    // These tests inspect the `ColoredString` styling attributes
    // directly (foreground color + bold-style bit) rather than rendering
    // through `Display`.  Rendering depends on the GLOBAL `colored`
    // override, which other tests in this process may toggle in
    // parallel — checking the attributes avoids that race entirely.

    fn entry_of(role: Role, entry_type: EntryType) -> TranscriptEntry {
        TranscriptEntry {
            timestamp: chrono::Utc::now(),
            role,
            entry_type,
            content: String::new(),
            tool_name: None,
            tool_input: None,
        }
    }

    #[test]
    fn test_format_human_label_colored_preserves_plain_text() {
        // The colored label MUST match the plain label byte-for-byte
        // when ANSI escapes are stripped — i.e. the user-visible text
        // is the same; only styling is added.
        for (role, entry_type) in [
            (Role::User, EntryType::Text),
            (Role::Assistant, EntryType::Text),
            (Role::Assistant, EntryType::ToolUse),
            (Role::Assistant, EntryType::Thinking),
            (Role::User, EntryType::ToolResult),
            (Role::User, EntryType::ToolError),
            (Role::User, EntryType::Error),
        ] {
            let e = entry_of(role, entry_type);
            let plain = format_human_label(&e);
            let cs = format_human_label_colored(&e);
            let colored_input: &str = cs.as_ref();
            assert_eq!(
                colored_input, plain,
                "colored input text must equal plain label for {:?}/{:?}",
                e.role, e.entry_type,
            );
        }
    }

    #[test]
    fn test_format_human_label_colored_user_is_green_bold() {
        let cs = format_human_label_colored(&entry_of(Role::User, EntryType::Text));
        assert_eq!(cs.fgcolor, Some(colored::Color::Green));
        assert!(cs.style.contains(colored::Styles::Bold));
    }

    #[test]
    fn test_format_human_label_colored_assistant_is_blue_bold() {
        let cs = format_human_label_colored(&entry_of(Role::Assistant, EntryType::Text));
        assert_eq!(cs.fgcolor, Some(colored::Color::Blue));
        assert!(cs.style.contains(colored::Styles::Bold));
    }

    #[test]
    fn test_format_human_label_colored_tool_use_is_magenta_bold() {
        let cs = format_human_label_colored(&entry_of(Role::Assistant, EntryType::ToolUse));
        assert_eq!(cs.fgcolor, Some(colored::Color::Magenta));
        assert!(cs.style.contains(colored::Styles::Bold));
    }

    #[test]
    fn test_format_human_label_colored_thinking_is_dim() {
        let cs = format_human_label_colored(&entry_of(Role::Assistant, EntryType::Thinking));
        assert_eq!(cs.fgcolor, Some(colored::Color::BrightBlack));
    }

    #[test]
    fn test_format_human_label_colored_tool_result_is_yellow() {
        let cs = format_human_label_colored(&entry_of(Role::User, EntryType::ToolResult));
        assert_eq!(cs.fgcolor, Some(colored::Color::Yellow));
    }

    #[test]
    fn test_format_human_label_colored_tool_error_is_red_bold() {
        let cs = format_human_label_colored(&entry_of(Role::User, EntryType::ToolError));
        assert_eq!(cs.fgcolor, Some(colored::Color::Red));
        assert!(cs.style.contains(colored::Styles::Bold));
    }

    #[test]
    fn test_format_human_label_colored_error_is_red_bold() {
        let cs = format_human_label_colored(&entry_of(Role::User, EntryType::Error));
        assert_eq!(cs.fgcolor, Some(colored::Color::Red));
        assert!(cs.style.contains(colored::Styles::Bold));
    }
}

//! Auto-detect the current AI session.
//!
//! Detection strategy (in priority order):
//! 1. Check env vars for authoritative session ID
//!    (`OPENCODE_SESSION_ID` / `CLAUDE_SESSION_ID`)
//! 2. If running inside tmux, capture pane scrollback and fingerprint
//!    the TUI output: classify lines by ANSI color codes, extract
//!    assistant messages and tool invocations, build structured filters,
//!    then search all sessions for a match.
//!
//! The `--match` flag (`find_session_by_match`) provides a separate
//! code path that searches session message text directly.

use anyhow::{bail, Context, Result};
use regex::Regex;
use std::env;

pub use crate::provider::Provider;

/// Minimum length for an extracted segment to be useful as a search needle.
const MIN_NEEDLE_LENGTH: usize = 20;

/// Number of scrollback lines to capture for last-session detection.
const LAST_SESSION_SCROLLBACK_LINES: &str = "3000";

// ── Filter system types ─────────────────────────────────────────

/// Classification of a TUI line based on ANSI color codes.
#[derive(Debug, Clone, PartialEq)]
enum TuiLineKind {
    AssistantText(String),
    ToolInvocation(String),
    PanelContent,
    CompletionMarker,
    SessionTitle,
    Footer,
    Empty,
    Other,
}

#[derive(Debug, Clone, PartialEq)]
struct ParsedToolCall {
    tool_name: String,
    fields: Vec<(String, String)>,
}

#[derive(Debug, Clone)]
struct PaneMessage {
    text_lines: Vec<String>,
    tool_calls: Vec<ParsedToolCall>,
    depth: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum FilterCriterion {
    TextContains(String),
    ToolFieldEquals {
        tool_name: String,
        field: String,
        value: String,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct SessionFilter {
    pub depth: usize,
    pub criteria: Vec<FilterCriterion>,
}

// ── Core public types ───────────────────────────────────────────

/// Result of session auto-detection.
#[derive(Debug, Clone)]
pub struct DetectedSession {
    pub session_id: String,
    pub provider: Provider,
}

/// Options for last-session detection.
pub struct LastSessionOptions {
    /// Optional provider filter.
    pub provider_filter: Option<Provider>,
    /// Read scrollback from file instead of capturing from tmux pane.
    pub scrollback_file: Option<std::path::PathBuf>,
}

/// Options for match-based session detection.
pub struct MatchOptions {
    /// Text to search for in recent messages.
    pub needle: String,
    /// Number of recent messages to search.
    pub last_messages: usize,
    /// Optional provider filter.
    pub provider_filter: Option<Provider>,
    /// Optional project directory filter.
    pub project_dir: Option<String>,
}

// ── ANSI / TUI helpers ──────────────────────────────────────────

/// Strip ANSI escape codes (CSI and OSC sequences) from a string.
fn strip_ansi(s: &str) -> String {
    // CSI: \x1b[ ... letter   OSC: \x1b] ... \x1b\\ or \x1b] ... \x07
    let re = Regex::new(r"\x1b\[[0-9;]*[a-zA-Z]|\x1b\][^\x07\x1b]*(?:\x1b\\|\x07)")
        .expect("valid regex");
    re.replace_all(s, "").to_string()
}

/// Split a string by ANSI escape codes (CSI and OSC), returning the text segments between them.
fn ansi_segments(s: &str) -> Vec<&str> {
    let re = Regex::new(r"\x1b\[[0-9;]*[a-zA-Z]|\x1b\][^\x07\x1b]*(?:\x1b\\|\x07)")
        .expect("valid regex");
    re.split(s).filter(|seg| !seg.is_empty()).collect()
}

/// Extract the foreground color code active at the first visible character.
///
/// Walks through ANSI escapes until the first non-whitespace text,
/// returning the last `38;...` foreground color seen before it.
fn leading_fg_color(raw_line: &str) -> Option<String> {
    let re = Regex::new(r"\x1b\[([0-9;]*)[a-zA-Z]").expect("valid regex");
    let mut last_fg: Option<String> = None;
    let mut pos = 0;

    for m in re.find_iter(raw_line) {
        // Check for visible text between previous position and this escape
        let between = &raw_line[pos..m.start()];
        if between.chars().any(|c| !c.is_whitespace()) {
            // Hit visible text — return whatever fg we had
            return last_fg;
        }
        // Extract the params from the escape
        let params_match = Regex::new(r"\x1b\[([0-9;]*)[a-zA-Z]").expect("valid regex");
        if let Some(caps) = params_match.captures(m.as_str()) {
            let params = &caps[1];
            if params.starts_with("38;") {
                last_fg = Some(params.to_string());
            }
        }
        pos = m.end();
    }
    // Check remaining text after last escape
    let remaining = &raw_line[pos..];
    if remaining.chars().any(|c| !c.is_whitespace()) {
        return last_fg;
    }
    last_fg
}

/// Check if a line starts with a visible character rendered in the given fg color.
fn line_starts_with_fg_color(raw_line: &str, fg_color: &str) -> bool {
    leading_fg_color(raw_line).as_deref() == Some(fg_color)
}

/// Find the longest text segment between ANSI escape codes in a string.
fn longest_ansi_segment(s: &str) -> Option<String> {
    ansi_segments(s)
        .into_iter()
        .map(|seg| seg.trim())
        .filter(|seg| !seg.is_empty())
        .max_by_key(|seg| seg.len())
        .map(|s| s.to_string())
}

/// Classify a raw TUI line based on ANSI color codes and content.
fn classify_tui_line(raw_line: &str) -> TuiLineKind {
    let stripped = strip_ansi(raw_line);
    let trimmed = stripped.trim();

    // Empty
    if trimmed.is_empty() {
        return TuiLineKind::Empty;
    }

    // Footer: ╹ followed by 10+ ▀ chars
    if trimmed.starts_with('╹') {
        let after_marker = &trimmed['╹'.len_utf8()..];
        if after_marker.chars().take_while(|&c| c == '▀').count() >= 10 {
            return TuiLineKind::Footer;
        }
    }

    // CompletionMarker: starts with ▣
    if trimmed.starts_with('▣') {
        return TuiLineKind::CompletionMarker;
    }

    // SessionTitle: bold # heading with cost/token stats.
    // Raw line has \x1b[1m (bold) before the #, and stripped line
    // contains "# " after the panel border and ends with cost like "($...)"
    if raw_line.contains("\x1b[1m") {
        // Strip panel border (┃) and check for "# " heading
        let after_border = if let Some(pos) = trimmed.find('┃') {
            trimmed[pos + '┃'.len_utf8()..].trim_start()
        } else {
            trimmed
        };
        if after_border.starts_with("# ") && trimmed.contains("($") {
            return TuiLineKind::SessionTitle;
        }
    }

    // PanelContent: raw has background color AND stripped has ┃,
    // but NOT if the line also has assistant text foreground — those
    // lines carry session content rendered inside the panel and should
    // be collected as AssistantText instead.
    if raw_line.contains("48;2;")
        && stripped.contains('┃')
        && !raw_line.contains("38;2;242;244;248")
    {
        return TuiLineKind::PanelContent;
    }

    // ToolInvocation: dim gray color AND starts with tool icon
    if raw_line.contains("38;2;125;132;143") {
        if trimmed.starts_with('→')
            || trimmed.starts_with('✱')
            || trimmed.starts_with('←')
            || trimmed.starts_with('⚙')
        {
            return TuiLineKind::ToolInvocation(trimmed.to_string());
        }
    }

    // AssistantText: light gray foreground (with or without background).
    // Lines inside the ┃ panel that carry assistant text color contain
    // session content (system reminders, titles, responses) and should
    // be collected for fingerprinting.
    // Store the raw line so we can extract ANSI segments later.
    if raw_line.contains("38;2;242;244;248") {
        return TuiLineKind::AssistantText(raw_line.to_string());
    }

    TuiLineKind::Other
}

/// Parse a tool invocation line into a structured tool call.
///
/// Handles icons: → (read), ✱ (grep), ← (edit), ⚙ (task/other)
fn parse_tool_line(stripped_text: &str) -> Option<ParsedToolCall> {
    let trimmed = stripped_text.trim();

    // Determine icon and skip it
    let after_icon = if trimmed.starts_with('→') {
        &trimmed['→'.len_utf8()..]
    } else if trimmed.starts_with('✱') {
        &trimmed['✱'.len_utf8()..]
    } else if trimmed.starts_with('←') {
        &trimmed['←'.len_utf8()..]
    } else if trimmed.starts_with('⚙') {
        &trimmed['⚙'.len_utf8()..]
    } else {
        return None;
    };

    let after_icon = after_icon.trim_start();

    // Extract tool name (first word, lowercased)
    let tool_name = after_icon.split_whitespace().next()?.to_lowercase();

    let rest = after_icon[tool_name.len()..].trim_start();

    let mut fields = Vec::new();

    if tool_name == "grep" {
        // Grep format: Grep "pattern" in path
        if let Some(start) = rest.find('"') {
            let after_open = &rest[start + 1..];
            if let Some(end) = after_open.find('"') {
                let pattern = &after_open[..end];
                fields.push(("pattern".to_string(), pattern.to_string()));
                let after_pattern = &after_open[end + 1..].trim_start();
                if let Some(path) = after_pattern.strip_prefix("in ") {
                    fields.push(("path".to_string(), path.trim().to_string()));
                }
            }
        }
    } else {
        // Read/Edit/Task format: ToolName path [key=value, ...]
        // or: ToolName [key=value, ...]
        if let Some(bracket_start) = rest.find('[') {
            let path = rest[..bracket_start].trim();
            if !path.is_empty() {
                fields.push(("filePath".to_string(), path.to_string()));
            }
            let bracket_end = rest.rfind(']').unwrap_or(rest.len());
            let kv_str = &rest[bracket_start + 1..bracket_end];
            for pair in kv_str.split(',') {
                let pair = pair.trim();
                if let Some(eq_pos) = pair.find('=') {
                    let key = pair[..eq_pos].trim().to_string();
                    let value = pair[eq_pos + 1..].trim().to_string();
                    fields.push((key, value));
                }
            }
        } else {
            // No brackets — rest is just the path
            let path = rest.trim();
            if !path.is_empty() {
                fields.push(("filePath".to_string(), path.to_string()));
            }
        }
    }

    // Normalize tool names: the TUI renders some tools differently
    // from how they appear in the database.
    let (tool_name, fields) = if tool_name == "skill_mcp" {
        // TUI: ⚙ skill_mcp [mcp_name=playwright, tool_name=browser_navigate]
        // DB:  tool="skill", input={name: "playwright"}
        let mapped_fields: Vec<(String, String)> = fields
            .into_iter()
            .filter_map(|(k, v)| {
                if k == "mcp_name" {
                    Some(("name".to_string(), v))
                } else {
                    // Drop tool_name and other sub-agent fields not in DB
                    None
                }
            })
            .collect();
        ("skill".to_string(), mapped_fields)
    } else {
        (tool_name, fields)
    };

    Some(ParsedToolCall { tool_name, fields })
}

/// Parse pane scrollback content into structured messages.
///
/// Walks the TUI output bottom-up from the last footer, collecting
/// assistant text and tool invocations into messages separated by
/// completion markers or panel content.
fn parse_pane_messages(content: &str) -> Vec<PaneMessage> {
    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() {
        return Vec::new();
    }

    // Classify all lines
    let classified: Vec<TuiLineKind> = lines.iter().map(|l| classify_tui_line(l)).collect();

    // Find last Footer
    let footer_idx = match classified
        .iter()
        .rposition(|k| matches!(k, TuiLineKind::Footer))
    {
        Some(idx) => idx,
        None => return Vec::new(),
    };

    log::debug!(
        "parse_pane_messages: footer at line {}/{}",
        footer_idx,
        lines.len()
    );

    // Detect the footer's leading foreground color (the ╹ character color).
    // Lines above the footer that start with the same fg color are the
    // input/compose prompt area — skip them.
    let footer_fg = leading_fg_color(lines[footer_idx]);
    let mut content_start = footer_idx;
    if let Some(ref fg) = footer_fg {
        // Skip lines with same fg color as footer (prompt/input area)
        for i in (0..footer_idx).rev() {
            if line_starts_with_fg_color(lines[i], fg) {
                content_start = i;
            } else {
                break;
            }
        }
        // Also skip a blank line + ▣ CompletionMarker above the prompt area
        let mut skip_to = content_start;
        if skip_to > 0 && matches!(classified[skip_to - 1], TuiLineKind::Empty) {
            skip_to -= 1;
            if skip_to > 0 && matches!(classified[skip_to - 1], TuiLineKind::CompletionMarker) {
                skip_to -= 1;
            }
        }
        if content_start < footer_idx || skip_to < content_start {
            content_start = skip_to;
            log::debug!(
                "parse_pane_messages: skipping prompt area lines {}..{} (fg color {})",
                content_start,
                footer_idx,
                fg
            );
        }
    }

    // Walk upward from content boundary, collecting messages
    let mut messages: Vec<PaneMessage> = Vec::new();
    let mut current_text: Vec<String> = Vec::new();
    let mut current_tools: Vec<ParsedToolCall> = Vec::new();
    let mut completion_count = 0;

    for i in (0..content_start).rev() {
        if messages.len() >= 10 || completion_count >= 3 {
            break;
        }

        match &classified[i] {
            TuiLineKind::CompletionMarker | TuiLineKind::PanelContent => {
                // Message boundary — flush current message if non-empty
                if !current_text.is_empty() || !current_tools.is_empty() {
                    current_text.reverse();
                    current_tools.reverse();
                    messages.push(PaneMessage {
                        text_lines: current_text,
                        tool_calls: current_tools,
                        depth: 0, // assigned later
                    });
                    current_text = Vec::new();
                    current_tools = Vec::new();
                }
                if matches!(&classified[i], TuiLineKind::CompletionMarker) {
                    completion_count += 1;
                }
            }
            TuiLineKind::AssistantText(text) => {
                current_text.push(text.clone());
            }
            TuiLineKind::ToolInvocation(text) => {
                if let Some(parsed) = parse_tool_line(text) {
                    current_tools.push(parsed);
                }
            }
            TuiLineKind::Footer | TuiLineKind::SessionTitle => {
                // Another footer or session title above — stop
                break;
            }
            _ => {}
        }
    }

    // Flush any remaining content
    if !current_text.is_empty() || !current_tools.is_empty() {
        current_text.reverse();
        current_tools.reverse();
        messages.push(PaneMessage {
            text_lines: current_text,
            tool_calls: current_tools,
            depth: 0,
        });
    }

    // Assign depths: 0 = closest to footer, 1 = next, etc.
    for (i, msg) in messages.iter_mut().enumerate() {
        msg.depth = i;
    }

    log::debug!("parse_pane_messages: found {} messages", messages.len());
    for (i, msg) in messages.iter().enumerate() {
        log::trace!(
            "  message[{}]: depth={}, text_lines={}, tool_calls={}",
            i,
            msg.depth,
            msg.text_lines.len(),
            msg.tool_calls.len()
        );
    }

    messages
}

/// Build structured filters from parsed pane messages.
///
/// Selects up to 3 messages, preferring those with both text and tool calls.
/// For each selected message:
/// - Longest text line >= MIN_NEEDLE_LENGTH → TextContains
/// - Tool call fields → ToolFieldEquals
fn build_filters(messages: &[PaneMessage]) -> Vec<SessionFilter> {
    if messages.is_empty() {
        return Vec::new();
    }

    // Score messages by actual content quality.
    // Primary: max ANSI segment length (longer = more distinctive needle).
    // Secondary: number of tool call fields (more fields = stronger filter).
    // This ensures messages with long, meaningful text rank above those with
    // only short syntax-highlighted fragments (e.g. CSS diffs).
    let mut scored: Vec<(usize, usize)> = messages
        .iter()
        .enumerate()
        .map(|(idx, msg)| {
            let max_seg_len = msg
                .text_lines
                .iter()
                .filter_map(|raw| longest_ansi_segment(raw))
                .map(|seg| seg.len())
                .max()
                .unwrap_or(0);
            let tool_field_count: usize = msg.tool_calls.iter().map(|tc| tc.fields.len()).sum();
            // Combine: segment length dominates, tool fields add minor boost
            let score = max_seg_len + tool_field_count;
            (idx, score)
        })
        .collect();

    // Sort by score descending, then by index ascending (prefer closer to footer)
    scored.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));

    let mut filters = Vec::new();

    for &(idx, _score) in scored.iter().take(3) {
        let msg = &messages[idx];
        let mut criteria = Vec::new();

        // Longest ANSI segment across all text lines as TextContains.
        // Text lines contain raw ANSI — split by escape codes to get
        // the actual text segments.  The biggest segment is the most
        // distinctive needle and will match as a substring of the DB
        // text even when the TUI rendered away markdown formatting.
        if let Some(longest) = msg
            .text_lines
            .iter()
            .filter_map(|raw| longest_ansi_segment(raw))
            .filter(|seg| seg.len() >= MIN_NEEDLE_LENGTH)
            .max_by_key(|seg| seg.len())
        {
            // Strip TUI "$ " command prefix — the DB stores
            // commands without this shell-prompt chrome.
            let needle = if longest.starts_with("$ ") {
                longest[2..].to_string()
            } else {
                longest
            };
            if needle.len() >= MIN_NEEDLE_LENGTH {
                criteria.push(FilterCriterion::TextContains(needle));
            }
        }

        // Tool call fields as ToolFieldEquals
        for tc in &msg.tool_calls {
            for (field, value) in &tc.fields {
                criteria.push(FilterCriterion::ToolFieldEquals {
                    tool_name: tc.tool_name.clone(),
                    field: field.clone(),
                    value: value.clone(),
                });
            }
        }

        if !criteria.is_empty() {
            filters.push(SessionFilter {
                depth: msg.depth,
                criteria,
            });
        }
    }

    log::debug!("build_filters: built {} filters", filters.len());
    for (i, f) in filters.iter().enumerate() {
        log::trace!(
            "  filter[{}]: depth={}, criteria={}",
            i,
            f.depth,
            f.criteria.len()
        );
        for (j, c) in f.criteria.iter().enumerate() {
            match c {
                FilterCriterion::TextContains(needle) => {
                    log::trace!("    criterion[{}]: TextContains({:?})", j, needle);
                }
                FilterCriterion::ToolFieldEquals {
                    tool_name,
                    field,
                    value,
                } => {
                    log::trace!(
                        "    criterion[{}]: ToolFieldEquals(tool={}, {}={:?})",
                        j,
                        tool_name,
                        field,
                        value
                    );
                }
            }
        }
    }

    filters
}

/// Search all sessions for one matching the structured filters.
///
/// Lists sessions from both providers (respecting `provider_filter`),
/// sorts by `updated_at` descending, skips sub-agents. For each session,
/// checks ALL filters with incremental search margins.
fn find_session_by_filters(
    filters: &[SessionFilter],
    provider_filter: Option<Provider>,
) -> Option<DetectedSession> {
    if filters.is_empty() {
        return None;
    }

    // Collect sessions from both providers
    // Tuple: (session_id, provider, updated_at, project_dir)
    let mut all_sessions: Vec<(String, Provider, chrono::DateTime<chrono::Utc>, String)> =
        Vec::new();

    let include_opencode = provider_filter.is_none() || provider_filter == Some(Provider::OpenCode);
    let include_claudecode =
        provider_filter.is_none() || provider_filter == Some(Provider::ClaudeCode);
    let include_pi = provider_filter.is_none() || provider_filter == Some(Provider::Pi);

    if include_opencode {
        if let Ok(sessions) = crate::opencode::list_sessions() {
            for s in sessions {
                // Skip sub-agent sessions
                if s.parent_id.is_some() {
                    continue;
                }
                all_sessions.push((
                    s.session_id,
                    Provider::OpenCode,
                    s.updated_at,
                    s.project_dir,
                ));
            }
        }
    }

    if include_claudecode {
        if let Ok(sessions) = crate::claudecode::session::list_sessions() {
            for s in sessions {
                all_sessions.push((
                    s.session_id,
                    Provider::ClaudeCode,
                    s.updated_at,
                    String::new(),
                ));
            }
        }
    }

    if include_pi {
        if let Ok(sessions) = crate::pi::session::list_sessions() {
            for s in sessions {
                // Skip sub-agent sessions (parent_id is set when the session
                // file lives nested under another session's directory).
                if s.parent_id.is_some() {
                    continue;
                }
                all_sessions.push((s.session_id, Provider::Pi, s.updated_at, s.project_dir));
            }
        }
    }

    // Sort by updated_at descending (most recent first)
    all_sessions.sort_by(|a, b| b.2.cmp(&a.2));

    log::debug!(
        "find_session_by_filters: searching {} sessions with {} filters",
        all_sessions.len(),
        filters.len()
    );

    // Incremental margins: try tighter windows first
    let margins = [5usize, 10, 20];

    // Graceful degradation: try all filters, then drop the deepest ones.
    // Scrollback may contain content from previous sessions (user switched
    // via Ctrl+P), so older/deeper messages may belong to a different session.
    // Require at least 2 filters to avoid false positives.
    let mut filter_counts: Vec<usize> = Vec::new();
    filter_counts.push(filters.len());
    if filters.len() > 2 {
        filter_counts.push(filters.len() - 1);
    }

    // Sort filters by depth ascending (shallowest = most recent first).
    // When degrading, we drop the deepest (oldest) filters.
    let mut sorted_filters: Vec<&SessionFilter> = filters.iter().collect();
    sorted_filters.sort_by_key(|f| f.depth);

    for &num_filters in &filter_counts {
        let active_filters = &sorted_filters[..num_filters];

        for margin in &margins {
            for (session_id, provider, _updated_at, project_dir) in &all_sessions {
                let all_match = active_filters.iter().all(|filter| {
                    let window = margin + filter.depth;
                    match provider {
                        Provider::OpenCode => crate::opencode::session_matches_filters(
                            session_id,
                            &[(*filter).clone()],
                            window,
                            project_dir,
                        ),
                        Provider::ClaudeCode => {
                            // Fallback: use depth-0 TextContains only
                            if filter.depth == 0 {
                                for criterion in &filter.criteria {
                                    if let FilterCriterion::TextContains(needle) = criterion {
                                        if crate::claudecode::session::session_tail_contains_text(
                                            session_id, needle, window,
                                        ) {
                                            return true;
                                        }
                                    }
                                }
                            }
                            false
                        }
                        Provider::Pi => {
                            // Same fallback as Claude Code: depth-0
                            // TextContains only.  Pi's TUI uses different
                            // ANSI codes than OpenCode, so the structured
                            // ToolFieldEquals filters built from OpenCode
                            // icons (→ ✱ ← ⚙) will not match — defer
                            // pi-specific TUI parsing to a future pass.
                            if filter.depth == 0 {
                                for criterion in &filter.criteria {
                                    if let FilterCriterion::TextContains(needle) = criterion {
                                        if crate::pi::session::session_tail_contains_text(
                                            session_id, needle, window,
                                        ) {
                                            return true;
                                        }
                                    }
                                }
                            }
                            false
                        }
                    }
                });

                if all_match {
                    log::debug!(
                        "find_session_by_filters: matched {} ({:?}) at margin {} with {}/{} filters",
                        session_id,
                        provider,
                        margin,
                        num_filters,
                        filters.len()
                    );
                    return Some(DetectedSession {
                        session_id: session_id.clone(),
                        provider: *provider,
                    });
                }
            }
        }
    }

    log::debug!("find_session_by_filters: no match found");
    None
}

// ── Public API ──────────────────────────────────────────────────

/// Find a session by matching text in its last N messages.
///
/// Gathers candidate sessions (optionally filtered by provider and project),
/// then searches each one's recent messages for the needle.
/// Returns the matching session, or an error if zero or multiple match.
pub fn find_session_by_match(opts: &MatchOptions) -> Result<DetectedSession> {
    // Resolve project dir to absolute path
    let project_path: Option<String> = match &opts.project_dir {
        Some(p) => {
            let path = std::path::PathBuf::from(p);
            let abs = if path.is_absolute() {
                path
            } else {
                std::env::current_dir().unwrap_or_default().join(path)
            };
            let resolved = abs.canonicalize().unwrap_or(abs);
            Some(resolved.to_string_lossy().to_string())
        }
        None => None,
    };

    let mut matches: Vec<DetectedSession> = Vec::new();

    let include_opencode =
        opts.provider_filter.is_none() || opts.provider_filter == Some(Provider::OpenCode);
    let include_claudecode =
        opts.provider_filter.is_none() || opts.provider_filter == Some(Provider::ClaudeCode);
    let include_pi = opts.provider_filter.is_none() || opts.provider_filter == Some(Provider::Pi);

    if include_opencode {
        if let Ok(sessions) = crate::opencode::list_sessions() {
            for s in sessions {
                if let Some(ref expected) = project_path {
                    if s.project_dir != *expected {
                        continue;
                    }
                }
                if crate::opencode::session_tail_contains_text(
                    &s.session_id,
                    &opts.needle,
                    opts.last_messages,
                ) {
                    matches.push(DetectedSession {
                        session_id: s.session_id,
                        provider: Provider::OpenCode,
                    });
                }
            }
        }
    }

    if include_claudecode {
        if let Ok(sessions) = crate::claudecode::session::list_sessions() {
            for s in sessions {
                if let Some(ref expected) = project_path {
                    if s.project_dir != *expected {
                        continue;
                    }
                }
                if crate::claudecode::session::session_tail_contains_text(
                    &s.session_id,
                    &opts.needle,
                    opts.last_messages,
                ) {
                    matches.push(DetectedSession {
                        session_id: s.session_id,
                        provider: Provider::ClaudeCode,
                    });
                }
            }
        }
    }

    if include_pi {
        if let Ok(sessions) = crate::pi::session::list_sessions() {
            for s in sessions {
                if let Some(ref expected) = project_path {
                    if s.project_dir != *expected {
                        continue;
                    }
                }
                if crate::pi::session::session_tail_contains_text(
                    &s.session_id,
                    &opts.needle,
                    opts.last_messages,
                ) {
                    matches.push(DetectedSession {
                        session_id: s.session_id,
                        provider: Provider::Pi,
                    });
                }
            }
        }
    }

    match matches.len() {
        0 => bail!(
            "No session found matching \"{}\" in last {} messages",
            opts.needle,
            opts.last_messages
        ),
        1 => Ok(matches.into_iter().next().unwrap()),
        n => {
            let mut msg = format!(
                "Ambiguous: {} sessions match \"{}\". Use --type or --project to narrow:\n",
                n, opts.needle
            );
            for m in &matches {
                msg.push_str(&format!("  {} ({:?})\n", m.session_id, m.provider));
            }
            bail!("{}", msg.trim_end());
        }
    }
}

/// Try to auto-detect the current session.
///
/// Strategy:
/// 1. Check env vars `OPENCODE_SESSION_ID` / `CLAUDE_SESSION_ID` /
///    `PI_SESSION_ID` (the latter is exported by the
///    `pi-env-session-id` extension at session_start)
/// 2. If in tmux → tmux scrollback fingerprinting
/// 3. Otherwise bail with error
pub fn detect_current_session() -> Result<DetectedSession> {
    // Step 1: Check authoritative env vars
    if let Ok(sid) = env::var("OPENCODE_SESSION_ID") {
        if !sid.is_empty() {
            log::debug!("detect_current_session: found OPENCODE_SESSION_ID={}", sid);
            return Ok(DetectedSession {
                session_id: sid,
                provider: Provider::OpenCode,
            });
        }
    }
    if let Ok(sid) = env::var("CLAUDE_SESSION_ID") {
        if !sid.is_empty() {
            log::debug!("detect_current_session: found CLAUDE_SESSION_ID={}", sid);
            return Ok(DetectedSession {
                session_id: sid,
                provider: Provider::ClaudeCode,
            });
        }
    }
    if let Ok(sid) = env::var("PI_SESSION_ID") {
        if !sid.is_empty() {
            log::debug!("detect_current_session: found PI_SESSION_ID={}", sid);
            return Ok(DetectedSession {
                session_id: sid,
                provider: Provider::Pi,
            });
        }
    }

    // Step 2: Try tmux scrollback fingerprinting
    #[cfg(unix)]
    if is_tmux_available() {
        log::debug!("detect_current_session: tmux available, trying fingerprinting");
        if let Some(result) = detect_last_session_via_tmux(None) {
            return Ok(result);
        }
    }

    // Step 3: No detection possible
    bail!(
        "Could not detect current session.\n\
         Set OPENCODE_SESSION_ID, CLAUDE_SESSION_ID, or PI_SESSION_ID, \
         or run inside tmux for scrollback fingerprinting."
    );
}

/// Detect the last AI session used in the current tmux pane.
///
/// Detection strategy:
/// 1. If in tmux → capture scrollback → parse messages → build filters → search
/// 2. Otherwise bail with error
pub fn detect_last_session(opts: &LastSessionOptions) -> Result<DetectedSession> {
    // If a scrollback file is provided, use it directly instead of tmux
    if let Some(ref path) = opts.scrollback_file {
        log::debug!("detect_last_session: reading scrollback from {:?}", path);
        let scrollback = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read scrollback file {:?}", path))?;
        return detect_last_session_from_scrollback(&scrollback, opts.provider_filter);
    }

    #[cfg(unix)]
    if is_tmux_available() {
        log::debug!("detect_last_session: tmux available, trying fingerprinting");
        if let Some(result) = detect_last_session_via_tmux(opts.provider_filter) {
            return Ok(result);
        }
    }

    bail!(
        "Could not detect last session.\n\
         Run inside tmux for scrollback fingerprinting, \
         or use --session to specify explicitly."
    );
}

/// Run the detection pipeline from pre-captured scrollback content.
fn detect_last_session_from_scrollback(
    scrollback: &str,
    provider_filter: Option<Provider>,
) -> Result<DetectedSession> {
    let messages = parse_pane_messages(scrollback);
    if messages.is_empty() {
        bail!("Could not detect last session.\nNo messages parsed from scrollback file.");
    }

    log::debug!(
        "detect_last_session_from_scrollback: parsed {} messages",
        messages.len()
    );

    let filters = build_filters(&messages);
    if filters.is_empty() {
        bail!("Could not detect last session.\nNo filters built from scrollback file.");
    }

    log::debug!(
        "detect_last_session_from_scrollback: built {} filters",
        filters.len()
    );

    find_session_by_filters(&filters, provider_filter).ok_or_else(|| {
        anyhow::anyhow!(
            "Could not detect last session.\n\
             Filters built from scrollback file matched no session."
        )
    })
}

// ── Tmux helpers ────────────────────────────────────────────────

/// Check if we're running inside a tmux session.
#[cfg(unix)]
fn is_tmux_available() -> bool {
    env::var("TMUX").is_ok_and(|v| !v.is_empty())
}

/// Capture pane scrollback for session detection.
#[cfg(unix)]
fn capture_pane_scrollback(pane_id: &str) -> Option<String> {
    let output = std::process::Command::new("tmux")
        .args([
            "capture-pane",
            "-p",
            "-e",
            "-S",
            &format!("-{}", LAST_SESSION_SCROLLBACK_LINES),
            "-t",
            pane_id,
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Detect the last session by reading the current tmux pane's scrollback.
///
/// Algorithm:
/// 1. Get pane info
/// 2. Capture pane scrollback (3000 lines)
/// 3. Parse messages from TUI output
/// 4. Build structured filters
/// 5. Search all sessions for a match
#[cfg(unix)]
fn detect_last_session_via_tmux(provider_filter: Option<Provider>) -> Option<DetectedSession> {
    let pane_id = env::var("TMUX_PANE").ok()?;

    log::debug!("detect_last_session_via_tmux: pane_id={}", pane_id);

    // Capture pane scrollback
    let scrollback = capture_pane_scrollback(&pane_id)?;

    log::trace!(
        "detect_last_session_via_tmux: captured {} bytes of scrollback",
        scrollback.len()
    );

    // Parse messages from TUI output
    let messages = parse_pane_messages(&scrollback);
    if messages.is_empty() {
        log::debug!("detect_last_session_via_tmux: no messages parsed from scrollback");
        return None;
    }

    // Build structured filters
    let filters = build_filters(&messages);
    if filters.is_empty() {
        log::debug!("detect_last_session_via_tmux: no filters built from messages");
        return None;
    }

    // Search all sessions
    find_session_by_filters(&filters, provider_filter)
}

// ── Tests ───────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // === CLI parsing tests (kept) ===

    #[test]
    fn cli_accepts_last_session_command() {
        use clap::Parser;

        let args = crate::cli::Args::try_parse_from(["ai-audit", "last-session"])
            .expect("bare last-session should work");
        match args.command {
            crate::cli::Commands::LastSession { session_type, .. } => {
                assert!(session_type.is_none());
            }
            _ => panic!("expected LastSession command"),
        }
    }

    #[test]
    fn cli_last_session_with_type() {
        use clap::Parser;

        let args = crate::cli::Args::try_parse_from(["ai-audit", "last-session", "-t", "opencode"])
            .expect("last-session with -t should work");
        match args.command {
            crate::cli::Commands::LastSession { session_type, .. } => {
                assert!(session_type.is_some());
            }
            _ => panic!("expected LastSession command"),
        }
    }

    // === strip_ansi tests ===

    #[test]
    fn test_strip_ansi() {
        assert_eq!(strip_ansi("hello"), "hello");
        assert_eq!(strip_ansi("\x1b[31mred\x1b[0m"), "red");
        assert_eq!(strip_ansi("\x1b[38;2;242;244;248mtext\x1b[0m"), "text");
        assert_eq!(strip_ansi(""), "");
        assert_eq!(
            strip_ansi("\x1b[1m\x1b[38;2;125;132;143m→ Read\x1b[0m"),
            "→ Read"
        );
    }

    // === classify_tui_line tests ===

    #[test]
    fn test_classify_tui_line_assistant_text() {
        let raw = "\x1b[38;2;242;244;248mThis is assistant text output\x1b[0m";
        match classify_tui_line(raw) {
            TuiLineKind::AssistantText(text) => {
                // Stores raw ANSI for segment extraction
                assert!(text.contains("\x1b["));
                assert!(text.contains("This is assistant text output"));
            }
            other => panic!("expected AssistantText, got {:?}", other),
        }
    }

    #[test]
    fn test_classify_tui_line_tool_invocation() {
        let raw = "\x1b[38;2;125;132;143m→ Read src/main.rs [offset=0, limit=50]\x1b[0m";
        match classify_tui_line(raw) {
            TuiLineKind::ToolInvocation(text) => {
                assert!(text.starts_with('→'));
                assert!(text.contains("Read"));
            }
            other => panic!("expected ToolInvocation, got {:?}", other),
        }
    }

    #[test]
    fn test_classify_tui_line_panel() {
        let raw = "\x1b[48;2;30;30;30m  ┃  Some panel content  \x1b[0m";
        assert_eq!(classify_tui_line(raw), TuiLineKind::PanelContent);
    }

    #[test]
    fn test_classify_tui_line_panel_with_assistant_text_is_assistant() {
        // Panel line with assistant text foreground color should be
        // AssistantText, not PanelContent — it carries session content.
        let raw = "\x1b[38;2;0;206;209m┃\x1b[48;2;26;26;26m  \x1b[38;2;242;244;248mFind vigil-watch PermissionEntry struct\x1b[0m";
        match classify_tui_line(raw) {
            TuiLineKind::AssistantText(text) => {
                assert!(text.contains("Find vigil-watch"));
            }
            other => panic!(
                "panel line with assistant text color should be AssistantText, got {:?}",
                other
            ),
        }
    }

    #[test]
    fn test_classify_tui_line_panel_empty_stays_panel() {
        // Empty panel border line (no assistant text color) stays PanelContent.
        let raw = "\x1b[38;2;0;206;209m┃\x1b[48;2;26;26;26m                              \x1b[0m";
        assert_eq!(classify_tui_line(raw), TuiLineKind::PanelContent);
    }

    #[test]
    fn test_classify_tui_line_completion() {
        let raw = "  ▣  Sisyphus · claude-opus-4-6 · 9.9s";
        assert_eq!(classify_tui_line(raw), TuiLineKind::CompletionMarker);
    }

    #[test]
    fn test_classify_tui_line_footer() {
        let raw = "  ╹▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀";
        assert_eq!(classify_tui_line(raw), TuiLineKind::Footer);
    }

    #[test]
    fn test_classify_tui_line_empty() {
        assert_eq!(classify_tui_line(""), TuiLineKind::Empty);
        assert_eq!(classify_tui_line("   "), TuiLineKind::Empty);
        // ANSI codes that produce empty stripped content
        assert_eq!(classify_tui_line("\x1b[0m  \x1b[0m"), TuiLineKind::Empty);
    }

    #[test]
    fn test_classify_tui_line_dim_gray_not_tool() {
        // Dim gray line WITHOUT tool icon should NOT be ToolInvocation
        let raw = "\x1b[38;2;125;132;143mSome info text without icon\x1b[0m";
        match classify_tui_line(raw) {
            TuiLineKind::ToolInvocation(_) => {
                panic!("should NOT be ToolInvocation without tool icon")
            }
            _ => {} // Any other kind is acceptable
        }
    }

    // === parse_tool_line tests ===

    #[test]
    fn test_parse_tool_line_read() {
        let result = parse_tool_line("→ Read src/foo.rs [offset=100, limit=50]");
        assert!(result.is_some());
        let tc = result.unwrap();
        assert_eq!(tc.tool_name, "read");
        assert!(tc
            .fields
            .contains(&("filePath".to_string(), "src/foo.rs".to_string())));
        assert!(tc
            .fields
            .contains(&("offset".to_string(), "100".to_string())));
        assert!(tc.fields.contains(&("limit".to_string(), "50".to_string())));
    }

    #[test]
    fn test_parse_tool_line_grep() {
        let result = parse_tool_line("✱ Grep \"pattern\" in src/");
        assert!(result.is_some());
        let tc = result.unwrap();
        assert_eq!(tc.tool_name, "grep");
        assert!(tc
            .fields
            .contains(&("pattern".to_string(), "pattern".to_string())));
        assert!(tc
            .fields
            .contains(&("path".to_string(), "src/".to_string())));
    }

    #[test]
    fn test_parse_tool_line_edit() {
        let result = parse_tool_line("← Edit src/foo.rs");
        assert!(result.is_some());
        let tc = result.unwrap();
        assert_eq!(tc.tool_name, "edit");
        assert!(tc
            .fields
            .contains(&("filePath".to_string(), "src/foo.rs".to_string())));
    }

    #[test]
    fn test_parse_tool_line_task() {
        let result = parse_tool_line("⚙ background_output [task_id=bg_xxx]");
        assert!(result.is_some());
        let tc = result.unwrap();
        assert_eq!(tc.tool_name, "background_output");
        assert!(tc
            .fields
            .contains(&("task_id".to_string(), "bg_xxx".to_string())));
    }

    // === parse_pane_messages tests ===

    #[test]
    fn test_parse_pane_messages_basic() {
        // Build a minimal TUI fixture with ANSI codes
        let content = format!(
            "{}\n{}\n{}\n{}\n{}\n{}\n",
            "\x1b[38;2;242;244;248mI'll help you refactor the authentication module now.\x1b[0m",
            "\x1b[38;2;125;132;143m→ Read src/auth.rs [offset=0, limit=100]\x1b[0m",
            "",
            "  ▣  Sisyphus · claude-opus-4-6 · 5.2s",
            "",
            "  ╹▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀",
        );

        let messages = parse_pane_messages(&content);
        assert_eq!(messages.len(), 1, "should find 1 message");
        assert_eq!(messages[0].depth, 0);
        assert!(!messages[0].text_lines.is_empty(), "should have text lines");
        // text_lines contain raw ANSI; extract segments to check content
        let seg = longest_ansi_segment(&messages[0].text_lines[0]);
        assert!(
            seg.as_deref()
                .is_some_and(|s| s.contains("refactor the authentication")),
            "longest segment should contain assistant message, got {:?}",
            seg
        );
        assert!(!messages[0].tool_calls.is_empty(), "should have tool calls");
        assert_eq!(messages[0].tool_calls[0].tool_name, "read");
    }

    #[test]
    fn test_parse_pane_messages_multiple() {
        let content = format!(
            "{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n",
            "\x1b[38;2;242;244;248mFirst assistant message with enough text to be useful.\x1b[0m",
            "  ▣  Sisyphus · claude-opus-4-6 · 3.0s",
            "",
            "\x1b[38;2;242;244;248mSecond assistant message with different content here.\x1b[0m",
            "\x1b[38;2;125;132;143m✱ Grep \"pattern\" in src/\x1b[0m",
            "  ▣  Sisyphus · claude-opus-4-6 · 5.2s",
            "",
            "  ╹▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀",
        );

        let messages = parse_pane_messages(&content);
        assert!(
            messages.len() >= 2,
            "should find at least 2 messages, got {}",
            messages.len()
        );
        // Depth 0 = closest to footer (second message)
        assert_eq!(messages[0].depth, 0);
        assert_eq!(messages[1].depth, 1);
    }

    #[test]
    fn test_parse_pane_messages_no_footer() {
        let content = "just some plain text\nno TUI elements here\n";
        let messages = parse_pane_messages(content);
        assert!(messages.is_empty(), "no footer means no messages");
    }

    // === build_filters tests ===

    #[test]
    fn test_parse_pane_messages_panel_text_extracted() {
        // Reproduces the bug: a QUEUED session where all visible content is
        // inside ┃ panels with background color.  The system-reminder text
        // must be extracted as AssistantText, not discarded as PanelContent.
        let content = [
            // Empty panel border (no assistant color) → PanelContent
            "\x1b[38;2;0;206;209m┃\x1b[48;2;26;26;26m                              \x1b[0m",
            // System reminder text (has assistant color inside panel)
            "\x1b[38;2;0;206;209m┃\x1b[48;2;26;26;26m  \x1b[38;2;242;244;248m<system-reminder>\x1b[0m",
            "\x1b[38;2;0;206;209m┃\x1b[48;2;26;26;26m  \x1b[38;2;242;244;248mFind vigil-watch PermissionEntry struct and list_all_permissions to understand value field\x1b[0m",
            "\x1b[38;2;0;206;209m┃\x1b[48;2;26;26;26m  \x1b[38;2;242;244;248m</system-reminder>\x1b[0m",
            // Empty panel border → PanelContent
            "\x1b[38;2;0;206;209m┃\x1b[48;2;26;26;26m                              \x1b[0m",
            // Completion marker
            "  ▣  Sisyphus · claude-opus-4-6",
            "",
            // Footer
            "  ╹▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀",
        ]
        .join("\n");

        let messages = parse_pane_messages(&content);
        assert!(
            !messages.is_empty(),
            "should extract messages from panel text"
        );

        // The system-reminder text lines should be collected
        let all_text: Vec<&str> = messages
            .iter()
            .flat_map(|m| m.text_lines.iter().map(|s| s.as_str()))
            .collect();
        assert!(
            all_text.iter().any(|t| t.contains("PermissionEntry")),
            "should have extracted the system-reminder text, got: {:?}",
            all_text
        );

        // build_filters should produce a filter from the long text
        let filters = build_filters(&messages);
        assert!(
            !filters.is_empty(),
            "system-reminder text should produce filters"
        );
        assert!(
            filters.iter().any(|f| f.criteria.iter().any(|c| matches!(
                c,
                FilterCriterion::TextContains(s) if s.contains("PermissionEntry")
            ))),
            "filter should contain the distinctive text"
        );
    }

    #[test]
    fn test_build_filters_prefers_long_segments_over_short_fragments() {
        // Reproduces the CSS-diff scenario: messages near the footer have
        // syntax-highlighted fragments (all < MIN_NEEDLE_LENGTH), while a
        // deeper message has a long distinctive text segment.  The scoring
        // must rank the deeper message higher so its segment becomes a filter.
        let messages = vec![
            // msg[0] depth=0: model selector, 15 chars — too short
            PaneMessage {
                text_lines: vec![
                    "\x1b[38;2;242;244;248mClaude Opus 4.6\x1b[0m".to_string(),
                ],
                tool_calls: vec![],
                depth: 0,
            },
            // msg[1] depth=1: "Done.", 5 chars — too short
            PaneMessage {
                text_lines: vec!["\x1b[38;2;242;244;248mDone.\x1b[0m".to_string()],
                tool_calls: vec![],
                depth: 1,
            },
            // msg[2] depth=2: CSS diff with syntax-highlighted fragments, all < 20 chars
            PaneMessage {
                text_lines: vec![
                    "\x1b[38;2;100;200;100m.back-btn\x1b[0m \x1b[38;2;200;100;100m{\x1b[0m".to_string(),
                    "\x1b[38;2;150;150;255m  padding\x1b[0m\x1b[38;2;200;200;200m:\x1b[0m \x1b[38;2;200;150;50m0rem\x1b[0m".to_string(),
                    "\x1b[38;2;200;100;100m}\x1b[0m".to_string(),
                ],
                tool_calls: vec![],
                depth: 2,
            },
            // msg[3] depth=3: tool calls only (Read + Edit)
            PaneMessage {
                text_lines: vec![],
                tool_calls: vec![
                    ParsedToolCall {
                        tool_name: "Read".to_string(),
                        fields: vec![("filePath".to_string(), "style.css".to_string())],
                    },
                    ParsedToolCall {
                        tool_name: "Edit".to_string(),
                        fields: vec![("filePath".to_string(), "style.css".to_string())],
                    },
                ],
                depth: 3,
            },
            // msg[4] depth=4: long distinctive text, 46 chars — the good one
            PaneMessage {
                text_lines: vec![
                    "\x1b[38;2;242;244;248m`.back-btn` should have `padding: 0rem .6rem;`\x1b[0m"
                        .to_string(),
                ],
                tool_calls: vec![],
                depth: 4,
            },
        ];

        let filters = build_filters(&messages);
        assert!(
            !filters.is_empty(),
            "should produce at least one filter from the long segment in msg[4]"
        );

        // The filter with the TextContains criterion must come from msg[4] (depth=4)
        // because it has the only segment >= MIN_NEEDLE_LENGTH.
        let text_filter = filters
            .iter()
            .find(|f| {
                f.criteria
                    .iter()
                    .any(|c| matches!(c, FilterCriterion::TextContains(_)))
            })
            .expect("should have a TextContains filter");
        assert_eq!(
            text_filter.depth, 4,
            "TextContains filter should come from msg[4] (depth=4), not shallow messages"
        );

        // Verify the needle content is from msg[4]
        let needle = text_filter
            .criteria
            .iter()
            .find_map(|c| match c {
                FilterCriterion::TextContains(s) => Some(s.as_str()),
                _ => None,
            })
            .unwrap();
        assert!(
            needle.contains("`.back-btn` should have"),
            "needle should contain the distinctive text from msg[4], got: {:?}",
            needle
        );
    }

    #[test]
    fn test_build_filters_from_messages() {
        // text_lines must contain raw ANSI so longest_ansi_segment works
        let messages = vec![
            PaneMessage {
                text_lines: vec![
                    "\x1b[38;2;242;244;248mShort line\x1b[0m".to_string(),
                    "\x1b[38;2;242;244;248mThis is a long enough assistant text line for filtering purposes\x1b[0m".to_string(),
                ],
                tool_calls: vec![ParsedToolCall {
                    tool_name: "read".to_string(),
                    fields: vec![("filePath".to_string(), "src/main.rs".to_string())],
                }],
                depth: 0,
            },
            PaneMessage {
                text_lines: vec![
                    "\x1b[38;2;242;244;248mAnother message without tools but long enough text here\x1b[0m".to_string()
                ],
                tool_calls: vec![],
                depth: 1,
            },
        ];

        let filters = build_filters(&messages);
        assert!(!filters.is_empty(), "should produce at least one filter");

        // First filter should be from message with both text+tools (depth 0)
        let f0 = &filters[0];
        assert_eq!(f0.depth, 0);
        assert!(
            f0.criteria
                .iter()
                .any(|c| matches!(c, FilterCriterion::TextContains(_))),
            "should have TextContains criterion"
        );
        assert!(
            f0.criteria
                .iter()
                .any(|c| matches!(c, FilterCriterion::ToolFieldEquals { .. })),
            "should have ToolFieldEquals criterion"
        );
    }

    /// Integration test against the real saved scrollback from the bug report.
    /// Skipped if the scrollback file is absent (e.g. CI).
    #[test]
    fn test_build_filters_against_saved_scrollback() {
        let path = "/tmp/pane-579-scrollback-raw.txt";
        let Ok(scrollback) = std::fs::read_to_string(path) else {
            eprintln!("SKIP: {} not found", path);
            return;
        };

        let messages = parse_pane_messages(&scrollback);
        assert!(
            !messages.is_empty(),
            "should parse messages from saved scrollback"
        );

        let filters = build_filters(&messages);
        assert!(
            !filters.is_empty(),
            "should produce filters from saved scrollback (was 0 before fix)"
        );

        // Must have at least one TextContains filter
        let has_text_filter = filters.iter().any(|f| {
            f.criteria
                .iter()
                .any(|c| matches!(c, FilterCriterion::TextContains(_)))
        });
        assert!(
            has_text_filter,
            "should have at least one TextContains filter from the scrollback"
        );

        // Log what we got for diagnostic purposes
        for (i, f) in filters.iter().enumerate() {
            for c in &f.criteria {
                match c {
                    FilterCriterion::TextContains(needle) => {
                        eprintln!(
                            "  filter[{}] depth={}: TextContains({:?})",
                            i, f.depth, needle
                        );
                    }
                    FilterCriterion::ToolFieldEquals {
                        tool_name,
                        field,
                        value,
                    } => {
                        eprintln!(
                            "  filter[{}] depth={}: ToolFieldEquals({}:{}={:?})",
                            i, f.depth, tool_name, field, value
                        );
                    }
                }
            }
        }
    }
}

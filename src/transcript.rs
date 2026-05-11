use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
    System,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::System => "system",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryType {
    Text,
    ToolUse,
    ToolResult,
    /// Failed tool-call response: user refusal, dismissed prompt, tool error,
    /// or any other non-success outcome. Sibling of `ToolResult` (success).
    /// Distinct from `Error`, which is a message-level / API-level failure.
    ToolError,
    Thinking,
    Error,
}

impl EntryType {
    pub fn as_str(&self) -> &'static str {
        match self {
            EntryType::Text => "text",
            EntryType::ToolUse => "tool_use",
            EntryType::ToolResult => "tool_result",
            EntryType::ToolError => "tool_error",
            EntryType::Thinking => "thinking",
            EntryType::Error => "error",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TranscriptEntry {
    pub timestamp: DateTime<Utc>,
    pub role: Role,
    pub entry_type: EntryType,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_input: Option<Value>,
}

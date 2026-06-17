//! Provider-agnostic extraction of file write/edit operations from a
//! parsed [`crate::transcript`] stream.
//!
//! AI assistants (Claude Code, OpenCode, pi) all expose roughly the
//! same set of file-touching tools — `Write` / `Edit` / `MultiEdit`
//! and their snake_case / camelCase variants — but each provider
//! names the JSON keys differently:
//!
//! | Provider     | Path key                          | Old/new keys                  | Replace-all key |
//! |--------------|-----------------------------------|-------------------------------|-----------------|
//! | Claude Code  | `file_path`                       | `old_string` / `new_string`   | `replace_all`   |
//! | OpenCode     | `filePath` (fallback `file_path`) | `oldString` / `newString`     | `replaceAll`    |
//! | pi           | `path`                            | `old_string` / `new_string`   | `replace_all`   |
//!
//! This module normalises those variants into a single [`FileOp`]
//! enum so downstream consumers (`session edited-files` stat / apply /
//! extract modes) can treat them uniformly.  The variant is decided
//! by the **shape of `tool_input`** (which keys are present), NOT by
//! the tool name, so a hypothetical future provider that calls its
//! write tool `save_file` will still be recognised as long as the
//! payload has a `content` field plus one of the recognised path
//! keys.

use crate::transcript::{EntryType, TranscriptEntry};
use serde::Serialize;
use serde_json::Value;

/// Tool names (lower-cased, compared case-insensitively) that
/// write or edit files.  This is the SINGLE SOURCE OF TRUTH for the
/// write-tool set; the legacy private list in
/// `crate::cli::action::transcript` defers to [`is_write_tool`].
pub const WRITE_TOOL_NAMES: &[&str] = &[
    "write",
    "edit",
    "multiedit",
    "multi_edit",
    "createfile",
    "create",
];

/// True if `name` (any case) is a known write/edit tool.
pub fn is_write_tool(name: &str) -> bool {
    WRITE_TOOL_NAMES.contains(&name.to_ascii_lowercase().as_str())
}

/// Extract the target file path from a tool input value, trying the
/// recognised keys in order: `file_path` → `filePath` → `path`.
pub fn input_path(input: &Value) -> Option<&str> {
    input
        .get("file_path")
        .or_else(|| input.get("filePath"))
        .or_else(|| input.get("path"))
        .and_then(|p| p.as_str())
}

/// A single recorded file operation, normalised across providers.
///
/// The `op` discriminator + `snake_case` rename matches the rest of
/// the JSON surface emitted by ai-audit (see [`crate::transcript`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum FileOp {
    /// Full-content write (creation or overwrite).
    Write { path: String, content: String },
    /// In-place substitution.
    Edit {
        path: String,
        old: String,
        new: String,
        replace_all: bool,
    },
}

impl FileOp {
    /// The file path this op targets, regardless of variant.
    pub fn path(&self) -> &str {
        match self {
            FileOp::Write { path, .. } | FileOp::Edit { path, .. } => path,
        }
    }
}

/// Per-file aggregation used by the `--stat-like` default output of
/// `session edited-files`.
///
/// `insertions` and `deletions` are git-`--stat`-style line counts
/// aggregated by [`stat`].  They are *approximations*: a [`FileOp::Write`]
/// has no prior version to diff against so its entire content counts as
/// insertions, and a [`FileOp::Edit`] with `replace_all=true` counts the
/// old/new blocks ONCE (we cannot know occurrence count without the
/// source file).  See [`stat`] for the full rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EditedFile {
    pub path: String,
    /// Number of [`FileOp::Write`] ops touching `path`.
    pub writes: usize,
    /// Number of [`FileOp::Edit`] ops touching `path`.
    pub edits: usize,
    /// True iff the FIRST op for `path` is a [`FileOp::Write`] — the
    /// strongest signal we have that the file was created (rather
    /// than just modified) during the session.
    pub created: bool,
    /// Total lines added (writes count entirely; edits count `new`).
    pub insertions: usize,
    /// Total lines removed (edits count `old`; writes contribute 0).
    pub deletions: usize,
}

/// Count the number of lines a literal block represents for stat
/// purposes.  An empty string is 0 lines; otherwise it's the number of
/// `\n` plus 1 if the block does not end in `\n` (so `"a\nb"` = 2,
/// `"a\nb\n"` = 2, `""` = 0, `"a"` = 1).
pub fn count_lines(s: &str) -> usize {
    if s.is_empty() {
        return 0;
    }
    let nl = s.matches('\n').count();
    if s.ends_with('\n') {
        nl
    } else {
        nl + 1
    }
}

/// Extract a chronological stream of [`FileOp`]s from a parsed
/// transcript.
///
/// Rules:
///
/// * Only [`EntryType::ToolUse`] entries are considered.
/// * The tool name must match [`is_write_tool`].
/// * `tool_input` must be present and carry a usable path
///   ([`input_path`]).
/// * The variant is decided by the *shape* of `tool_input`:
///   * an `edits` array → flattened into one [`FileOp::Edit`] per
///     sub-entry (preserving order);
///   * else a `content` field → [`FileOp::Write`];
///   * else old/new keys → single [`FileOp::Edit`];
///   * else the entry is skipped (malformed / unrecognised payload).
pub fn extract_ops(entries: &[TranscriptEntry]) -> Vec<FileOp> {
    let mut ops = Vec::new();
    for entry in entries {
        if !matches!(entry.entry_type, EntryType::ToolUse) {
            continue;
        }
        let tool_name = match &entry.tool_name {
            Some(n) => n,
            None => continue,
        };
        if !is_write_tool(tool_name) {
            continue;
        }
        let input = match &entry.tool_input {
            Some(v) => v,
            None => continue,
        };
        let path = match input_path(input) {
            Some(p) => p.to_string(),
            None => continue,
        };

        // Multi-edit: a non-empty `edits` array always wins over a
        // co-located `content` field (some providers send both).
        if let Some(arr) = input.get("edits").and_then(|v| v.as_array()) {
            if !arr.is_empty() {
                for sub in arr {
                    if let Some(op) = edit_from_value(&path, sub) {
                        ops.push(op);
                    }
                }
                continue;
            }
        }

        // Plain write: presence of a string `content` field.
        if let Some(content) = input.get("content").and_then(|v| v.as_str()) {
            ops.push(FileOp::Write {
                path,
                content: content.to_string(),
            });
            continue;
        }

        // Single edit: old/new keys on the top-level payload.
        if let Some(op) = edit_from_value(&path, input) {
            ops.push(op);
        }
    }
    ops
}

/// Build a [`FileOp::Edit`] from a JSON object carrying the standard
/// old/new keys.  Returns `None` if either key is absent or not a
/// string — callers treat that as "unrecognised payload, skip".
fn edit_from_value(path: &str, v: &Value) -> Option<FileOp> {
    let old = v
        .get("old_string")
        .or_else(|| v.get("oldString"))
        .and_then(|s| s.as_str())?;
    let new = v
        .get("new_string")
        .or_else(|| v.get("newString"))
        .and_then(|s| s.as_str())?;
    let replace_all = v
        .get("replace_all")
        .or_else(|| v.get("replaceAll"))
        .and_then(|s| s.as_bool())
        .unwrap_or(false);
    Some(FileOp::Edit {
        path: path.to_string(),
        old: old.to_string(),
        new: new.to_string(),
        replace_all,
    })
}

/// Aggregate ops per file path, preserving first-seen order so the
/// stat output reflects the chronological "touch order" of the
/// session.
///
/// `insertions` / `deletions` line counts follow git's `--stat` model
/// but with two documented approximations:
///
/// * [`FileOp::Write`] has no prior version to diff against, so its
///   full content counts as insertions and contributes zero deletions.
/// * [`FileOp::Edit`] with `replace_all=true` counts the old/new
///   blocks ONCE — without the source file we cannot know the
///   occurrence count.  A single edit therefore adds `count_lines(new)`
///   to insertions and `count_lines(old)` to deletions.
pub fn stat(ops: &[FileOp]) -> Vec<EditedFile> {
    let mut order: Vec<String> = Vec::new();
    let mut by_path: std::collections::HashMap<String, EditedFile> =
        std::collections::HashMap::new();
    for op in ops {
        let path = op.path().to_string();
        let entry = by_path.entry(path.clone()).or_insert_with(|| {
            order.push(path.clone());
            EditedFile {
                path: path.clone(),
                writes: 0,
                edits: 0,
                created: matches!(op, FileOp::Write { .. }),
                insertions: 0,
                deletions: 0,
            }
        });
        match op {
            FileOp::Write { content, .. } => {
                entry.writes += 1;
                entry.insertions += count_lines(content);
            }
            FileOp::Edit { old, new, .. } => {
                entry.edits += 1;
                entry.deletions += count_lines(old);
                entry.insertions += count_lines(new);
            }
        }
    }
    order
        .into_iter()
        .map(|p| by_path.remove(&p).expect("path was inserted above"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::{EntryType, Role};
    use chrono::Utc;
    use serde_json::json;

    fn tool_use(name: &str, input: serde_json::Value) -> TranscriptEntry {
        TranscriptEntry {
            timestamp: Utc::now(),
            role: Role::Assistant,
            entry_type: EntryType::ToolUse,
            content: String::new(),
            tool_name: Some(name.to_string()),
            tool_input: Some(input),
        }
    }

    fn text_entry(s: &str) -> TranscriptEntry {
        TranscriptEntry {
            timestamp: Utc::now(),
            role: Role::Assistant,
            entry_type: EntryType::Text,
            content: s.to_string(),
            tool_name: None,
            tool_input: None,
        }
    }

    // ---- is_write_tool / input_path -----------------------------------

    #[test]
    fn test_is_write_tool_recognises_all_canonical_names() {
        for n in [
            "Write",
            "Edit",
            "MultiEdit",
            "CreateFile",
            "write",
            "edit",
            "multi_edit",
            "create",
            "MULTIEDIT",
        ] {
            assert!(is_write_tool(n), "tool {n} should be recognised");
        }
    }

    #[test]
    fn test_is_write_tool_rejects_others() {
        for n in ["Read", "Bash", "Glob", "Grep", "", "Multi-Edit"] {
            assert!(!is_write_tool(n), "tool {n} should NOT be recognised");
        }
    }

    #[test]
    fn test_input_path_prefers_file_path() {
        let v = json!({"file_path": "a", "filePath": "b", "path": "c"});
        assert_eq!(input_path(&v), Some("a"));
    }

    #[test]
    fn test_input_path_falls_back_to_camel_case() {
        let v = json!({"filePath": "b", "path": "c"});
        assert_eq!(input_path(&v), Some("b"));
    }

    #[test]
    fn test_input_path_falls_back_to_path() {
        let v = json!({"path": "c"});
        assert_eq!(input_path(&v), Some("c"));
    }

    #[test]
    fn test_input_path_absent() {
        assert_eq!(input_path(&json!({})), None);
        assert_eq!(input_path(&json!({"file_path": 42})), None);
    }

    // ---- extract_ops: per-provider shapes ------------------------------

    #[test]
    fn test_extract_ops_claudecode_write() {
        let entries = vec![tool_use(
            "Write",
            json!({"file_path": "/a/b.rs", "content": "fn main() {}"}),
        )];
        assert_eq!(
            extract_ops(&entries),
            vec![FileOp::Write {
                path: "/a/b.rs".into(),
                content: "fn main() {}".into(),
            }]
        );
    }

    #[test]
    fn test_extract_ops_claudecode_edit_with_replace_all() {
        let entries = vec![tool_use(
            "Edit",
            json!({
                "file_path": "/a/b.rs",
                "old_string": "foo",
                "new_string": "bar",
                "replace_all": true,
            }),
        )];
        assert_eq!(
            extract_ops(&entries),
            vec![FileOp::Edit {
                path: "/a/b.rs".into(),
                old: "foo".into(),
                new: "bar".into(),
                replace_all: true,
            }]
        );
    }

    #[test]
    fn test_extract_ops_claudecode_multiedit_flattens_in_order() {
        let entries = vec![tool_use(
            "MultiEdit",
            json!({
                "file_path": "/a/b.rs",
                "edits": [
                    {"old_string": "x", "new_string": "1"},
                    {"old_string": "y", "new_string": "2", "replace_all": true},
                ],
            }),
        )];
        assert_eq!(
            extract_ops(&entries),
            vec![
                FileOp::Edit {
                    path: "/a/b.rs".into(),
                    old: "x".into(),
                    new: "1".into(),
                    replace_all: false,
                },
                FileOp::Edit {
                    path: "/a/b.rs".into(),
                    old: "y".into(),
                    new: "2".into(),
                    replace_all: true,
                },
            ]
        );
    }

    #[test]
    fn test_extract_ops_opencode_camel_case_write() {
        let entries = vec![tool_use(
            "write",
            json!({"filePath": "/a/b.rs", "content": "hi"}),
        )];
        assert_eq!(
            extract_ops(&entries),
            vec![FileOp::Write {
                path: "/a/b.rs".into(),
                content: "hi".into(),
            }]
        );
    }

    #[test]
    fn test_extract_ops_opencode_camel_case_edit() {
        let entries = vec![tool_use(
            "edit",
            json!({
                "filePath": "/a/b.rs",
                "oldString": "foo",
                "newString": "bar",
                "replaceAll": true,
            }),
        )];
        assert_eq!(
            extract_ops(&entries),
            vec![FileOp::Edit {
                path: "/a/b.rs".into(),
                old: "foo".into(),
                new: "bar".into(),
                replace_all: true,
            }]
        );
    }

    #[test]
    fn test_extract_ops_opencode_multi_edit() {
        let entries = vec![tool_use(
            "multi_edit",
            json!({
                "filePath": "/a/b.rs",
                "edits": [
                    {"oldString": "x", "newString": "1"},
                    {"oldString": "y", "newString": "2"},
                ],
            }),
        )];
        assert_eq!(extract_ops(&entries).len(), 2);
    }

    #[test]
    fn test_extract_ops_opencode_snake_case_path_fallback() {
        // OpenCode may use snake_case in some payload variants; we
        // tolerate it via the `file_path` key.
        let entries = vec![tool_use(
            "write",
            json!({"file_path": "/a/b.rs", "content": "hi"}),
        )];
        assert_eq!(extract_ops(&entries).len(), 1);
        assert_eq!(extract_ops(&entries)[0].path(), "/a/b.rs");
    }

    #[test]
    fn test_extract_ops_pi_write() {
        let entries = vec![tool_use(
            "write",
            json!({"path": "/a/b.rs", "content": "fn main() {}"}),
        )];
        assert_eq!(
            extract_ops(&entries),
            vec![FileOp::Write {
                path: "/a/b.rs".into(),
                content: "fn main() {}".into(),
            }]
        );
    }

    #[test]
    fn test_extract_ops_pi_edit() {
        let entries = vec![tool_use(
            "edit",
            json!({
                "path": "/a/b.rs",
                "old_string": "foo",
                "new_string": "bar",
            }),
        )];
        assert_eq!(
            extract_ops(&entries),
            vec![FileOp::Edit {
                path: "/a/b.rs".into(),
                old: "foo".into(),
                new: "bar".into(),
                replace_all: false,
            }]
        );
    }

    // ---- extract_ops: skip conditions ---------------------------------

    #[test]
    fn test_extract_ops_skips_read_and_bash() {
        let entries = vec![
            tool_use("Read", json!({"file_path": "/a/b.rs"})),
            tool_use("Bash", json!({"command": "ls"})),
        ];
        assert!(extract_ops(&entries).is_empty());
    }

    #[test]
    fn test_extract_ops_skips_text_entries() {
        let entries = vec![text_entry("hello, I will edit src/main.rs")];
        assert!(extract_ops(&entries).is_empty());
    }

    #[test]
    fn test_extract_ops_skips_missing_path() {
        let entries = vec![tool_use("Write", json!({"content": "hi"}))];
        assert!(extract_ops(&entries).is_empty());
    }

    #[test]
    fn test_extract_ops_skips_missing_content_and_no_edit_keys() {
        // Write tool name but neither content nor old/new — skipped.
        let entries = vec![tool_use("Write", json!({"file_path": "/a/b.rs"}))];
        assert!(extract_ops(&entries).is_empty());
    }

    #[test]
    fn test_extract_ops_empty_edits_array_falls_back_to_content() {
        // If `edits` is present but empty, treat as plain Write when
        // `content` is also present.
        let entries = vec![tool_use(
            "Write",
            json!({"file_path": "/a/b.rs", "content": "hi", "edits": []}),
        )];
        assert_eq!(
            extract_ops(&entries),
            vec![FileOp::Write {
                path: "/a/b.rs".into(),
                content: "hi".into(),
            }]
        );
    }

    #[test]
    fn test_extract_ops_replace_all_defaults_to_false() {
        let entries = vec![tool_use(
            "Edit",
            json!({"file_path": "/a/b.rs", "old_string": "x", "new_string": "y"}),
        )];
        if let FileOp::Edit { replace_all, .. } = &extract_ops(&entries)[0] {
            assert!(!replace_all);
        } else {
            panic!("expected edit");
        }
    }

    #[test]
    fn test_extract_ops_preserves_chronological_order_across_tools() {
        let entries = vec![
            tool_use("Write", json!({"file_path": "/a", "content": "1"})),
            tool_use(
                "Edit",
                json!({
                    "file_path": "/a", "old_string": "1", "new_string": "2",
                }),
            ),
            tool_use("Write", json!({"file_path": "/b", "content": "x"})),
        ];
        let ops = extract_ops(&entries);
        assert_eq!(ops.len(), 3);
        assert!(matches!(ops[0], FileOp::Write { .. }));
        assert!(matches!(ops[1], FileOp::Edit { .. }));
        assert_eq!(ops[2].path(), "/b");
    }

    // ---- stat ---------------------------------------------------------

    #[test]
    fn test_stat_aggregates_per_path() {
        let ops = vec![
            FileOp::Write {
                path: "/a".into(),
                content: "1".into(),
            },
            FileOp::Edit {
                path: "/a".into(),
                old: "1".into(),
                new: "2".into(),
                replace_all: false,
            },
            FileOp::Edit {
                path: "/a".into(),
                old: "2".into(),
                new: "3".into(),
                replace_all: false,
            },
            FileOp::Write {
                path: "/b".into(),
                content: "x".into(),
            },
        ];
        let stat = stat(&ops);
        assert_eq!(
            stat,
            vec![
                EditedFile {
                    path: "/a".into(),
                    writes: 1,
                    edits: 2,
                    created: true,
                    // write "1" (1 ins) + edit 1→2 (1 ins, 1 del) +
                    // edit 2→3 (1 ins, 1 del) = 3 ins, 2 del.
                    insertions: 3,
                    deletions: 2,
                },
                EditedFile {
                    path: "/b".into(),
                    writes: 1,
                    edits: 0,
                    created: true,
                    insertions: 1,
                    deletions: 0,
                },
            ]
        );
    }

    #[test]
    fn test_stat_first_op_edit_means_not_created() {
        // Edit-only history: the file existed BEFORE the session and
        // was just modified — `created` MUST be false.
        let ops = vec![
            FileOp::Edit {
                path: "/a".into(),
                old: "x".into(),
                new: "y".into(),
                replace_all: false,
            },
            FileOp::Edit {
                path: "/a".into(),
                old: "y".into(),
                new: "z".into(),
                replace_all: false,
            },
        ];
        let stat = stat(&ops);
        assert_eq!(stat.len(), 1);
        assert!(!stat[0].created);
        assert_eq!(stat[0].edits, 2);
        assert_eq!(stat[0].writes, 0);
    }

    #[test]
    fn test_stat_preserves_first_seen_order() {
        let ops = vec![
            FileOp::Write {
                path: "/b".into(),
                content: "x".into(),
            },
            FileOp::Write {
                path: "/a".into(),
                content: "y".into(),
            },
            FileOp::Edit {
                path: "/b".into(),
                old: "x".into(),
                new: "z".into(),
                replace_all: false,
            },
        ];
        let stat = stat(&ops);
        assert_eq!(stat[0].path, "/b");
        assert_eq!(stat[1].path, "/a");
    }

    #[test]
    fn test_stat_empty_input() {
        assert!(stat(&[]).is_empty());
    }

    // ---- count_lines ---------------------------------------------------

    #[test]
    fn test_count_lines_empty() {
        assert_eq!(count_lines(""), 0);
    }

    #[test]
    fn test_count_lines_single_no_trailing_newline() {
        assert_eq!(count_lines("a"), 1);
    }

    #[test]
    fn test_count_lines_single_with_trailing_newline() {
        assert_eq!(count_lines("a\n"), 1);
    }

    #[test]
    fn test_count_lines_two_lines_no_trailing_newline() {
        assert_eq!(count_lines("a\nb"), 2);
    }

    #[test]
    fn test_count_lines_two_lines_with_trailing_newline() {
        assert_eq!(count_lines("a\nb\n"), 2);
    }

    #[test]
    fn test_count_lines_bare_newline() {
        assert_eq!(count_lines("\n"), 1);
    }

    #[test]
    fn test_count_lines_two_bare_newlines() {
        assert_eq!(count_lines("\n\n"), 2);
    }

    // ---- stat: insertions / deletions math -----------------------------

    #[test]
    fn test_stat_insertions_pure_write_multiline() {
        // A pure write counts its full content as insertions, zero
        // deletions (no prior version to diff against).
        let ops = vec![FileOp::Write {
            path: "/a".into(),
            content: "line1\nline2\nline3\n".into(),
        }];
        let stat = stat(&ops);
        assert_eq!(stat[0].insertions, 3);
        assert_eq!(stat[0].deletions, 0);
    }

    #[test]
    fn test_stat_insertions_write_then_edit_chain() {
        // write "a\nb\n" (2 ins) + edit "b" → "c\nd" (2 ins, 1 del).
        let ops = vec![
            FileOp::Write {
                path: "/a".into(),
                content: "a\nb\n".into(),
            },
            FileOp::Edit {
                path: "/a".into(),
                old: "b".into(),
                new: "c\nd".into(),
                replace_all: false,
            },
        ];
        let stat = stat(&ops);
        assert_eq!(stat[0].insertions, 4);
        assert_eq!(stat[0].deletions, 1);
    }

    #[test]
    fn test_stat_insertions_edit_only_multiline_blocks() {
        // Pure edit-only history: insertions and deletions reflect the
        // old/new blocks of each edit, summed.
        let ops = vec![
            FileOp::Edit {
                path: "/a".into(),
                old: "old1\nold2".into(),
                new: "new1\nnew2\nnew3".into(),
                replace_all: false,
            },
            FileOp::Edit {
                path: "/a".into(),
                old: "x".into(),
                new: "".into(),
                replace_all: false,
            },
        ];
        let stat = stat(&ops);
        // Edit 1: del 2, ins 3.  Edit 2: del 1, ins 0.
        assert_eq!(stat[0].insertions, 3);
        assert_eq!(stat[0].deletions, 3);
        assert!(!stat[0].created);
    }

    #[test]
    fn test_stat_insertions_replace_all_counts_block_once() {
        // replace_all=true with a 2-line old / 1-line new: counted ONCE
        // (we don't know occurrence count without the source file).
        let ops = vec![FileOp::Edit {
            path: "/a".into(),
            old: "foo\nfoo".into(),
            new: "bar".into(),
            replace_all: true,
        }];
        let stat = stat(&ops);
        assert_eq!(stat[0].insertions, 1);
        assert_eq!(stat[0].deletions, 2);
    }
}

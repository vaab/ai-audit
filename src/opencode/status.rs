use anyhow::{anyhow, Result};
use rusqlite::Connection;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StaticStatus {
    Completed,
    UserPending,
    AssistantEmpty,
    AssistantPartial,
    AssistantToolStuck,
}

impl StaticStatus {
    pub fn is_resumable(&self) -> bool {
        matches!(
            self,
            Self::UserPending
                | Self::AssistantEmpty
                | Self::AssistantPartial
                | Self::AssistantToolStuck
        )
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::UserPending => "user-pending",
            Self::AssistantEmpty => "assistant-empty",
            Self::AssistantPartial => "assistant-partial",
            Self::AssistantToolStuck => "assistant-tool-stuck",
        }
    }

    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "completed" => Ok(Self::Completed),
            "user-pending" => Ok(Self::UserPending),
            "assistant-empty" => Ok(Self::AssistantEmpty),
            "assistant-partial" => Ok(Self::AssistantPartial),
            "assistant-tool-stuck" => Ok(Self::AssistantToolStuck),
            _ => Err(anyhow!(
                "invalid status; valid static values: completed, user-pending, assistant-empty, assistant-partial, assistant-tool-stuck"
            )),
        }
    }

    pub fn resumable_set() -> Vec<Self> {
        vec![
            Self::UserPending,
            Self::AssistantEmpty,
            Self::AssistantPartial,
            Self::AssistantToolStuck,
        ]
    }

    pub fn all() -> Vec<Self> {
        vec![
            Self::Completed,
            Self::UserPending,
            Self::AssistantEmpty,
            Self::AssistantPartial,
            Self::AssistantToolStuck,
        ]
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastMessageMeta {
    pub session_id: String,
    pub msg_id: String,
    pub last_msg_ts: i64,
    pub session_updated_ts: i64,
    pub last_role: String,
    pub last_completed: bool,
    pub parts_total: i64,
    pub stuck_tools: i64,
}

pub fn fetch_last_message_meta(
    conn: &Connection,
    session_ids: &[String],
) -> Result<HashMap<String, LastMessageMeta>> {
    let filter = if session_ids.is_empty() {
        String::new()
    } else {
        format!(
            "WHERE m.session_id IN ({})",
            std::iter::repeat_n("?", session_ids.len())
                .collect::<Vec<_>>()
                .join(",")
        )
    };
    let sql = format!(
        "WITH last_msg AS (
            SELECT m.session_id, m.id AS msg_id, m.time_created, m.data,
                   ROW_NUMBER() OVER (
                     PARTITION BY m.session_id
                     ORDER BY m.time_created DESC, m.id DESC
                   ) AS rn
            FROM message m
            {filter}
         )
         SELECT s.id, s.time_updated,
                last_msg.msg_id, last_msg.time_created AS last_msg_ts,
                json_extract(last_msg.data,'$.role') AS last_role,
                json_extract(last_msg.data,'$.time.completed') AS last_completed,
                (SELECT COUNT(*) FROM part p WHERE p.message_id = last_msg.msg_id) AS parts_total,
                (SELECT COUNT(*) FROM part p WHERE p.message_id = last_msg.msg_id
                   AND json_extract(p.data,'$.type')='tool'
                   AND json_extract(p.data,'$.state.status') IN ('running','pending')) AS stuck_tools
         FROM session s
         JOIN last_msg ON last_msg.session_id = s.id AND last_msg.rn = 1"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(session_ids.iter()), |row| {
        let last_completed = row.get::<_, Option<i64>>(5)?.is_some();
        Ok(LastMessageMeta {
            session_id: row.get(0)?,
            session_updated_ts: row.get::<_, i64>(1)? / 1000,
            msg_id: row.get(2)?,
            last_msg_ts: row.get::<_, i64>(3)? / 1000,
            last_role: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
            last_completed,
            parts_total: row.get(6)?,
            stuck_tools: row.get(7)?,
        })
    })?;

    Ok(rows
        .filter_map(|row| row.ok())
        .map(|meta| (meta.session_id.clone(), meta))
        .collect())
}

pub fn classify_static(meta: &LastMessageMeta) -> StaticStatus {
    if meta.last_role == "assistant" && meta.last_completed {
        return StaticStatus::Completed;
    }
    if meta.last_role == "user" {
        return StaticStatus::UserPending;
    }
    if meta.parts_total == 0 {
        return StaticStatus::AssistantEmpty;
    }
    if meta.stuck_tools > 0 {
        return StaticStatus::AssistantToolStuck;
    }
    StaticStatus::AssistantPartial
}

/// Resume payload for a clean-resume nudge: the existing user message
/// ID (revert cutoff) and the concatenated text of all its text parts
/// (replayed verbatim via `prompt_async`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumePayload {
    pub user_msg_id: String,
    pub text: String,
}

/// Look up the last user message of a session and concatenate its
/// text-part contents.
///
/// Returns `Ok(None)` if the session has no user messages.
///
/// Used by the nudge command for `user-pending` and `assistant-empty`
/// shapes: we revert at the user message ID, then re-fire it with the
/// same text. This is the same workflow the TUI's "revert + edit"
/// performs.
pub fn fetch_last_user_message(
    conn: &Connection,
    session_id: &str,
) -> Result<Option<ResumePayload>> {
    let mut stmt = conn.prepare(
        "SELECT m.id
         FROM message m
         WHERE m.session_id = ?
           AND json_extract(m.data,'$.role') = 'user'
         ORDER BY m.time_created DESC, m.id DESC
         LIMIT 1",
    )?;
    let user_msg_id: Option<String> = stmt
        .query_row([session_id], |row| row.get::<_, String>(0))
        .map(Some)
        .or_else(|err| match err {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })?;
    let Some(user_msg_id) = user_msg_id else {
        return Ok(None);
    };

    let mut stmt = conn.prepare(
        "SELECT json_extract(p.data,'$.text')
         FROM part p
         WHERE p.message_id = ?
           AND json_extract(p.data,'$.type') = 'text'
         ORDER BY p.time_created ASC, p.id ASC",
    )?;
    let text: String = stmt
        .query_map([&user_msg_id], |row| row.get::<_, Option<String>>(0))?
        .filter_map(|row| row.ok().flatten())
        .collect::<Vec<_>>()
        .join("\n");

    Ok(Some(ResumePayload { user_msg_id, text }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn setup_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE session (
                id TEXT PRIMARY KEY,
                parent_id TEXT,
                directory TEXT,
                title TEXT,
                time_created INTEGER NOT NULL,
                time_updated INTEGER NOT NULL
            );
            CREATE TABLE message (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                time_created INTEGER NOT NULL,
                data TEXT NOT NULL
            );
            CREATE TABLE part (
                id TEXT PRIMARY KEY,
                message_id TEXT NOT NULL,
                time_created INTEGER NOT NULL,
                data TEXT NOT NULL
            );",
        )
        .unwrap();
        conn
    }

    fn insert_session(
        conn: &Connection,
        session_id: &str,
        msg_id: &str,
        role: &str,
        completed: bool,
    ) {
        conn.execute(
            "INSERT INTO session (id, parent_id, directory, title, time_created, time_updated) VALUES (?, NULL, '', '', 1000, 2000)",
            [session_id],
        )
        .unwrap();
        let completed_json = if completed { "123" } else { "null" };
        conn.execute(
            "INSERT INTO message (id, session_id, time_created, data) VALUES (?, ?, 1500, ?)",
            rusqlite::params![
                msg_id,
                session_id,
                format!(
                    r#"{{"role":"{}","time":{{"completed":{}}}}}"#,
                    role, completed_json
                ),
            ],
        )
        .unwrap();
    }

    fn insert_part(conn: &Connection, id: &str, msg_id: &str, data: &str) {
        conn.execute(
            "INSERT INTO part (id, message_id, time_created, data) VALUES (?, ?, 1600, ?)",
            rusqlite::params![id, msg_id, data],
        )
        .unwrap();
    }

    #[test]
    fn classify_static_shapes() {
        let meta = |last_role: &str, last_completed: bool, parts_total: i64, stuck_tools: i64| {
            LastMessageMeta {
                session_id: "ses_1".to_string(),
                msg_id: "msg_1".to_string(),
                last_msg_ts: 1,
                session_updated_ts: 2,
                last_role: last_role.to_string(),
                last_completed,
                parts_total,
                stuck_tools,
            }
        };

        assert_eq!(
            classify_static(&meta("assistant", true, 1, 0)),
            StaticStatus::Completed
        );
        assert_eq!(
            classify_static(&meta("user", false, 0, 0)),
            StaticStatus::UserPending
        );
        assert_eq!(
            classify_static(&meta("assistant", false, 0, 0)),
            StaticStatus::AssistantEmpty
        );
        assert_eq!(
            classify_static(&meta("assistant", false, 1, 0)),
            StaticStatus::AssistantPartial
        );
        assert_eq!(
            classify_static(&meta("assistant", false, 1, 1)),
            StaticStatus::AssistantToolStuck
        );
    }

    #[test]
    fn resumable_set_is_exact() {
        assert_eq!(
            StaticStatus::resumable_set(),
            vec![
                StaticStatus::UserPending,
                StaticStatus::AssistantEmpty,
                StaticStatus::AssistantPartial,
                StaticStatus::AssistantToolStuck,
            ]
        );
    }

    #[test]
    fn fetch_last_message_meta_reads_fixture_shapes() {
        let conn = setup_db();
        insert_session(&conn, "ses_completed", "msg_completed", "assistant", true);
        insert_part(
            &conn,
            "prt_completed",
            "msg_completed",
            r#"{"type":"text","text":"done"}"#,
        );
        insert_session(&conn, "ses_user", "msg_user", "user", false);
        insert_session(&conn, "ses_empty", "msg_empty", "assistant", false);
        insert_session(&conn, "ses_partial", "msg_partial", "assistant", false);
        insert_part(
            &conn,
            "prt_partial",
            "msg_partial",
            r#"{"type":"text","text":"partial"}"#,
        );
        insert_session(&conn, "ses_tool", "msg_tool", "assistant", false);
        insert_part(
            &conn,
            "prt_tool",
            "msg_tool",
            r#"{"type":"tool","state":{"status":"running"}}"#,
        );

        let meta = fetch_last_message_meta(&conn, &[]).unwrap();

        assert_eq!(
            classify_static(meta.get("ses_completed").unwrap()),
            StaticStatus::Completed
        );
        assert_eq!(
            classify_static(meta.get("ses_user").unwrap()),
            StaticStatus::UserPending
        );
        assert_eq!(
            classify_static(meta.get("ses_empty").unwrap()),
            StaticStatus::AssistantEmpty
        );
        assert_eq!(
            classify_static(meta.get("ses_partial").unwrap()),
            StaticStatus::AssistantPartial
        );
        assert_eq!(
            classify_static(meta.get("ses_tool").unwrap()),
            StaticStatus::AssistantToolStuck
        );
    }

    /// Insert a message with a custom timestamp (the helper above hardcodes 1500).
    fn insert_message_at(
        conn: &Connection,
        msg_id: &str,
        session_id: &str,
        role: &str,
        time_created: i64,
    ) {
        conn.execute(
            "INSERT INTO message (id, session_id, time_created, data) VALUES (?, ?, ?, ?)",
            rusqlite::params![
                msg_id,
                session_id,
                time_created,
                format!(r#"{{"role":"{role}","time":{{"completed":null}}}}"#),
            ],
        )
        .unwrap();
    }

    fn insert_part_at(conn: &Connection, id: &str, msg_id: &str, data: &str, time_created: i64) {
        conn.execute(
            "INSERT INTO part (id, message_id, time_created, data) VALUES (?, ?, ?, ?)",
            rusqlite::params![id, msg_id, time_created, data],
        )
        .unwrap();
    }

    #[test]
    fn fetch_last_user_message_returns_text_for_user_pending_session() {
        let conn = setup_db();
        // user-pending shape: only one user message, no assistant follow-up.
        conn.execute(
            "INSERT INTO session (id, parent_id, directory, title, time_created, time_updated) VALUES ('ses_1', NULL, '', '', 1000, 2000)",
            [],
        ).unwrap();
        insert_message_at(&conn, "msg_user", "ses_1", "user", 1500);
        insert_part_at(
            &conn,
            "prt_text",
            "msg_user",
            r#"{"type":"text","text":"do the thing"}"#,
            1600,
        );

        let payload = fetch_last_user_message(&conn, "ses_1").unwrap().unwrap();
        assert_eq!(payload.user_msg_id, "msg_user");
        assert_eq!(payload.text, "do the thing");
    }

    #[test]
    fn fetch_last_user_message_picks_most_recent_user_message() {
        let conn = setup_db();
        conn.execute(
            "INSERT INTO session (id, parent_id, directory, title, time_created, time_updated) VALUES ('ses_1', NULL, '', '', 1000, 2000)",
            [],
        ).unwrap();
        // Older user message — should be ignored.
        insert_message_at(&conn, "msg_user_old", "ses_1", "user", 1000);
        insert_part_at(
            &conn,
            "prt_old",
            "msg_user_old",
            r#"{"type":"text","text":"original request"}"#,
            1100,
        );
        // Newer user message (e.g. follow-up) — should be picked.
        insert_message_at(&conn, "msg_user_new", "ses_1", "user", 2000);
        insert_part_at(
            &conn,
            "prt_new",
            "msg_user_new",
            r#"{"type":"text","text":"follow up"}"#,
            2100,
        );
        // Empty assistant stub between/after — must not be picked.
        insert_message_at(&conn, "msg_assist", "ses_1", "assistant", 2500);

        let payload = fetch_last_user_message(&conn, "ses_1").unwrap().unwrap();
        assert_eq!(payload.user_msg_id, "msg_user_new");
        assert_eq!(payload.text, "follow up");
    }

    #[test]
    fn fetch_last_user_message_concatenates_multiple_text_parts() {
        let conn = setup_db();
        conn.execute(
            "INSERT INTO session (id, parent_id, directory, title, time_created, time_updated) VALUES ('ses_1', NULL, '', '', 1000, 2000)",
            [],
        ).unwrap();
        insert_message_at(&conn, "msg_user", "ses_1", "user", 1500);
        insert_part_at(
            &conn,
            "prt_a",
            "msg_user",
            r#"{"type":"text","text":"first line"}"#,
            1600,
        );
        insert_part_at(
            &conn,
            "prt_b",
            "msg_user",
            r#"{"type":"text","text":"second line"}"#,
            1700,
        );
        // Non-text parts must be ignored.
        insert_part_at(
            &conn,
            "prt_file",
            "msg_user",
            r#"{"type":"file","filename":"a.txt"}"#,
            1800,
        );

        let payload = fetch_last_user_message(&conn, "ses_1").unwrap().unwrap();
        assert_eq!(payload.text, "first line\nsecond line");
    }

    #[test]
    fn fetch_last_user_message_returns_none_when_no_user_message() {
        let conn = setup_db();
        conn.execute(
            "INSERT INTO session (id, parent_id, directory, title, time_created, time_updated) VALUES ('ses_1', NULL, '', '', 1000, 2000)",
            [],
        ).unwrap();
        // Only an assistant stub; no user message at all.
        insert_message_at(&conn, "msg_assist", "ses_1", "assistant", 1500);

        let payload = fetch_last_user_message(&conn, "ses_1").unwrap();
        assert!(payload.is_none());
    }

    #[test]
    fn fetch_last_user_message_handles_user_with_no_text_parts() {
        // Defensive: if a user message exists but has no text parts
        // (e.g. only attachments), we still return the message id with
        // empty text rather than dropping the session.
        let conn = setup_db();
        conn.execute(
            "INSERT INTO session (id, parent_id, directory, title, time_created, time_updated) VALUES ('ses_1', NULL, '', '', 1000, 2000)",
            [],
        ).unwrap();
        insert_message_at(&conn, "msg_user", "ses_1", "user", 1500);
        insert_part_at(
            &conn,
            "prt_file",
            "msg_user",
            r#"{"type":"file","filename":"a.txt"}"#,
            1600,
        );

        let payload = fetch_last_user_message(&conn, "ses_1").unwrap().unwrap();
        assert_eq!(payload.user_msg_id, "msg_user");
        assert_eq!(payload.text, "");
    }
}

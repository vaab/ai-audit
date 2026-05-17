//! Single-shot session metadata for `ai-audit session info`.
//!
//! Fast O(few queries) path against `opencode.db`.  Returns a
//! `SessionDetailInfo` blob covering everything the spec at
//! `doc/admin.org § ai-audit / session info / single-shot session metadata`
//! requires.  Live status (busy/idle) is fetched separately by the
//! caller via [`server_client::ServerClient::session_status`] — this
//! module only reads the DB.

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, TimeZone, Utc};
use rusqlite::Connection;

use crate::provider::Provider;

use super::db;
use super::status::{classify_static, LastMessageMeta, StaticStatus};

/// Provider-agnostic session info shape.
///
/// Lives at the crate root of `opencode/` (rather than `lib.rs`)
/// because the OpenCode path is the most field-rich and the other
/// providers populate a subset via their own `info::fetch_info`.
/// The `cli/action/session/info.rs` dispatcher converts between
/// per-provider variants and the unified shape it formats.
#[derive(Debug, Clone)]
pub struct SessionDetailInfo {
    pub session_id: String,
    pub provider: Provider,
    pub project_dir: Option<String>,
    pub title: Option<String>,
    pub started_at: Option<DateTime<Utc>>,
    pub last_updated_at: Option<DateTime<Utc>>,
    pub message_count: usize,
    pub tool_call_count: usize,
    /// OpenCode-only.  `None` for JSONL providers (Claude Code, pi).
    pub static_status: Option<StaticStatus>,
    /// `true` when the session's last assistant turn errored or
    /// was interrupted mid-stream.  Derived per provider from the
    /// last message/entry shape.
    pub aborted: bool,
    pub parent_session_id: Option<String>,
    /// OpenCode-only.  `None` for JSONL providers.
    pub agent: Option<String>,
    /// Model id as recorded on the last assistant message.
    pub model: Option<String>,
}

/// Fetch session info from the canonical OpenCode SQLite DB.
///
/// Errors if the session id does not exist.  Returns enough data to
/// drive both human and `--json` output without any further queries.
pub fn fetch_info(session_id: &str) -> Result<SessionDetailInfo> {
    let conn = db::open_db()?;
    fetch_info_from_conn(&conn, session_id)
}

/// Testable variant of [`fetch_info`].
pub fn fetch_info_from_conn(conn: &Connection, session_id: &str) -> Result<SessionDetailInfo> {
    let session = fetch_session_row(conn, session_id)?
        .ok_or_else(|| anyhow!("session not found: {}", session_id))?;
    let message_count = fetch_message_count(conn, session_id)?;
    let tool_call_count = fetch_tool_call_count(conn, session_id)?;
    let last_meta = fetch_last_message_meta_single(conn, session_id)?;
    let last_message_attrs = fetch_last_assistant_attrs(conn, session_id)?;

    let static_status = last_meta.as_ref().map(classify_static);
    let aborted = match (static_status, last_meta.as_ref()) {
        (Some(status), Some(meta)) => is_aborted(status, meta),
        _ => false,
    };

    let last_updated_at = if session.time_updated > 0 {
        Some(ms_to_dt(session.time_updated))
    } else {
        last_meta.as_ref().map(|m| ms_to_dt(m.last_msg_ts * 1000))
    };

    Ok(SessionDetailInfo {
        session_id: session.id,
        provider: Provider::OpenCode,
        project_dir: nullable_string(session.directory),
        title: nullable_string(session.title),
        started_at: Some(ms_to_dt(session.time_created)),
        last_updated_at,
        message_count,
        tool_call_count,
        static_status,
        aborted,
        parent_session_id: session.parent_id,
        agent: last_message_attrs.as_ref().and_then(|a| a.agent.clone()),
        model: last_message_attrs.and_then(|a| a.model),
    })
}

struct SessionRow {
    id: String,
    parent_id: Option<String>,
    directory: String,
    title: String,
    time_created: i64,
    time_updated: i64,
}

fn fetch_session_row(conn: &Connection, session_id: &str) -> Result<Option<SessionRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, parent_id, directory, title, time_created, time_updated \
         FROM session WHERE id = ?",
    )?;
    let result = stmt
        .query_row([session_id], |row| {
            Ok(SessionRow {
                id: row.get(0)?,
                parent_id: row.get(1)?,
                directory: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                title: row.get::<_, Option<String>>(3)?.unwrap_or_default(),
                time_created: row.get(4)?,
                time_updated: row.get(5)?,
            })
        })
        .map(Some)
        .or_else(|err| match err {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })?;
    Ok(result)
}

fn fetch_message_count(conn: &Connection, session_id: &str) -> Result<usize> {
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM message WHERE session_id = ?",
            [session_id],
            |row| row.get(0),
        )
        .context("counting messages")?;
    Ok(n.max(0) as usize)
}

fn fetch_tool_call_count(conn: &Connection, session_id: &str) -> Result<usize> {
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM part \
             WHERE session_id = ? AND json_extract(data,'$.type') = 'tool'",
            [session_id],
            |row| row.get(0),
        )
        .context("counting tool parts")?;
    Ok(n.max(0) as usize)
}

/// Single-session variant of [`super::status::fetch_last_message_meta`].
///
/// Returns `None` when the session has no messages at all.
fn fetch_last_message_meta_single(
    conn: &Connection,
    session_id: &str,
) -> Result<Option<LastMessageMeta>> {
    let sql = "WITH last_msg AS (
            SELECT m.session_id, m.id AS msg_id, m.time_created, m.data,
                   ROW_NUMBER() OVER (
                     PARTITION BY m.session_id
                     ORDER BY m.time_created DESC, m.id DESC
                   ) AS rn
            FROM message m
            WHERE m.session_id = ?
         )
         SELECT s.id, s.time_updated,
                last_msg.msg_id, last_msg.time_created AS last_msg_ts,
                json_extract(last_msg.data,'$.role') AS last_role,
                json_extract(last_msg.data,'$.time.completed') AS last_completed,
                json_extract(last_msg.data,'$.error') AS last_error,
                (SELECT COUNT(*) FROM part p WHERE p.message_id = last_msg.msg_id) AS parts_total,
                (SELECT COUNT(*) FROM part p WHERE p.message_id = last_msg.msg_id
                   AND json_extract(p.data,'$.type')='tool'
                   AND (
                     json_extract(p.data,'$.state.status') IN ('running','pending')
                     OR (
                       json_extract(p.data,'$.state.status')='error'
                       AND json_extract(p.data,'$.state.metadata.interrupted')=1
                     )
                   )) AS stuck_tools
         FROM session s
         JOIN last_msg ON last_msg.session_id = s.id AND last_msg.rn = 1";
    let mut stmt = conn.prepare(sql)?;
    let result = stmt
        .query_row([session_id], |row| {
            let last_completed = row.get::<_, Option<i64>>(5)?.is_some();
            let assistant_errored = !matches!(row.get_ref(6)?, rusqlite::types::ValueRef::Null);
            Ok(LastMessageMeta {
                session_id: row.get(0)?,
                session_updated_ts: row.get::<_, i64>(1)? / 1000,
                msg_id: row.get(2)?,
                last_msg_ts: row.get::<_, i64>(3)? / 1000,
                last_role: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
                last_completed,
                parts_total: row.get(7)?,
                stuck_tools: row.get(8)?,
                assistant_errored,
            })
        })
        .map(Some)
        .or_else(|err| match err {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })?;
    Ok(result)
}

struct LastAssistantAttrs {
    agent: Option<String>,
    model: Option<String>,
}

/// Pull `agent` and a printable `provider/model` string from the most
/// recent assistant message.  Returns `None` if the session has no
/// assistant message.
///
/// The printable model is `"{providerID}/{modelID}"` when both are
/// recorded, falling back to whichever single field is present.
fn fetch_last_assistant_attrs(
    conn: &Connection,
    session_id: &str,
) -> Result<Option<LastAssistantAttrs>> {
    // Assistant messages in opencode use FLAT `modelID` / `providerID`
    // on the message itself; the nested `model.{providerID,modelID}`
    // shape lives on USER messages (the user-selected model at send
    // time, replayed by the resumed assistant turn).  We read flat
    // first and fall back to nested for older shapes.
    let mut stmt = conn.prepare(
        "SELECT json_extract(m.data,'$.agent') AS agent,
                json_extract(m.data,'$.providerID') AS provider_id_flat,
                json_extract(m.data,'$.modelID') AS model_id_flat,
                json_extract(m.data,'$.model.providerID') AS provider_id_nested,
                json_extract(m.data,'$.model.modelID') AS model_id_nested
         FROM message m
         WHERE m.session_id = ?
           AND json_extract(m.data,'$.role') = 'assistant'
         ORDER BY m.time_created DESC, m.id DESC
         LIMIT 1",
    )?;
    let result = stmt
        .query_row([session_id], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
            ))
        })
        .map(Some)
        .or_else(|err| match err {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })?;
    let Some((agent, pid_flat, mid_flat, pid_nested, mid_nested)) = result else {
        return Ok(None);
    };
    let provider_id = pid_flat.or(pid_nested);
    let model_id = mid_flat.or(mid_nested);
    let model = match (provider_id, model_id) {
        (Some(p), Some(m)) => Some(format!("{}/{}", p, m)),
        (Some(p), None) => Some(p),
        (None, Some(m)) => Some(m),
        (None, None) => None,
    };
    Ok(Some(LastAssistantAttrs { agent, model }))
}

/// Detect "aborted" sessions: the last assistant turn either crashed
/// (`$.error IS NOT NULL`), is stuck mid-tool, or never produced any
/// content despite being marked completed.
///
/// User-pending shapes are NOT aborted — the user just hasn't been
/// answered yet.  A completed turn with no error and no stuck tools
/// is also not aborted.
fn is_aborted(status: StaticStatus, meta: &LastMessageMeta) -> bool {
    match status {
        StaticStatus::Completed | StaticStatus::UserPending => false,
        StaticStatus::AssistantEmpty
        | StaticStatus::AssistantPartial
        | StaticStatus::AssistantToolStuck => {
            meta.assistant_errored
                || meta.stuck_tools > 0
                || (status == StaticStatus::AssistantEmpty && !meta.last_completed)
        }
    }
}

fn ms_to_dt(ms: i64) -> DateTime<Utc> {
    Utc.timestamp_millis_opt(ms)
        .single()
        .unwrap_or_else(Utc::now)
}

fn nullable_string(s: String) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn setup_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        db::create_schema(&conn).unwrap();
        conn
    }

    fn insert_session(
        conn: &Connection,
        id: &str,
        directory: &str,
        title: &str,
        created: i64,
        updated: i64,
    ) {
        conn.execute(
            "INSERT INTO session (id, project_id, parent_id, directory, title, time_created, time_updated) \
             VALUES (?1, ?2, NULL, ?3, ?4, ?5, ?6)",
            rusqlite::params![id, "proj_1", directory, title, created, updated],
        )
        .unwrap();
    }

    fn insert_message(conn: &Connection, id: &str, session_id: &str, ts_ms: i64, data: &str) {
        conn.execute(
            "INSERT INTO message (id, session_id, time_created, time_updated, data) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![id, session_id, ts_ms, ts_ms, data],
        )
        .unwrap();
    }

    fn insert_part(
        conn: &Connection,
        id: &str,
        msg_id: &str,
        session_id: &str,
        ts_ms: i64,
        data: &str,
    ) {
        conn.execute(
            "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![id, msg_id, session_id, ts_ms, ts_ms, data],
        )
        .unwrap();
    }

    #[test]
    fn fetch_info_returns_basic_metadata() {
        let conn = setup_db();
        insert_session(
            &conn,
            "ses_001",
            "/home/u/proj",
            "My session",
            1_700_000_000_000,
            1_700_000_010_000,
        );
        insert_message(
            &conn,
            "msg_u",
            "ses_001",
            1_700_000_001_000,
            r#"{"role":"user","time":{"created":1700000001000}}"#,
        );
        insert_message(
            &conn,
            "msg_a",
            "ses_001",
            1_700_000_002_000,
            r#"{"role":"assistant","time":{"created":1700000002000,"completed":1700000003000},"agent":"build","modelID":"claude-opus-4-7","providerID":"anthropic"}"#,
        );
        insert_part(
            &conn,
            "prt_text",
            "msg_a",
            "ses_001",
            1_700_000_002_500,
            r#"{"type":"text","text":"hi"}"#,
        );
        insert_part(
            &conn,
            "prt_tool",
            "msg_a",
            "ses_001",
            1_700_000_002_600,
            r#"{"type":"tool","tool":"bash","state":{"status":"completed"}}"#,
        );

        let info = fetch_info_from_conn(&conn, "ses_001").unwrap();
        assert_eq!(info.session_id, "ses_001");
        assert_eq!(info.provider, Provider::OpenCode);
        assert_eq!(info.project_dir.as_deref(), Some("/home/u/proj"));
        assert_eq!(info.title.as_deref(), Some("My session"));
        assert_eq!(info.message_count, 2);
        assert_eq!(info.tool_call_count, 1);
        assert_eq!(info.static_status, Some(StaticStatus::Completed));
        assert!(!info.aborted);
        assert_eq!(info.agent.as_deref(), Some("build"));
        assert_eq!(info.model.as_deref(), Some("anthropic/claude-opus-4-7"));
    }

    #[test]
    fn fetch_info_unknown_session_errors() {
        let conn = setup_db();
        let err = fetch_info_from_conn(&conn, "ses_missing").unwrap_err();
        assert!(err.to_string().contains("session not found"));
        assert!(err.to_string().contains("ses_missing"));
    }

    #[test]
    fn fetch_info_marks_errored_completed_as_aborted() {
        let conn = setup_db();
        insert_session(&conn, "ses_err", "/p", "t", 1_000_000, 2_000_000);
        insert_message(
            &conn,
            "msg_a",
            "ses_err",
            1_500_000,
            r#"{"role":"assistant","time":{"completed":1800000},"error":{"name":"APIError"}}"#,
        );
        insert_part(
            &conn,
            "prt_step",
            "msg_a",
            "ses_err",
            1_600_000,
            r#"{"type":"step-start"}"#,
        );
        insert_part(
            &conn,
            "prt_text",
            "msg_a",
            "ses_err",
            1_700_000,
            r#"{"type":"text","text":"partial"}"#,
        );

        let info = fetch_info_from_conn(&conn, "ses_err").unwrap();
        assert_eq!(info.static_status, Some(StaticStatus::AssistantPartial));
        assert!(info.aborted);
    }

    #[test]
    fn fetch_info_user_pending_is_not_aborted() {
        let conn = setup_db();
        insert_session(&conn, "ses_u", "/p", "t", 1_000_000, 2_000_000);
        insert_message(
            &conn,
            "msg_u",
            "ses_u",
            1_500_000,
            r#"{"role":"user","time":{"created":1500000}}"#,
        );

        let info = fetch_info_from_conn(&conn, "ses_u").unwrap();
        assert_eq!(info.static_status, Some(StaticStatus::UserPending));
        assert!(!info.aborted);
    }

    #[test]
    fn fetch_info_empty_directory_renders_as_none() {
        let conn = setup_db();
        insert_session(&conn, "ses_empty", "", "", 1_000_000, 2_000_000);

        let info = fetch_info_from_conn(&conn, "ses_empty").unwrap();
        assert!(info.project_dir.is_none());
        assert!(info.title.is_none());
    }

    #[test]
    fn fetch_info_carries_parent_id() {
        let conn = setup_db();
        conn.execute(
            "INSERT INTO session (id, project_id, parent_id, directory, title, time_created, time_updated) \
             VALUES ('ses_child', 'p', 'ses_parent', '/p', '', 1, 2)",
            [],
        )
        .unwrap();

        let info = fetch_info_from_conn(&conn, "ses_child").unwrap();
        assert_eq!(info.parent_session_id.as_deref(), Some("ses_parent"));
    }

    #[test]
    fn fetch_info_tool_stuck_is_aborted() {
        let conn = setup_db();
        insert_session(&conn, "ses_t", "/p", "t", 1_000_000, 2_000_000);
        insert_message(
            &conn,
            "msg_a",
            "ses_t",
            1_500_000,
            r#"{"role":"assistant","time":{"created":1500000}}"#,
        );
        insert_part(
            &conn,
            "prt_tool",
            "msg_a",
            "ses_t",
            1_600_000,
            r#"{"type":"tool","state":{"status":"running"}}"#,
        );

        let info = fetch_info_from_conn(&conn, "ses_t").unwrap();
        assert_eq!(info.static_status, Some(StaticStatus::AssistantToolStuck));
        assert!(info.aborted);
    }
}

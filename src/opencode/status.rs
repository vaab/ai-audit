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
    /// Count of tool parts on the last message that are *not* in a
    /// healthy terminal state.  Two shapes are counted:
    ///
    /// 1. `state.status` is `running` or `pending` — the canonical
    ///    "in-flight when opencode died" shape.
    /// 2. `state.status='error'` with `state.metadata.interrupted=true`
    ///    — opencode's recorded shape for tools that were aborted
    ///    mid-execution (e.g. the host process tore down the tool
    ///    invocation while waiting on the model).  These look like
    ///    completed errors on disk but are functionally the same
    ///    "stuck tool" as (1) from the LLM's perspective.
    pub stuck_tools: i64,
    /// `$.error IS NOT NULL` on the last message's JSON `data` blob.
    /// Opencode writes a non-null `error` (e.g. `APIError` with
    /// `STREAM_IDLE_TIMEOUT`) when the assistant turn died mid-stream
    /// — even though it ALSO stamps `time.completed`.  Without this
    /// flag, `classify_static` would label such sessions `Completed`
    /// and `is_resumable()` would refuse to surface them, which is
    /// the wrong answer: the turn is genuinely incomplete and needs
    /// a `continue` nudge.
    pub assistant_errored: bool,
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
    // `stuck_tools` counts BOTH genuinely in-flight tool parts and
    // tool parts that opencode marked as errored+interrupted (its
    // recorded shape for aborted tool calls — see `LastMessageMeta`
    // docs for the rationale).
    //
    // `assistant_errored` reflects `$.error IS NOT NULL` on the last
    // message's JSON.  Used by `classify_static` to override the
    // `time.completed` happy path when the turn actually died with
    // an error (e.g. `STREAM_IDLE_TIMEOUT`).
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
         JOIN last_msg ON last_msg.session_id = s.id AND last_msg.rn = 1"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(session_ids.iter()), |row| {
        let last_completed = row.get::<_, Option<i64>>(5)?.is_some();
        // `$.error` is JSON-typed (object).  We don't need its
        // contents — only its presence.  rusqlite surfaces this as
        // a `Value` we read by column index; any non-NULL value means
        // opencode recorded an error on this assistant turn.
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
    })?;

    Ok(rows
        .filter_map(|row| row.ok())
        .map(|meta| (meta.session_id.clone(), meta))
        .collect())
}

pub fn classify_static(meta: &LastMessageMeta) -> StaticStatus {
    // A turn is `Completed` only when opencode stamped
    // `time.completed` AND did NOT record an error AND has no
    // stuck/interrupted tool parts.  The latter two are the
    // smoking-gun signals that a "completed" turn actually died
    // mid-stream (e.g. `STREAM_IDLE_TIMEOUT` while a tool was
    // executing — opencode then marks the tool as
    // `state.status='error'` with `metadata.interrupted=true`, and
    // ALSO stamps `time.completed` on the assistant message).  Without
    // these guards the session would slip into `Completed` and be
    // excluded from the resumable set, despite being a textbook
    // candidate for a `continue` nudge.
    if meta.last_role == "assistant"
        && meta.last_completed
        && !meta.assistant_errored
        && meta.stuck_tools == 0
    {
        return StaticStatus::Completed;
    }
    if meta.last_role == "user" {
        return StaticStatus::UserPending;
    }
    // `stuck_tools > 0` is checked BEFORE the errored/empty branches
    // so a turn that died mid-tool is still classified as
    // `AssistantToolStuck` (the narrowest, most actionable shape)
    // rather than the broader `AssistantPartial`.
    if meta.stuck_tools > 0 {
        return StaticStatus::AssistantToolStuck;
    }
    // `assistant_errored` overrides `parts_total == 0`.  An assistant
    // turn that recorded `$.error` (e.g. `STREAM_IDLE_TIMEOUT`) is
    // semantically "interrupted mid-stream", NOT "never started" — even
    // if the model crashed before emitting its first part.  Conflating
    // the two shapes (the pre-fix behavior) hid the diagnostic signal
    // and made `ses_1dc9500afffe3ChzA3rE9JxPom` indistinguishable from
    // a user-just-pressed-enter-and-walked-away session.  Both are
    // resumable, but only the errored one is genuinely "interrupted".
    if meta.assistant_errored {
        return StaticStatus::AssistantPartial;
    }
    if meta.parts_total == 0 {
        return StaticStatus::AssistantEmpty;
    }
    StaticStatus::AssistantPartial
}

/// Provider+model pair stored on an opencode user message.
///
/// We forward this verbatim in the nudge `prompt_async` body so the
/// resumed turn uses the same model that originally produced the
/// session.  Omitting it would let opencode fall back to the agent's
/// configured model (or the daemon default), which is wrong when the
/// user explicitly chose a model for the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeModel {
    pub provider_id: String,
    pub model_id: String,
}

/// (user_msg_id, agent, provider_id, model_id) returned by the SQL
/// query that fetches a user message's identity fields.  Aliased
/// here because the bare 4-tuple of Options triggers
/// clippy::type_complexity twice (once per call site).
type UserMessageRow = (String, Option<String>, Option<String>, Option<String>);

/// Resume payload for a nudge.
///
/// Used by both `CleanResume` and `ContinuePrompt`:
///
/// * `user_msg_id` — the revert cutoff for `CleanResume`.  Not used
///   by `ContinuePrompt` (no revert there), but the field is still
///   carried as the identity of the user-message-we-derived-from for
///   diagnostics.
/// * `text` — the verbatim user text to replay for `CleanResume`.
///   Empty/unused for `ContinuePrompt`.
/// * `agent` — the `agent` field stamped on that user message.  This
///   is the agent the session was originally driven by.  Forwarded
///   to opencode in `prompt_async` so the resumed turn doesn't fall
///   back to the daemon's default agent.
/// * `model` — the `model` (provider/id) stamped on that user
///   message.  Forwarded for the same reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumePayload {
    pub user_msg_id: String,
    pub text: String,
    pub agent: Option<String>,
    pub model: Option<ResumeModel>,
}

/// Look up the last user message of a session and extract everything
/// the nudge needs to faithfully replay (or continue from) it.
///
/// Returns `Ok(None)` if the session has no user messages.
///
/// Used by the nudge command for `user-pending` and `assistant-empty`
/// shapes — the `user_msg_id` and `text` drive the `revert + replay`
/// pair, while `agent` + `model` are forwarded in the new
/// `prompt_async` body so the resumed turn keeps the original
/// session's identity.
pub fn fetch_last_user_message(
    conn: &Connection,
    session_id: &str,
) -> Result<Option<ResumePayload>> {
    let mut stmt = conn.prepare(
        "SELECT m.id,
                json_extract(m.data,'$.agent') AS agent,
                json_extract(m.data,'$.model.providerID') AS provider_id,
                json_extract(m.data,'$.model.modelID') AS model_id
         FROM message m
         WHERE m.session_id = ?
           AND json_extract(m.data,'$.role') = 'user'
         ORDER BY m.time_created DESC, m.id DESC
         LIMIT 1",
    )?;
    let row: Option<UserMessageRow> = stmt
        .query_row([session_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })
        .map(Some)
        .or_else(|err| match err {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })?;
    let Some((user_msg_id, agent, provider_id, model_id)) = row else {
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

    let model = match (provider_id, model_id) {
        (Some(provider_id), Some(model_id)) => Some(ResumeModel {
            provider_id,
            model_id,
        }),
        _ => None,
    };

    Ok(Some(ResumePayload {
        user_msg_id,
        text,
        agent,
        model,
    }))
}

/// Like `fetch_last_user_message`, but for sessions in
/// `AssistantPartial`/`AssistantToolStuck` shapes: the most recent
/// message is the (broken) assistant turn, so the "user message we
/// want context from" is the one immediately PRECEDING that assistant
/// turn.
///
/// Returns `Ok(None)` if no preceding user message exists (which
/// would be an exotic shape — an assistant message with no prior
/// user message — but we guard for it).
///
/// Used by the nudge command for `assistant-partial` and
/// `assistant-tool-stuck` shapes: the nudge does NOT revert in those
/// cases (we keep the partial work) but DOES need the original
/// agent/model so the `continue` prompt runs under the same context.
pub fn fetch_user_message_before_last_assistant(
    conn: &Connection,
    session_id: &str,
) -> Result<Option<ResumePayload>> {
    // Find the time_created of the most recent assistant message in
    // this session — that's the broken turn the nudge will continue.
    let mut stmt = conn.prepare(
        "SELECT m.time_created, m.id
         FROM message m
         WHERE m.session_id = ?
           AND json_extract(m.data,'$.role') = 'assistant'
         ORDER BY m.time_created DESC, m.id DESC
         LIMIT 1",
    )?;
    let assistant: Option<(i64, String)> = stmt
        .query_row([session_id], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })
        .map(Some)
        .or_else(|err| match err {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })?;
    let Some((assistant_ts, _assistant_id)) = assistant else {
        return Ok(None);
    };

    // The user message we want is the most recent one STRICTLY before
    // the assistant message in time order.  (Equality on time_created
    // would be ambiguous, but in practice user messages are always
    // strictly older than the assistant message they triggered.)
    let mut stmt = conn.prepare(
        "SELECT m.id,
                json_extract(m.data,'$.agent') AS agent,
                json_extract(m.data,'$.model.providerID') AS provider_id,
                json_extract(m.data,'$.model.modelID') AS model_id
         FROM message m
         WHERE m.session_id = ?
           AND json_extract(m.data,'$.role') = 'user'
           AND m.time_created < ?
         ORDER BY m.time_created DESC, m.id DESC
         LIMIT 1",
    )?;
    let row: Option<UserMessageRow> = stmt
        .query_row(rusqlite::params![session_id, assistant_ts], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })
        .map(Some)
        .or_else(|err| match err {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })?;
    let Some((user_msg_id, agent, provider_id, model_id)) = row else {
        return Ok(None);
    };

    // Text is unused for ContinuePrompt but we still populate it for
    // diagnostics.
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

    let model = match (provider_id, model_id) {
        (Some(provider_id), Some(model_id)) => Some(ResumeModel {
            provider_id,
            model_id,
        }),
        _ => None,
    };

    Ok(Some(ResumePayload {
        user_msg_id,
        text,
        agent,
        model,
    }))
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
        let meta = |last_role: &str,
                    last_completed: bool,
                    parts_total: i64,
                    stuck_tools: i64,
                    assistant_errored: bool| {
            LastMessageMeta {
                session_id: "ses_1".to_string(),
                msg_id: "msg_1".to_string(),
                last_msg_ts: 1,
                session_updated_ts: 2,
                last_role: last_role.to_string(),
                last_completed,
                parts_total,
                stuck_tools,
                assistant_errored,
            }
        };

        // Healthy completion: time.completed set, no error, no stuck tools.
        assert_eq!(
            classify_static(&meta("assistant", true, 1, 0, false)),
            StaticStatus::Completed
        );
        assert_eq!(
            classify_static(&meta("user", false, 0, 0, false)),
            StaticStatus::UserPending
        );
        assert_eq!(
            classify_static(&meta("assistant", false, 0, 0, false)),
            StaticStatus::AssistantEmpty
        );
        assert_eq!(
            classify_static(&meta("assistant", false, 1, 0, false)),
            StaticStatus::AssistantPartial
        );
        assert_eq!(
            classify_static(&meta("assistant", false, 1, 1, false)),
            StaticStatus::AssistantToolStuck
        );
    }

    /// Regression for the real-world `ses_1df528d3effeeHt8GvXcpgeeZo`
    /// shape: opencode died mid-tool with `STREAM_IDLE_TIMEOUT`, so it
    /// recorded:
    ///
    /// * `time.completed`: set (truthy) — looks "done" naively
    /// * `$.error`: `{APIError, STREAM_IDLE_TIMEOUT}` — turn actually
    ///    died
    /// * one tool part with `state.status='error'` and
    ///   `state.metadata.interrupted=true` — tool was aborted
    ///
    /// Before the fix this landed in `Completed` (excluded from
    /// `is_resumable`).  With the assistant-errored + interrupted-tool
    /// guards the turn correctly classifies as `AssistantToolStuck`,
    /// which `nudge` resolves to a `ContinuePrompt` strategy.
    #[test]
    fn classify_static_errored_completed_with_interrupted_tool_is_tool_stuck() {
        let meta = LastMessageMeta {
            session_id: "ses_x".to_string(),
            msg_id: "msg_x".to_string(),
            last_msg_ts: 1,
            session_updated_ts: 2,
            last_role: "assistant".to_string(),
            last_completed: true,
            parts_total: 3,
            stuck_tools: 1,
            assistant_errored: true,
        };
        let status = classify_static(&meta);
        assert_eq!(status, StaticStatus::AssistantToolStuck);
        assert!(status.is_resumable());
    }

    /// Sibling regression: assistant turn died with an error but had
    /// no in-flight or interrupted tool parts (e.g. error fired during
    /// text streaming before any tool was invoked).  Must land in
    /// `AssistantPartial` (still resumable), NOT `Completed`.
    #[test]
    fn classify_static_errored_completed_without_tool_is_partial() {
        let meta = LastMessageMeta {
            session_id: "ses_x".to_string(),
            msg_id: "msg_x".to_string(),
            last_msg_ts: 1,
            session_updated_ts: 2,
            last_role: "assistant".to_string(),
            last_completed: true,
            parts_total: 2, // step-start + partial text
            stuck_tools: 0,
            assistant_errored: true,
        };
        let status = classify_static(&meta);
        assert_eq!(status, StaticStatus::AssistantPartial);
        assert!(status.is_resumable());
    }

    /// Regression for the real-world `ses_1dc9500afffe3ChzA3rE9JxPom`
    /// shape: assistant turn died with `STREAM_IDLE_TIMEOUT` *before*
    /// producing any parts, so opencode recorded:
    ///
    /// * `$.role`            : "assistant"
    /// * `$.time.completed`  : set (truthy timestamp — looks "done")
    /// * `$.error`           : `{APIError, STREAM_IDLE_TIMEOUT}`
    /// * zero parts on this message
    /// * zero stuck tools (no tools were ever invoked)
    ///
    /// Before the fix this landed in `AssistantEmpty`, which is the
    /// same bucket as "user message sent, assistant never replied at
    /// all".  Conflating those two shapes hides the diagnostic signal
    /// (`$.error`) that says *the turn started and crashed*.  It also
    /// routes the nudge down `CleanResume` (revert + replay), which is
    /// the right behavior here but loses the "this was interrupted"
    /// telemetry.
    ///
    /// After the fix, an errored assistant turn — regardless of
    /// `parts_total` — classifies as `AssistantPartial` so the
    /// diagnostic signal survives.  Resumability is unchanged
    /// (`AssistantEmpty` and `AssistantPartial` are both resumable);
    /// the nudge layer is responsible for picking the right strategy
    /// from the shape.
    #[test]
    fn classify_static_errored_completed_with_zero_parts_is_partial() {
        let meta = LastMessageMeta {
            session_id: "ses_1dc9500afffe3ChzA3rE9JxPom".to_string(),
            msg_id: "msg_e2379f816001UiuNaDH6r4wtm0".to_string(),
            last_msg_ts: 1778711656470,
            session_updated_ts: 1778711656605,
            last_role: "assistant".to_string(),
            last_completed: true,
            parts_total: 0, // turn died before producing any part
            stuck_tools: 0,
            assistant_errored: true, // $.error = APIError/STREAM_IDLE_TIMEOUT
        };
        let status = classify_static(&meta);
        assert_eq!(
            status,
            StaticStatus::AssistantPartial,
            "errored turn with zero parts MUST classify as AssistantPartial \
             (not AssistantEmpty); the `$.error` signal overrides `parts_total==0` \
             so the 'interrupted' shape is distinguishable from 'never started'"
        );
        assert!(status.is_resumable());
    }

    /// Guard against the reverse regression: an assistant turn that
    /// genuinely completed (time.completed set, no error, no stuck
    /// tools) must still classify as `Completed`.  This pins the
    /// happy path so a future change to the resumability heuristic
    /// can't silently turn every completed session into a nudge target.
    #[test]
    fn classify_static_genuine_completion_stays_completed() {
        let meta = LastMessageMeta {
            session_id: "ses_x".to_string(),
            msg_id: "msg_x".to_string(),
            last_msg_ts: 1,
            session_updated_ts: 2,
            last_role: "assistant".to_string(),
            last_completed: true,
            parts_total: 5,
            stuck_tools: 0,
            assistant_errored: false,
        };
        let status = classify_static(&meta);
        assert_eq!(status, StaticStatus::Completed);
        assert!(!status.is_resumable());
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

    /// End-to-end regression for the SQL → mapper → `classify_static`
    /// pipeline against the real-world `ses_1df528d3effeeHt8GvXcpgeeZo`
    /// shape (see `classify_static_errored_completed_with_interrupted_tool_is_tool_stuck`
    /// for the standalone classifier check).
    ///
    /// This test pins the on-disk byte shape:
    ///   * `$.time.completed` is set (truthy timestamp)
    ///   * `$.error` is a populated object
    ///   * one tool part has `state.status='error'` AND
    ///     `state.metadata.interrupted=true`
    /// and confirms `fetch_last_message_meta` extracts both
    /// `assistant_errored=true` and `stuck_tools>=1`, so
    /// `classify_static` yields `AssistantToolStuck` and the session
    /// surfaces in the resumable set.
    #[test]
    fn fetch_last_message_meta_detects_errored_completed_with_interrupted_tool() {
        let conn = setup_db();
        conn.execute(
            "INSERT INTO session (id, parent_id, directory, title, time_created, time_updated) VALUES ('ses_err', NULL, '', '', 1000, 2000)",
            [],
        )
        .unwrap();
        // Last assistant message: time.completed set AND $.error populated.
        // Mirrors opencode's recorded shape on STREAM_IDLE_TIMEOUT.
        conn.execute(
            "INSERT INTO message (id, session_id, time_created, data) VALUES ('msg_err', 'ses_err', 1500, ?)",
            [r#"{
                "role": "assistant",
                "time": {"created": 1000, "completed": 1800},
                "error": {
                    "name": "APIError",
                    "data": {
                        "message": "Stream idle timeout: no LLM events for 30s",
                        "isRetryable": true,
                        "metadata": {"code": "STREAM_IDLE_TIMEOUT"}
                    }
                }
            }"#],
        )
        .unwrap();
        // Three parts mirroring the on-disk shape: step-start, text, then
        // an interrupted tool (`state.status='error'` + `metadata.interrupted=true`).
        insert_part(&conn, "prt_step", "msg_err", r#"{"type":"step-start"}"#);
        insert_part(
            &conn,
            "prt_text",
            "msg_err",
            r#"{"type":"text","text":"Now log the duplicate-fire incident..."}"#,
        );
        insert_part(
            &conn,
            "prt_tool",
            "msg_err",
            r#"{"type":"tool","tool":"edit","callID":"toolu_x","state":{"status":"error","input":{},"raw":"","error":"Tool execution aborted","metadata":{"interrupted":true}}}"#,
        );

        let meta = fetch_last_message_meta(&conn, &[]).unwrap();
        let entry = meta
            .get("ses_err")
            .expect("session row should have been mapped");

        // Both signals must be picked up by the SQL/mapper.
        assert!(
            entry.assistant_errored,
            "assistant_errored must be true when $.error is populated: {entry:?}"
        );
        assert!(
            entry.last_completed,
            "last_completed reflects $.time.completed being set: {entry:?}"
        );
        assert_eq!(
            entry.stuck_tools, 1,
            "interrupted tool part should count toward stuck_tools: {entry:?}"
        );
        // And the classifier puts it into the resumable bucket.
        let status = classify_static(entry);
        assert_eq!(status, StaticStatus::AssistantToolStuck);
        assert!(status.is_resumable());
    }

    /// Sibling end-to-end: assistant turn errored but had no tool parts
    /// (e.g. died during text streaming).  Must extract
    /// `assistant_errored=true`, `stuck_tools=0`, and land in
    /// `AssistantPartial` — still resumable.
    #[test]
    fn fetch_last_message_meta_detects_errored_completed_without_tool() {
        let conn = setup_db();
        conn.execute(
            "INSERT INTO session (id, parent_id, directory, title, time_created, time_updated) VALUES ('ses_err', NULL, '', '', 1000, 2000)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message (id, session_id, time_created, data) VALUES ('msg_err', 'ses_err', 1500, ?)",
            [r#"{
                "role": "assistant",
                "time": {"created": 1000, "completed": 1800},
                "error": {"name": "APIError", "data": {"message": "boom"}}
            }"#],
        )
        .unwrap();
        insert_part(&conn, "prt_step", "msg_err", r#"{"type":"step-start"}"#);
        insert_part(
            &conn,
            "prt_text",
            "msg_err",
            r#"{"type":"text","text":"partial"}"#,
        );

        let meta = fetch_last_message_meta(&conn, &[]).unwrap();
        let entry = meta.get("ses_err").unwrap();
        assert!(entry.assistant_errored);
        assert_eq!(entry.stuck_tools, 0);
        let status = classify_static(entry);
        assert_eq!(status, StaticStatus::AssistantPartial);
        assert!(status.is_resumable());
    }

    /// End-to-end regression for the SQL → mapper → `classify_static`
    /// pipeline against the real-world `ses_1dc9500afffe3ChzA3rE9JxPom`
    /// shape: the assistant turn died with `STREAM_IDLE_TIMEOUT` BEFORE
    /// producing a single part.
    ///
    /// Pins the on-disk byte shape:
    ///   * `$.time.completed` is set (truthy timestamp)
    ///   * `$.error` is a populated object (`APIError` /
    ///     `STREAM_IDLE_TIMEOUT`)
    ///   * `parts_total = 0` — no parts attached to this message
    ///   * `stuck_tools = 0` — no tools were ever invoked
    ///
    /// Pre-fix, `assistant_errored=true` was dropped on the floor by
    /// `classify_static` once `parts_total==0` short-circuited to
    /// `AssistantEmpty`.  This test confirms the SQL still extracts
    /// `assistant_errored=true` AND the classifier honors it,
    /// producing `AssistantPartial` (errored mid-stream, distinguishable
    /// from "never started").
    #[test]
    fn fetch_last_message_meta_detects_errored_completed_with_zero_parts() {
        let conn = setup_db();
        conn.execute(
            "INSERT INTO session (id, parent_id, directory, title, time_created, time_updated) VALUES ('ses_err', NULL, '', '', 1000, 2000)",
            [],
        )
        .unwrap();
        // Last assistant message: time.completed set AND $.error populated,
        // but NO parts.  This is the exact shape recorded by opencode when
        // STREAM_IDLE_TIMEOUT fires before the model emits its first token.
        conn.execute(
            "INSERT INTO message (id, session_id, time_created, data) VALUES ('msg_err', 'ses_err', 1500, ?)",
            [r#"{
                "role": "assistant",
                "time": {"created": 1000, "completed": 1800},
                "error": {
                    "name": "APIError",
                    "data": {
                        "message": "Stream idle timeout: no LLM events for 30s",
                        "isRetryable": true,
                        "metadata": {"code": "STREAM_IDLE_TIMEOUT"}
                    }
                }
            }"#],
        )
        .unwrap();
        // Deliberately NO `insert_part` calls — this message has zero parts.

        let meta = fetch_last_message_meta(&conn, &[]).unwrap();
        let entry = meta
            .get("ses_err")
            .expect("session row should have been mapped");

        assert!(
            entry.assistant_errored,
            "assistant_errored must be true when $.error is populated: {entry:?}"
        );
        assert!(
            entry.last_completed,
            "last_completed reflects $.time.completed being set: {entry:?}"
        );
        assert_eq!(entry.parts_total, 0, "no parts attached: {entry:?}");
        assert_eq!(entry.stuck_tools, 0, "no tools invoked: {entry:?}");

        let status = classify_static(entry);
        assert_eq!(
            status,
            StaticStatus::AssistantPartial,
            "errored zero-parts turn MUST classify as AssistantPartial, \
             not AssistantEmpty (which would conflate it with 'never started')"
        );
        assert!(status.is_resumable());
    }

    /// Guard against the reverse regression at the SQL layer: a
    /// completed assistant message with no `$.error` and no
    /// interrupted/in-flight tool parts must still classify as
    /// `Completed`.  Pins the happy path so SQL widening can't
    /// accidentally turn every session into a nudge candidate.
    #[test]
    fn fetch_last_message_meta_keeps_genuine_completion_completed() {
        let conn = setup_db();
        insert_session(&conn, "ses_done", "msg_done", "assistant", true);
        insert_part(
            &conn,
            "prt_done_text",
            "msg_done",
            r#"{"type":"text","text":"all done"}"#,
        );
        // A finished tool part (status='completed', no interrupted flag)
        // must NOT count toward stuck_tools.
        insert_part(
            &conn,
            "prt_done_tool",
            "msg_done",
            r#"{"type":"tool","state":{"status":"completed"}}"#,
        );

        let meta = fetch_last_message_meta(&conn, &[]).unwrap();
        let entry = meta.get("ses_done").unwrap();
        assert!(!entry.assistant_errored);
        assert_eq!(entry.stuck_tools, 0);
        let status = classify_static(entry);
        assert_eq!(status, StaticStatus::Completed);
        assert!(!status.is_resumable());
    }

    /// Insert a message with a custom timestamp (the helper above hardcodes 1500).
    fn insert_message_at(
        conn: &Connection,
        msg_id: &str,
        session_id: &str,
        role: &str,
        time_created: i64,
    ) {
        insert_message_full(conn, msg_id, session_id, role, time_created, None, None);
    }

    /// Insert a message with optional `agent` and `model` fields in
    /// the JSON `data` blob, mirroring opencode's MessageV2.User shape.
    fn insert_message_full(
        conn: &Connection,
        msg_id: &str,
        session_id: &str,
        role: &str,
        time_created: i64,
        agent: Option<&str>,
        model: Option<(&str, &str)>,
    ) {
        // Build the JSON payload by hand to mirror opencode's storage.
        let mut data = serde_json::json!({
            "role": role,
            "time": { "completed": serde_json::Value::Null },
        });
        if let Some(agent) = agent {
            data["agent"] = serde_json::Value::String(agent.to_string());
        }
        if let Some((provider_id, model_id)) = model {
            data["model"] = serde_json::json!({
                "providerID": provider_id,
                "modelID": model_id,
            });
        }
        conn.execute(
            "INSERT INTO message (id, session_id, time_created, data) VALUES (?, ?, ?, ?)",
            rusqlite::params![msg_id, session_id, time_created, data.to_string()],
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
        assert_eq!(payload.agent, None);
        assert_eq!(payload.model, None);
    }

    /// Phase A2 — the new payload extracts `agent` and `model` from
    /// the user message's JSON.  This is what `nudge` forwards to
    /// opencode's `prompt_async` so the resumed turn keeps the
    /// session's original identity rather than falling back to the
    /// daemon's `default_agent`.
    #[test]
    fn fetch_last_user_message_extracts_agent_and_model() {
        let conn = setup_db();
        conn.execute(
            "INSERT INTO session (id, parent_id, directory, title, time_created, time_updated) VALUES ('ses_1', NULL, '', '', 1000, 2000)",
            [],
        ).unwrap();
        insert_message_full(
            &conn,
            "msg_user",
            "ses_1",
            "user",
            1500,
            Some("Conductor"),
            Some(("anthropic", "claude-opus-4-5")),
        );
        insert_part_at(
            &conn,
            "prt_text",
            "msg_user",
            r#"{"type":"text","text":"hello"}"#,
            1600,
        );

        let payload = fetch_last_user_message(&conn, "ses_1").unwrap().unwrap();
        assert_eq!(payload.user_msg_id, "msg_user");
        assert_eq!(payload.text, "hello");
        assert_eq!(payload.agent.as_deref(), Some("Conductor"));
        assert_eq!(
            payload.model,
            Some(ResumeModel {
                provider_id: "anthropic".to_string(),
                model_id: "claude-opus-4-5".to_string(),
            })
        );
    }

    /// Phase A2 — when `model` is missing or partial (e.g. only
    /// `providerID` recorded), we return `None` rather than fabricate
    /// a partial value.  Opencode will then fall back to the agent's
    /// configured model, which is the right behavior.
    #[test]
    fn fetch_last_user_message_missing_model_returns_none() {
        let conn = setup_db();
        conn.execute(
            "INSERT INTO session (id, parent_id, directory, title, time_created, time_updated) VALUES ('ses_1', NULL, '', '', 1000, 2000)",
            [],
        ).unwrap();
        // Insert with agent but no model.
        insert_message_full(&conn, "msg_user", "ses_1", "user", 1500, Some("ag"), None);

        let payload = fetch_last_user_message(&conn, "ses_1").unwrap().unwrap();
        assert_eq!(payload.agent.as_deref(), Some("ag"));
        assert_eq!(payload.model, None);
    }

    /// Phase A2b — for AssistantPartial / AssistantToolStuck shapes,
    /// the nudge's ContinuePrompt path needs the agent/model from the
    /// user message that PRECEDES the broken assistant turn (not the
    /// most recent user message in absolute terms, which may not
    /// exist or may be unrelated).
    #[test]
    fn fetch_user_message_before_last_assistant_picks_correct_user() {
        let conn = setup_db();
        conn.execute(
            "INSERT INTO session (id, parent_id, directory, title, time_created, time_updated) VALUES ('ses_1', NULL, '', '', 1000, 2000)",
            [],
        ).unwrap();
        // Realistic timeline:
        //   t=1000  user msg with agent=A   (driving turn 1)
        //   t=1100  assistant (completed, turn 1)
        //   t=2000  user msg with agent=B   (driving turn 2 — the broken one)
        //   t=2100  assistant (partial, turn 2 — never completed)
        insert_message_full(
            &conn,
            "msg_u1",
            "ses_1",
            "user",
            1000,
            Some("A"),
            Some(("anthropic", "claude-sonnet-4")),
        );
        insert_message_full(&conn, "msg_a1", "ses_1", "assistant", 1100, None, None);
        insert_message_full(
            &conn,
            "msg_u2",
            "ses_1",
            "user",
            2000,
            Some("B"),
            Some(("openai", "gpt-5")),
        );
        insert_message_full(&conn, "msg_a2", "ses_1", "assistant", 2100, None, None);
        insert_part_at(
            &conn,
            "prt_u2",
            "msg_u2",
            r#"{"type":"text","text":"do thing B"}"#,
            2050,
        );

        let payload = fetch_user_message_before_last_assistant(&conn, "ses_1")
            .unwrap()
            .unwrap();
        assert_eq!(
            payload.user_msg_id, "msg_u2",
            "must pick the user message that drove the last assistant"
        );
        assert_eq!(payload.agent.as_deref(), Some("B"));
        assert_eq!(payload.text, "do thing B");
    }

    /// Phase A2b — defensive: if no assistant message exists, return
    /// None.  (In practice this shouldn't happen for AssistantPartial
    /// shapes — the classifier would have produced UserPending —
    /// but the function should still behave safely.)
    #[test]
    fn fetch_user_message_before_last_assistant_no_assistant() {
        let conn = setup_db();
        conn.execute(
            "INSERT INTO session (id, parent_id, directory, title, time_created, time_updated) VALUES ('ses_1', NULL, '', '', 1000, 2000)",
            [],
        ).unwrap();
        insert_message_full(&conn, "msg_u", "ses_1", "user", 1000, Some("A"), None);

        let payload = fetch_user_message_before_last_assistant(&conn, "ses_1").unwrap();
        assert!(payload.is_none());
    }

    /// Phase D.1 — SAFETY BOUNDARY: a session with (user_msg,
    /// assistant_msg-with-parts) must NEVER classify as UserPending
    /// or AssistantEmpty, because CleanResume on either of those would
    /// delete real assistant work.  It must classify as
    /// AssistantPartial (or AssistantToolStuck if there's a stuck
    /// tool), which routes to ContinuePrompt (preserves the work).
    ///
    /// This guards against a hypothetical regression where the
    /// classifier's ordering invariant breaks.
    #[test]
    fn user_then_assistant_with_parts_never_classifies_as_user_pending_or_empty() {
        let conn = setup_db();
        conn.execute(
            "INSERT INTO session (id, parent_id, directory, title, time_created, time_updated) VALUES ('ses_1', NULL, '', '', 1000, 2000)",
            [],
        ).unwrap();
        // User message FIRST, then assistant message AFTER with a part.
        insert_message_at(&conn, "msg_u", "ses_1", "user", 1000);
        insert_message_at(&conn, "msg_a", "ses_1", "assistant", 2000);
        insert_part_at(
            &conn,
            "prt_a",
            "msg_a",
            r#"{"type":"text","text":"partial reply"}"#,
            2100,
        );

        let meta = fetch_last_message_meta(&conn, &[]).unwrap();
        let entry = meta.get("ses_1").unwrap();
        let status = classify_static(entry);
        assert_ne!(
            status,
            StaticStatus::UserPending,
            "classifier MUST NOT return UserPending while a partial assistant exists \
             (would cause CleanResume to delete the partial work). entry={:?}",
            entry
        );
        assert_ne!(
            status,
            StaticStatus::AssistantEmpty,
            "classifier MUST NOT return AssistantEmpty when assistant has parts \
             (would cause CleanResume to delete the partial work). entry={:?}",
            entry
        );
        assert_eq!(
            status,
            StaticStatus::AssistantPartial,
            "expected AssistantPartial for (user, assistant-with-parts) shape"
        );
    }

    /// Phase D.1 — the same boundary, with a stuck tool: must be
    /// classified as AssistantToolStuck, never UserPending/Empty.
    #[test]
    fn user_then_assistant_with_stuck_tool_classifies_as_tool_stuck() {
        let conn = setup_db();
        conn.execute(
            "INSERT INTO session (id, parent_id, directory, title, time_created, time_updated) VALUES ('ses_1', NULL, '', '', 1000, 2000)",
            [],
        ).unwrap();
        insert_message_at(&conn, "msg_u", "ses_1", "user", 1000);
        insert_message_at(&conn, "msg_a", "ses_1", "assistant", 2000);
        insert_part_at(
            &conn,
            "prt_tool",
            "msg_a",
            r#"{"type":"tool","state":{"status":"running"}}"#,
            2100,
        );

        let meta = fetch_last_message_meta(&conn, &[]).unwrap();
        let status = classify_static(meta.get("ses_1").unwrap());
        assert_eq!(status, StaticStatus::AssistantToolStuck);
    }
}

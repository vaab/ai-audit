use anyhow::{Context, Result};
use chrono::{DateTime, TimeZone, Utc};
use rusqlite::Connection;
use std::path::PathBuf;

use super::SessionInfo;

/// Path to the OpenCode SQLite database.
pub fn db_path() -> PathBuf {
    crate::opencode_data_dir().join("opencode.db")
}

/// Check whether the SQLite database file exists.
pub fn db_exists() -> bool {
    db_path().exists()
}

/// Open the database in read-only mode with WAL journal.
pub fn open_db() -> Result<Connection> {
    let path = db_path();
    let conn = Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("Failed to open database: {}", path.display()))?;
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA query_only=true;")?;
    Ok(conn)
}

/// Open an arbitrary database path (used by tests and for custom paths).
pub fn open_db_at(path: &std::path::Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("Failed to open database: {}", path.display()))?;
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA query_only=true;")?;
    Ok(conn)
}

/// Open the database in read-write mode.
///
/// Used exclusively by `session delete` to issue DELETE statements
/// against the `session`, `message`, and `part` tables.  Other
/// callers MUST use [`open_db`] which is read-only and pinned with
/// `PRAGMA query_only=true`.
///
/// WAL mode is preserved so concurrent readers (opencode itself, or
/// another `ai-audit` invocation) continue to function during the
/// write transaction.
pub fn open_db_rw() -> Result<Connection> {
    let path = db_path();
    let conn = Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)
        .with_context(|| format!("Failed to open database for write: {}", path.display()))?;
    conn.execute_batch("PRAGMA journal_mode=WAL;")?;
    Ok(conn)
}

/// Open an arbitrary database path in read-write mode (for tests).
pub fn open_db_rw_at(path: &std::path::Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)
        .with_context(|| format!("Failed to open database for write: {}", path.display()))?;
    conn.execute_batch("PRAGMA journal_mode=WAL;")?;
    Ok(conn)
}

/// Report from a successful per-session row deletion.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DbDeleteReport {
    /// Number of rows deleted from the `session` table (0 or 1).
    pub session_rows: usize,
    /// Number of rows deleted from the `message` table.
    pub message_rows: usize,
    /// Number of rows deleted from the `part` table.
    pub part_rows: usize,
}

impl DbDeleteReport {
    /// Total rows wiped across all three tables.
    pub fn total(&self) -> usize {
        self.session_rows + self.message_rows + self.part_rows
    }
}

/// Delete every row keyed by `session_id` across the `session`,
/// `message`, and `part` tables, atomically.
///
/// Order: `part` first, then `message`, then `session`.  This avoids
/// relying on `ON DELETE CASCADE` (which the upstream schema declares
/// for `part.message_id → message.id` but NOT for the denormalized
/// `part.session_id`) and ensures the wipe is exact even if the
/// schema changes.
///
/// Wrapped in a single transaction: a mid-delete crash leaves the
/// row set fully intact rather than half-removed.
///
/// MUST NOT touch the `permission` table (per-project, not
/// per-session) or any other table.
pub fn delete_session_rows(conn: &mut Connection, session_id: &str) -> Result<DbDeleteReport> {
    let tx = conn
        .transaction()
        .with_context(|| format!("Failed to begin transaction for session {}", session_id))?;
    let part_rows = tx
        .execute("DELETE FROM part WHERE session_id = ?", [session_id])
        .with_context(|| format!("DELETE FROM part WHERE session_id = '{}'", session_id))?;
    let message_rows = tx
        .execute("DELETE FROM message WHERE session_id = ?", [session_id])
        .with_context(|| format!("DELETE FROM message WHERE session_id = '{}'", session_id))?;
    let session_rows = tx
        .execute("DELETE FROM session WHERE id = ?", [session_id])
        .with_context(|| format!("DELETE FROM session WHERE id = '{}'", session_id))?;
    tx.commit()
        .with_context(|| format!("Failed to commit delete for session {}", session_id))?;
    Ok(DbDeleteReport {
        session_rows,
        message_rows,
        part_rows,
    })
}

fn ms_to_datetime(ms: i64) -> DateTime<Utc> {
    Utc.timestamp_millis_opt(ms)
        .single()
        .unwrap_or_else(Utc::now)
}

/// List all sessions from the SQLite database.
pub fn list_sessions_from_db() -> Result<Vec<SessionInfo>> {
    let conn = open_db()?;
    list_sessions_from_conn(&conn)
}

/// List all sessions using an existing connection (testable).
pub fn list_sessions_from_conn(conn: &Connection) -> Result<Vec<SessionInfo>> {
    let mut stmt = conn.prepare(
        "SELECT id, parent_id, directory, title, time_created, time_updated \
         FROM session ORDER BY time_created",
    )?;

    let rows = stmt.query_map([], |row| {
        let id: String = row.get(0)?;
        let parent_id: Option<String> = row.get(1)?;
        let directory: String = row.get::<_, Option<String>>(2)?.unwrap_or_default();
        let title: String = row.get::<_, Option<String>>(3)?.unwrap_or_default();
        let time_created: i64 = row.get(4)?;
        let time_updated: i64 = row.get(5)?;

        Ok((id, parent_id, directory, title, time_created, time_updated))
    })?;

    let mut sessions = Vec::new();
    for row in rows {
        let (id, parent_id, directory, title, time_created, time_updated) = row?;
        let started_at = ms_to_datetime(time_created);
        let updated_at = if time_updated > 0 {
            ms_to_datetime(time_updated)
        } else {
            started_at
        };
        sessions.push(SessionInfo {
            session_id: id,
            started_at,
            updated_at,
            project_dir: directory,
            title,
            parent_id,
        });
    }

    Ok(sessions)
}

/// Get info for a single session from the database.
pub fn get_session_info_from_db(session_id: &str) -> Result<SessionInfo> {
    let conn = open_db()?;
    get_session_info_from_conn(&conn, session_id)
}

/// Get info for a single session using an existing connection (testable).
pub fn get_session_info_from_conn(conn: &Connection, session_id: &str) -> Result<SessionInfo> {
    let mut stmt = conn.prepare(
        "SELECT id, parent_id, directory, title, time_created, time_updated \
         FROM session WHERE id = ?",
    )?;

    let (id, parent_id, directory, title, time_created, time_updated) =
        stmt.query_row([session_id], |row| {
            let id: String = row.get(0)?;
            let parent_id: Option<String> = row.get(1)?;
            let directory: String = row.get::<_, Option<String>>(2)?.unwrap_or_default();
            let title: String = row.get::<_, Option<String>>(3)?.unwrap_or_default();
            let time_created: i64 = row.get(4)?;
            let time_updated: i64 = row.get(5)?;
            Ok((id, parent_id, directory, title, time_created, time_updated))
        })?;

    let started_at = ms_to_datetime(time_created);
    let updated_at = if time_updated > 0 {
        ms_to_datetime(time_updated)
    } else {
        started_at
    };

    Ok(SessionInfo {
        session_id: id,
        started_at,
        updated_at,
        project_dir: directory,
        title,
        parent_id,
    })
}

/// Check if any part in a session contains the needle text.
///
/// Queries all parts for the session, parses their `data` JSON, and applies
/// the same `part_contains_needle()` logic used by the file-based search.
pub fn session_contains_text_from_db(session_id: &str, needle: &str) -> bool {
    let conn = match open_db() {
        Ok(c) => c,
        Err(_) => return false,
    };
    session_contains_text_from_conn(&conn, session_id, needle)
}

/// Testable version using an existing connection.
pub fn session_contains_text_from_conn(conn: &Connection, session_id: &str, needle: &str) -> bool {
    let parts = match get_parts_for_session(conn, session_id) {
        Ok(p) => p,
        Err(_) => return false,
    };

    for (_msg_id, part) in &parts {
        // Fast pre-filter on raw JSON string
        let raw = part.to_string();
        if !raw.contains(needle) {
            continue;
        }
        if super::part_contains_needle(part, needle) {
            return true;
        }
    }
    false
}

/// Check if the last N messages of a session contain the needle text.
pub fn session_tail_contains_text_from_db(session_id: &str, needle: &str, last_n: usize) -> bool {
    let conn = match open_db() {
        Ok(c) => c,
        Err(_) => return false,
    };
    session_tail_contains_text_from_conn(&conn, session_id, needle, last_n)
}

/// Testable version using an existing connection.
pub fn session_tail_contains_text_from_conn(
    conn: &Connection,
    session_id: &str,
    needle: &str,
    last_n: usize,
) -> bool {
    // Get the last N message IDs
    let mut stmt = match conn
        .prepare("SELECT id FROM message WHERE session_id = ? ORDER BY time_created DESC LIMIT ?")
    {
        Ok(s) => s,
        Err(_) => return false,
    };

    let msg_ids: Vec<String> = match stmt
        .query_map(rusqlite::params![session_id, last_n as i64], |row| {
            row.get(0)
        }) {
        Ok(rows) => rows.filter_map(|r| r.ok()).collect(),
        Err(_) => return false,
    };

    if msg_ids.is_empty() {
        return false;
    }

    // Check parts for each of these messages
    for msg_id in &msg_ids {
        let parts = match get_parts_for_message(conn, msg_id) {
            Ok(p) => p,
            Err(_) => continue,
        };
        for part in &parts {
            let raw = part.to_string();
            if !raw.contains(needle) {
                continue;
            }
            if super::part_contains_needle(part, needle) {
                return true;
            }
        }
    }
    false
}

/// Check if a session's recent messages match structured filters.
pub(crate) fn session_matches_filters_from_db(
    session_id: &str,
    filters: &[crate::session_detect::SessionFilter],
    last_n: usize,
    project_dir: &str,
) -> bool {
    let conn = match open_db() {
        Ok(c) => c,
        Err(_) => return false,
    };
    session_matches_filters_from_conn(&conn, session_id, filters, last_n, project_dir)
}

/// Testable version using an existing connection.
pub(crate) fn session_matches_filters_from_conn(
    conn: &Connection,
    session_id: &str,
    filters: &[crate::session_detect::SessionFilter],
    last_n: usize,
    project_dir: &str,
) -> bool {
    // Get the last `last_n` message IDs
    let mut stmt = match conn
        .prepare("SELECT id FROM message WHERE session_id = ? ORDER BY time_created DESC LIMIT ?")
    {
        Ok(s) => s,
        Err(_) => return false,
    };

    let msg_ids: Vec<String> = match stmt
        .query_map(rusqlite::params![session_id, last_n as i64], |row| {
            row.get(0)
        }) {
        Ok(rows) => rows.filter_map(|r| r.ok()).collect(),
        Err(_) => return false,
    };

    if msg_ids.is_empty() {
        return false;
    }

    log::trace!(
        "session_matches_filters: session={}, window={}, msg_ids={:?}",
        session_id,
        last_n,
        msg_ids
    );

    // Collect ALL parts from ALL messages in the window.
    // OpenCode spreads a single assistant turn across multiple DB messages
    // (one for text, one per tool call, etc.), so criteria from one TUI
    // "message" may land in different DB messages.  We match criteria
    // against the combined pool instead of per-message.
    let mut all_parts: Vec<serde_json::Value> = Vec::new();
    for msg_id in &msg_ids {
        if let Ok(parts) = get_parts_for_message(conn, msg_id) {
            all_parts.extend(parts);
        }
    }

    log::trace!(
        "session_matches_filters: session={}, total_parts={}",
        session_id,
        all_parts.len()
    );

    for (i, filter) in filters.iter().enumerate() {
        let matched =
            message_parts_match_criteria(&all_parts, &filter.criteria, project_dir, session_id);
        log::trace!(
            "session_matches_filters: session={}, filter[{}] depth={} => {}",
            session_id,
            i,
            filter.depth,
            if matched { "MATCH" } else { "NO MATCH" }
        );
        if !matched {
            return false;
        }
    }
    true
}

/// Resolve a relative path against a base directory (lexical, no filesystem).
fn resolve_path(rel: &str, base: &str) -> String {
    use std::path::{Component, Path};

    let p = Path::new(rel);
    if p.is_absolute() || base.is_empty() {
        return rel.to_string();
    }
    let joined = Path::new(base).join(p);
    let mut parts: Vec<&std::ffi::OsStr> = Vec::new();
    for comp in joined.components() {
        match comp {
            Component::ParentDir => {
                parts.pop();
            }
            Component::CurDir => {}
            other => parts.push(other.as_os_str()),
        }
    }
    let resolved: std::path::PathBuf = parts.iter().collect();
    resolved.to_string_lossy().to_string()
}

fn message_parts_match_criteria(
    parts: &[serde_json::Value],
    criteria: &[crate::session_detect::FilterCriterion],
    project_dir: &str,
    session_id: &str,
) -> bool {
    // ALL criteria must be satisfied by parts of THIS message
    for (ci, criterion) in criteria.iter().enumerate() {
        let matched = match criterion {
            crate::session_detect::FilterCriterion::TextContains(needle) => {
                log::trace!(
                    "  criterion[{}] TextContains({:?}): {} parts in pool (session={})",
                    ci,
                    needle,
                    parts.len(),
                    session_id
                );
                let found = parts.iter().any(|part| {
                    let hit = super::part_contains_needle(part, needle);
                    if hit {
                        let ptype = part.get("type").and_then(|t| t.as_str()).unwrap_or("?");
                        log::trace!("    MATCH in {} part", ptype);
                    }
                    hit
                });
                if !found {
                    log::trace!("    no part contains the needle");
                }
                found
            }
            crate::session_detect::FilterCriterion::ToolFieldEquals {
                tool_name,
                field,
                value,
            } => {
                // For path-like fields, resolve relative paths against
                // the candidate session's project directory before comparing.
                let resolved = if (field == "filePath" || field == "path")
                    && !value.is_empty()
                    && !std::path::Path::new(value.as_str()).is_absolute()
                {
                    let r = resolve_path(value, project_dir);
                    log::trace!(
                        "  criterion[{}] ToolFieldEquals: resolved {:?} against {:?} => {:?}",
                        ci,
                        value,
                        project_dir,
                        r
                    );
                    r
                } else {
                    value.clone()
                };

                let tool_parts: Vec<_> = parts
                    .iter()
                    .filter(|p| p.get("type").and_then(|t| t.as_str()) == Some("tool"))
                    .collect();
                log::trace!(
                    "  criterion[{}] ToolFieldEquals(tool={}, {}={:?}): {} tool parts (session={})",
                    ci,
                    tool_name,
                    field,
                    resolved,
                    tool_parts.len(),
                    session_id
                );

                let found = tool_parts.iter().any(|part| {
                    let part_tool = part.get("tool").and_then(|t| t.as_str()).unwrap_or("");
                    let tool_match = part_tool == tool_name;

                    if !tool_match {
                        return false;
                    }

                    let field_val = part
                        .get("state")
                        .and_then(|s| s.get("input"))
                        .and_then(|i| i.get(field.as_str()));

                    let field_match = field_val.is_some_and(|v| {
                        if let Some(s) = v.as_str() {
                            s.starts_with(resolved.as_str())
                        } else {
                            v.to_string().starts_with(resolved.as_str())
                        }
                    });

                    log::trace!(
                        "    tool={:?} tool_match={}, field {:?}={:?} vs expected {:?} => {}",
                        part_tool,
                        tool_match,
                        field,
                        field_val
                            .map(|v| v.to_string())
                            .unwrap_or_else(|| "MISSING".into()),
                        resolved,
                        if field_match { "MATCH" } else { "NO MATCH" }
                    );

                    field_match
                });

                if !found && log::log_enabled!(log::Level::Trace) {
                    let tool_names: Vec<&str> = tool_parts
                        .iter()
                        .filter_map(|p| p.get("tool").and_then(|t| t.as_str()))
                        .collect();
                    log::trace!(
                        "    NO MATCH for session {}: tool names in pool: {:?}",
                        session_id,
                        tool_names
                    );
                }

                found
            }
        };
        if !matched {
            return false;
        }
    }
    true
}

/// Check if any part in a session contains a write/edit tool targeting the given file path.
///
/// Queries all parts for the session, parses their `data` JSON, and applies
/// the `part_edits_file()` logic.
pub fn session_edited_file_from_db(session_id: &str, target_path: &str) -> bool {
    let conn = match open_db() {
        Ok(c) => c,
        Err(_) => return false,
    };
    session_edited_file_from_conn(&conn, session_id, target_path)
}

/// Testable version using an existing connection.
pub fn session_edited_file_from_conn(
    conn: &Connection,
    session_id: &str,
    target_path: &str,
) -> bool {
    let parts = match get_parts_for_session(conn, session_id) {
        Ok(p) => p,
        Err(_) => return false,
    };

    // Fast pre-filter: extract filename component
    let filename = std::path::Path::new(target_path)
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or(target_path);

    for (_msg_id, part) in &parts {
        // Fast pre-filter on raw JSON string
        let raw = part.to_string();
        if !raw.contains(filename) {
            continue;
        }
        if super::part_edits_file(part, target_path) {
            return true;
        }
    }
    false
}

/// Get messages for a session as parsed JSON values.
///
/// Returns `(msg_id, data)` pairs ordered by creation time.  The `id`
/// field from the SQL row is injected into the `data` JSON when absent
/// so that downstream consumers always find it at `data["id"]`.
pub fn get_messages_for_session(
    conn: &Connection,
    session_id: &str,
) -> Result<Vec<(String, serde_json::Value)>> {
    let mut stmt = conn
        .prepare("SELECT id, data FROM message WHERE session_id = ? ORDER BY time_created ASC")?;

    let rows = stmt.query_map([session_id], |row| {
        let id: String = row.get(0)?;
        let data_str: String = row.get(1)?;
        Ok((id, data_str))
    })?;

    let mut messages = Vec::new();
    for row in rows {
        let (id, data_str) = row?;
        let mut data: serde_json::Value = match serde_json::from_str(&data_str) {
            Ok(v) => v,
            Err(_) => continue,
        };
        // DB rows store `id` as a separate column; inject it so all
        // downstream code can rely on `data["id"]` being present.
        if data.get("id").is_none() {
            data["id"] = serde_json::Value::String(id.clone());
        }
        messages.push((id, data));
    }

    Ok(messages)
}

/// Get parsed `data` JSON values for all parts of a message.
pub fn get_parts_for_message(
    conn: &Connection,
    message_id: &str,
) -> Result<Vec<serde_json::Value>> {
    let mut stmt =
        conn.prepare("SELECT data FROM part WHERE message_id = ? ORDER BY time_created ASC")?;

    let rows = stmt.query_map([message_id], |row| {
        let data_str: String = row.get(0)?;
        Ok(data_str)
    })?;

    let mut parts = Vec::new();
    let mut raw_count = 0u32;
    let mut parse_failures = 0u32;
    for row in rows {
        raw_count += 1;
        let data_str = row?;
        match serde_json::from_str::<serde_json::Value>(&data_str) {
            Ok(val) => parts.push(val),
            Err(e) => {
                parse_failures += 1;
                log::warn!(
                    "get_parts_for_message: failed to parse part JSON for message {}: {}",
                    message_id,
                    e
                );
                log::trace!(
                    "get_parts_for_message: raw data (first 200 chars): {:?}",
                    &data_str[..data_str.len().min(200)]
                );
            }
        }
    }

    log::trace!(
        "get_parts_for_message: message={}, raw_rows={}, parsed={}, failures={}",
        message_id,
        raw_count,
        parts.len(),
        parse_failures
    );

    Ok(parts)
}

/// Get all parts for a session: returns (message_id, data_json).
///
/// Joins through the message table to find all parts belonging to a session.
pub fn get_parts_for_session(
    conn: &Connection,
    session_id: &str,
) -> Result<Vec<(String, serde_json::Value)>> {
    let mut stmt = conn.prepare(
        "SELECT message_id, data FROM part WHERE session_id = ? ORDER BY time_created ASC",
    )?;

    let rows = stmt.query_map([session_id], |row| {
        let msg_id: String = row.get(0)?;
        let data_str: String = row.get(1)?;
        Ok((msg_id, data_str))
    })?;

    let mut parts = Vec::new();
    for row in rows {
        let (msg_id, data_str) = row?;
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&data_str) {
            parts.push((msg_id, val));
        }
    }

    Ok(parts)
}

/// Create the schema in a connection (for testing).
#[cfg(test)]
pub fn create_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS session (
            id TEXT PRIMARY KEY,
            project_id TEXT,
            parent_id TEXT,
            directory TEXT,
            title TEXT,
            time_created INTEGER NOT NULL,
            time_updated INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS message (
            id TEXT PRIMARY KEY,
            session_id TEXT NOT NULL,
            time_created INTEGER NOT NULL,
            time_updated INTEGER NOT NULL,
            data TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS part (
            id TEXT PRIMARY KEY,
            message_id TEXT NOT NULL,
            session_id TEXT NOT NULL,
            time_created INTEGER NOT NULL,
            time_updated INTEGER NOT NULL,
            data TEXT NOT NULL
        );",
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn setup_test_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        create_schema(&conn).unwrap();
        conn
    }

    fn insert_session(
        conn: &Connection,
        id: &str,
        parent_id: Option<&str>,
        directory: &str,
        title: &str,
        time_created: i64,
        time_updated: i64,
    ) {
        conn.execute(
            "INSERT INTO session (id, project_id, parent_id, directory, title, time_created, time_updated) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![id, "proj_1", parent_id, directory, title, time_created, time_updated],
        )
        .unwrap();
    }

    fn insert_message(conn: &Connection, id: &str, session_id: &str, role: &str, time_ms: i64) {
        let data = format!(r#"{{"role":"{}","time":{{"created":{}}}}}"#, role, time_ms);
        conn.execute(
            "INSERT INTO message (id, session_id, time_created, time_updated, data) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![id, session_id, time_ms, time_ms, data],
        )
        .unwrap();
    }

    fn insert_part(
        conn: &Connection,
        id: &str,
        message_id: &str,
        session_id: &str,
        time_ms: i64,
        data_json: &str,
    ) {
        conn.execute(
            "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![id, message_id, session_id, time_ms, time_ms, data_json],
        )
        .unwrap();
    }

    #[test]
    fn test_list_sessions_from_conn_empty() {
        let conn = setup_test_db();
        let sessions = list_sessions_from_conn(&conn).unwrap();
        assert!(sessions.is_empty());
    }

    #[test]
    fn test_list_sessions_from_conn() {
        let conn = setup_test_db();
        insert_session(
            &conn,
            "ses_001",
            None,
            "/home/user/project",
            "First session",
            1705314600000,
            1705314700000,
        );
        insert_session(
            &conn,
            "ses_002",
            Some("ses_001"),
            "/home/user/project",
            "Sub-agent",
            1705314800000,
            1705314900000,
        );

        let sessions = list_sessions_from_conn(&conn).unwrap();
        assert_eq!(sessions.len(), 2);

        assert_eq!(sessions[0].session_id, "ses_001");
        assert_eq!(sessions[0].project_dir, "/home/user/project");
        assert_eq!(sessions[0].title, "First session");
        assert!(sessions[0].parent_id.is_none());

        assert_eq!(sessions[1].session_id, "ses_002");
        assert_eq!(sessions[1].parent_id.as_deref(), Some("ses_001"));
    }

    #[test]
    fn test_list_sessions_ordering() {
        let conn = setup_test_db();
        // Insert in reverse order
        insert_session(
            &conn,
            "ses_b",
            None,
            "/b",
            "B",
            1705314800000,
            1705314800000,
        );
        insert_session(
            &conn,
            "ses_a",
            None,
            "/a",
            "A",
            1705314600000,
            1705314600000,
        );

        let sessions = list_sessions_from_conn(&conn).unwrap();
        assert_eq!(sessions[0].session_id, "ses_a");
        assert_eq!(sessions[1].session_id, "ses_b");
    }

    #[test]
    fn test_get_session_info_from_conn() {
        let conn = setup_test_db();
        insert_session(
            &conn,
            "ses_001",
            None,
            "/home/user/project",
            "My session",
            1705314600000,
            1705314700000,
        );

        let info = get_session_info_from_conn(&conn, "ses_001").unwrap();
        assert_eq!(info.session_id, "ses_001");
        assert_eq!(info.title, "My session");
        assert_eq!(info.project_dir, "/home/user/project");
    }

    #[test]
    fn test_get_session_info_not_found() {
        let conn = setup_test_db();
        let result = get_session_info_from_conn(&conn, "ses_missing");
        assert!(result.is_err());
    }

    #[test]
    fn test_get_messages_for_session() {
        let conn = setup_test_db();
        insert_message(&conn, "msg_001", "ses_001", "user", 1705314600000);
        insert_message(&conn, "msg_002", "ses_001", "assistant", 1705314601000);
        insert_message(&conn, "msg_003", "ses_002", "user", 1705314602000);

        let messages = get_messages_for_session(&conn, "ses_001").unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].0, "msg_001");
        assert_eq!(messages[0].1.get("role").unwrap().as_str().unwrap(), "user");
        assert_eq!(
            messages[0].1.get("id").unwrap().as_str().unwrap(),
            "msg_001"
        );
        assert_eq!(messages[1].0, "msg_002");
        assert_eq!(
            messages[1].1.get("role").unwrap().as_str().unwrap(),
            "assistant"
        );
    }

    #[test]
    fn test_get_parts_for_message() {
        let conn = setup_test_db();
        insert_part(
            &conn,
            "prt_001",
            "msg_001",
            "ses_001",
            1705314600000,
            r#"{"type":"text","text":"Hello world"}"#,
        );
        insert_part(
            &conn,
            "prt_002",
            "msg_001",
            "ses_001",
            1705314600100,
            r#"{"type":"text","text":"More text"}"#,
        );

        let parts = get_parts_for_message(&conn, "msg_001").unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(
            parts[0].get("text").unwrap().as_str().unwrap(),
            "Hello world"
        );
        assert_eq!(parts[1].get("text").unwrap().as_str().unwrap(), "More text");
    }

    #[test]
    fn test_get_parts_for_session() {
        let conn = setup_test_db();
        insert_part(
            &conn,
            "prt_001",
            "msg_001",
            "ses_001",
            1705314600000,
            r#"{"type":"text","text":"Hello"}"#,
        );
        insert_part(
            &conn,
            "prt_002",
            "msg_002",
            "ses_001",
            1705314601000,
            r#"{"type":"tool","tool":"bash","state":{"input":{"command":"ls"}}}"#,
        );
        insert_part(
            &conn,
            "prt_003",
            "msg_003",
            "ses_002",
            1705314602000,
            r#"{"type":"text","text":"Other session"}"#,
        );

        let parts = get_parts_for_session(&conn, "ses_001").unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].0, "msg_001");
        assert_eq!(parts[1].0, "msg_002");
    }

    #[test]
    fn test_session_contains_text_from_conn_text_part() {
        let conn = setup_test_db();
        insert_message(&conn, "msg_001", "ses_001", "user", 1705314600000);
        insert_part(
            &conn,
            "prt_001",
            "msg_001",
            "ses_001",
            1705314600000,
            r#"{"type":"text","text":"Hello world"}"#,
        );

        assert!(session_contains_text_from_conn(&conn, "ses_001", "Hello"));
        assert!(session_contains_text_from_conn(&conn, "ses_001", "world"));
        assert!(!session_contains_text_from_conn(&conn, "ses_001", "WORLD"));
        assert!(!session_contains_text_from_conn(
            &conn, "ses_001", "goodbye"
        ));
    }

    #[test]
    fn test_session_contains_text_from_conn_tool_part() {
        let conn = setup_test_db();
        insert_message(&conn, "msg_001", "ses_001", "assistant", 1705314600000);
        insert_part(
            &conn,
            "prt_001",
            "msg_001",
            "ses_001",
            1705314600000,
            r#"{"type":"tool","tool":"bash","state":{"input":{"command":"cargo test"},"output":"test passed"}}"#,
        );

        assert!(session_contains_text_from_conn(&conn, "ses_001", "bash"));
        assert!(session_contains_text_from_conn(
            &conn,
            "ses_001",
            "cargo test"
        ));
        assert!(session_contains_text_from_conn(
            &conn,
            "ses_001",
            "test passed"
        ));
        assert!(!session_contains_text_from_conn(
            &conn,
            "ses_001",
            "npm install"
        ));
    }

    #[test]
    fn test_session_tail_contains_text_from_conn() {
        let conn = setup_test_db();
        insert_session(
            &conn,
            "ses_001",
            None,
            "/proj",
            "Test",
            1705314600000,
            1705314605000,
        );
        insert_message(&conn, "msg_001", "ses_001", "user", 1705314600000);
        insert_message(&conn, "msg_002", "ses_001", "assistant", 1705314601000);
        insert_message(&conn, "msg_003", "ses_001", "user", 1705314602000);
        insert_message(&conn, "msg_004", "ses_001", "assistant", 1705314603000);
        insert_message(&conn, "msg_005", "ses_001", "user", 1705314604000);

        insert_part(
            &conn,
            "prt_001",
            "msg_001",
            "ses_001",
            1705314600000,
            r#"{"type":"text","text":"old message"}"#,
        );
        insert_part(
            &conn,
            "prt_003",
            "msg_003",
            "ses_001",
            1705314602000,
            r#"{"type":"text","text":"unique target phrase"}"#,
        );
        insert_part(
            &conn,
            "prt_004",
            "msg_004",
            "ses_001",
            1705314603000,
            r#"{"type":"text","text":"recent message"}"#,
        );
        insert_part(
            &conn,
            "prt_005",
            "msg_005",
            "ses_001",
            1705314604000,
            r#"{"type":"text","text":"latest message"}"#,
        );

        // Last 2 should NOT find "unique target phrase" (it's in msg_003)
        assert!(!session_tail_contains_text_from_conn(
            &conn,
            "ses_001",
            "unique target phrase",
            2
        ));

        // Last 3 SHOULD find it
        assert!(session_tail_contains_text_from_conn(
            &conn,
            "ses_001",
            "unique target phrase",
            3
        ));

        // Last 2 should find "latest message"
        assert!(session_tail_contains_text_from_conn(
            &conn,
            "ses_001",
            "latest message",
            2
        ));
    }

    #[test]
    fn test_session_contains_text_empty_session() {
        let conn = setup_test_db();
        assert!(!session_contains_text_from_conn(
            &conn,
            "ses_missing",
            "anything"
        ));
    }

    #[test]
    fn test_timestamp_conversion() {
        let conn = setup_test_db();
        insert_session(
            &conn,
            "ses_ts",
            None,
            "/proj",
            "Timestamp test",
            1705314600000, // 2024-01-15 10:30:00 UTC
            1705314700000,
        );

        let info = get_session_info_from_conn(&conn, "ses_ts").unwrap();
        assert_eq!(info.started_at.timestamp_millis(), 1705314600000);
        assert_eq!(info.updated_at.timestamp_millis(), 1705314700000);
    }

    #[test]
    fn test_zero_updated_at_falls_back_to_started() {
        let conn = setup_test_db();
        insert_session(&conn, "ses_zero", None, "/proj", "Zero", 1705314600000, 0);

        let info = get_session_info_from_conn(&conn, "ses_zero").unwrap();
        assert_eq!(info.started_at, info.updated_at);
    }

    // === session_edited_file_from_conn tests ===

    #[test]
    fn test_session_edited_file_from_conn_write_tool() {
        let conn = setup_test_db();
        insert_message(&conn, "msg_001", "ses_001", "assistant", 1705314600000);
        insert_part(
            &conn,
            "prt_001",
            "msg_001",
            "ses_001",
            1705314600000,
            r#"{"type":"tool","tool":"write","state":{"status":"completed","input":{"filePath":"/home/user/src/main.rs","content":"fn main() {}"}}}"#,
        );

        assert!(session_edited_file_from_conn(
            &conn,
            "ses_001",
            "/home/user/src/main.rs"
        ));
        assert!(!session_edited_file_from_conn(
            &conn,
            "ses_001",
            "/home/user/src/lib.rs"
        ));
    }

    #[test]
    fn test_session_edited_file_from_conn_edit_tool() {
        let conn = setup_test_db();
        insert_message(&conn, "msg_001", "ses_001", "assistant", 1705314600000);
        insert_part(
            &conn,
            "prt_001",
            "msg_001",
            "ses_001",
            1705314600000,
            r#"{"type":"tool","tool":"edit","state":{"status":"completed","input":{"filePath":"/home/user/src/lib.rs","oldString":"old","newString":"new"}}}"#,
        );

        assert!(session_edited_file_from_conn(
            &conn,
            "ses_001",
            "/home/user/src/lib.rs"
        ));
    }

    #[test]
    fn test_session_edited_file_from_conn_relative_path() {
        let conn = setup_test_db();
        insert_message(&conn, "msg_001", "ses_001", "assistant", 1705314600000);
        insert_part(
            &conn,
            "prt_001",
            "msg_001",
            "ses_001",
            1705314600000,
            r#"{"type":"tool","tool":"write","state":{"status":"completed","input":{"filePath":"src/main.rs","content":"fn main() {}"}}}"#,
        );

        assert!(session_edited_file_from_conn(
            &conn,
            "ses_001",
            "/home/user/project/src/main.rs"
        ));
        assert!(!session_edited_file_from_conn(
            &conn,
            "ses_001",
            "/home/user/project/src/lib.rs"
        ));
    }

    #[test]
    fn test_session_edited_file_from_conn_snake_case_field() {
        let conn = setup_test_db();
        insert_message(&conn, "msg_001", "ses_001", "assistant", 1705314600000);
        insert_part(
            &conn,
            "prt_001",
            "msg_001",
            "ses_001",
            1705314600000,
            r#"{"type":"tool","tool":"write","state":{"status":"completed","input":{"file_path":"/home/user/src/main.rs","content":"fn main() {}"}}}"#,
        );

        assert!(session_edited_file_from_conn(
            &conn,
            "ses_001",
            "/home/user/src/main.rs"
        ));
    }

    #[test]
    fn test_session_edited_file_from_conn_ignores_non_write() {
        let conn = setup_test_db();
        insert_message(&conn, "msg_001", "ses_001", "assistant", 1705314600000);
        insert_part(
            &conn,
            "prt_001",
            "msg_001",
            "ses_001",
            1705314600000,
            r#"{"type":"tool","tool":"read","state":{"status":"completed","input":{"filePath":"/home/user/src/main.rs"}}}"#,
        );

        assert!(!session_edited_file_from_conn(
            &conn,
            "ses_001",
            "/home/user/src/main.rs"
        ));
    }

    #[test]
    fn test_session_edited_file_from_conn_ignores_text_parts() {
        let conn = setup_test_db();
        insert_message(&conn, "msg_001", "ses_001", "user", 1705314600000);
        insert_part(
            &conn,
            "prt_001",
            "msg_001",
            "ses_001",
            1705314600000,
            r#"{"type":"text","text":"/home/user/src/main.rs"}"#,
        );

        assert!(!session_edited_file_from_conn(
            &conn,
            "ses_001",
            "/home/user/src/main.rs"
        ));
    }

    #[test]
    fn test_session_edited_file_from_conn_empty_session() {
        let conn = setup_test_db();
        assert!(!session_edited_file_from_conn(
            &conn,
            "ses_missing",
            "/home/user/src/main.rs"
        ));
    }

    // === session_matches_filters tests ===

    #[test]
    fn test_session_matches_filters_text_contains() {
        let conn = setup_test_db();
        insert_session(
            &conn,
            "ses_001",
            None,
            "/proj",
            "Test",
            1705314600000,
            1705314605000,
        );
        insert_message(&conn, "msg_001", "ses_001", "assistant", 1705314600000);
        insert_part(
            &conn,
            "prt_001",
            "msg_001",
            "ses_001",
            1705314600000,
            r#"{"type":"text","text":"Hello world, this is a test message"}"#,
        );

        let filters = vec![crate::session_detect::SessionFilter {
            depth: 0,
            criteria: vec![crate::session_detect::FilterCriterion::TextContains(
                "this is a test".to_string(),
            )],
        }];

        assert!(session_matches_filters_from_conn(
            &conn, "ses_001", &filters, 5, "/proj"
        ));

        // Non-matching text
        let filters_bad = vec![crate::session_detect::SessionFilter {
            depth: 0,
            criteria: vec![crate::session_detect::FilterCriterion::TextContains(
                "nonexistent text".to_string(),
            )],
        }];
        assert!(!session_matches_filters_from_conn(
            &conn,
            "ses_001",
            &filters_bad,
            5,
            "/proj"
        ));
    }

    #[test]
    fn test_session_matches_filters_text_contains_in_tool_output() {
        let conn = setup_test_db();
        insert_session(
            &conn,
            "ses_001",
            None,
            "/proj",
            "Test",
            1705314600000,
            1705314605000,
        );
        insert_message(&conn, "msg_001", "ses_001", "assistant", 1705314600000);
        // Only a tool part with output — no text parts at all
        insert_part(
            &conn,
            "prt_001",
            "msg_001",
            "ses_001",
            1705314600000,
            r#"{"type":"tool","tool":"bash","state":{"input":{"command":"tmux capture-pane -t %616 -p"},"output":"╭ vaab@wen ~/dev/charm/0k-charms ── 6m59 2026-03-16 15:05:24\n╰ $"}}"#,
        );

        // TextContains should find text inside tool output
        let filters = vec![crate::session_detect::SessionFilter {
            depth: 0,
            criteria: vec![crate::session_detect::FilterCriterion::TextContains(
                "vaab@wen ~/dev/charm/0k-charms".to_string(),
            )],
        }];
        assert!(session_matches_filters_from_conn(
            &conn, "ses_001", &filters, 5, "/proj"
        ));

        // Non-matching text still fails
        let filters_bad = vec![crate::session_detect::SessionFilter {
            depth: 0,
            criteria: vec![crate::session_detect::FilterCriterion::TextContains(
                "nonexistent output".to_string(),
            )],
        }];
        assert!(!session_matches_filters_from_conn(
            &conn,
            "ses_001",
            &filters_bad,
            5,
            "/proj"
        ));
    }

    #[test]
    fn test_session_matches_filters_tool_field_equals() {
        let conn = setup_test_db();
        insert_session(
            &conn,
            "ses_001",
            None,
            "/proj",
            "Test",
            1705314600000,
            1705314605000,
        );
        insert_message(&conn, "msg_001", "ses_001", "assistant", 1705314600000);
        insert_part(
            &conn,
            "prt_001",
            "msg_001",
            "ses_001",
            1705314600000,
            r#"{"type":"tool","tool":"grep","state":{"input":{"pattern":"foo","path":"src/"},"output":"found"}}"#,
        );

        let filters = vec![crate::session_detect::SessionFilter {
            depth: 0,
            criteria: vec![crate::session_detect::FilterCriterion::ToolFieldEquals {
                tool_name: "grep".to_string(),
                field: "pattern".to_string(),
                value: "foo".to_string(),
            }],
        }];

        assert!(session_matches_filters_from_conn(
            &conn, "ses_001", &filters, 5, "/proj"
        ));

        // Wrong tool name
        let filters_bad = vec![crate::session_detect::SessionFilter {
            depth: 0,
            criteria: vec![crate::session_detect::FilterCriterion::ToolFieldEquals {
                tool_name: "read".to_string(),
                field: "pattern".to_string(),
                value: "foo".to_string(),
            }],
        }];
        assert!(!session_matches_filters_from_conn(
            &conn,
            "ses_001",
            &filters_bad,
            5,
            "/proj"
        ));
    }

    #[test]
    fn test_session_matches_filters_tool_field_prefix_match() {
        let conn = setup_test_db();
        insert_session(
            &conn,
            "ses_001",
            None,
            "/proj",
            "Test",
            1705314600000,
            1705314605000,
        );
        insert_message(&conn, "msg_001", "ses_001", "assistant", 1705314600000);
        insert_part(
            &conn,
            "prt_001",
            "msg_001",
            "ses_001",
            1705314600000,
            r#"{"type":"tool","tool":"interactive_bash","state":{"input":{"tmux_command":"send-keys -t %581 \"Rathole server connection details for vps-03.0k.io: Server host: vps-03 -- Port: 2333 -- Token: abc123\""},"output":"(no output)"}}"#,
        );

        // Truncated prefix (as TUI would show) should match
        let filters = vec![crate::session_detect::SessionFilter {
            depth: 0,
            criteria: vec![crate::session_detect::FilterCriterion::ToolFieldEquals {
                tool_name: "interactive_bash".to_string(),
                field: "tmux_command".to_string(),
                value:
                    "send-keys -t %581 \"Rathole server connection details for vps-03.0k.io: Server"
                        .to_string(),
            }],
        }];
        assert!(session_matches_filters_from_conn(
            &conn, "ses_001", &filters, 5, "/proj"
        ));

        // Exact match still works
        let filters_exact = vec![crate::session_detect::SessionFilter {
            depth: 0,
            criteria: vec![crate::session_detect::FilterCriterion::ToolFieldEquals {
                tool_name: "interactive_bash".to_string(),
                field: "tmux_command".to_string(),
                value: "send-keys -t %581 \"Rathole server connection details for vps-03.0k.io: Server host: vps-03 -- Port: 2333 -- Token: abc123\"".to_string(),
            }],
        }];
        assert!(session_matches_filters_from_conn(
            &conn,
            "ses_001",
            &filters_exact,
            5,
            "/proj"
        ));

        // Wrong prefix should not match
        let filters_bad = vec![crate::session_detect::SessionFilter {
            depth: 0,
            criteria: vec![crate::session_detect::FilterCriterion::ToolFieldEquals {
                tool_name: "interactive_bash".to_string(),
                field: "tmux_command".to_string(),
                value: "send-keys -t %999 \"Something else".to_string(),
            }],
        }];
        assert!(!session_matches_filters_from_conn(
            &conn,
            "ses_001",
            &filters_bad,
            5,
            "/proj"
        ));
    }

    #[test]
    fn test_session_matches_filters_combined_criteria() {
        let conn = setup_test_db();
        insert_session(
            &conn,
            "ses_001",
            None,
            "/proj",
            "Test",
            1705314600000,
            1705314605000,
        );
        insert_message(&conn, "msg_001", "ses_001", "assistant", 1705314600000);
        insert_part(
            &conn,
            "prt_001",
            "msg_001",
            "ses_001",
            1705314600000,
            r#"{"type":"text","text":"Implementing the new feature"}"#,
        );
        insert_part(
            &conn,
            "prt_002",
            "msg_001",
            "ses_001",
            1705314600100,
            r#"{"type":"tool","tool":"read","state":{"input":{"filePath":"/proj/src/main.rs"},"output":"fn main(){}"}}"#,
        );

        // Both criteria must match in the same message.
        // The filter value is relative — it gets resolved against project_dir "/proj".
        let filters = vec![crate::session_detect::SessionFilter {
            depth: 0,
            criteria: vec![
                crate::session_detect::FilterCriterion::TextContains("new feature".to_string()),
                crate::session_detect::FilterCriterion::ToolFieldEquals {
                    tool_name: "read".to_string(),
                    field: "filePath".to_string(),
                    value: "src/main.rs".to_string(),
                },
            ],
        }];

        assert!(session_matches_filters_from_conn(
            &conn, "ses_001", &filters, 5, "/proj"
        ));
    }

    #[test]
    fn test_session_matches_filters_empty_session() {
        let conn = setup_test_db();
        let filters = vec![crate::session_detect::SessionFilter {
            depth: 0,
            criteria: vec![crate::session_detect::FilterCriterion::TextContains(
                "anything".to_string(),
            )],
        }];
        assert!(!session_matches_filters_from_conn(
            &conn,
            "ses_missing",
            &filters,
            5,
            ""
        ));
    }

    #[test]
    fn test_session_matches_filters_window_limit() {
        let conn = setup_test_db();
        insert_session(
            &conn,
            "ses_001",
            None,
            "/proj",
            "Test",
            1705314600000,
            1705314605000,
        );
        insert_message(&conn, "msg_001", "ses_001", "user", 1705314600000);
        insert_message(&conn, "msg_002", "ses_001", "assistant", 1705314601000);
        insert_message(&conn, "msg_003", "ses_001", "user", 1705314602000);

        insert_part(
            &conn,
            "prt_001",
            "msg_001",
            "ses_001",
            1705314600000,
            r#"{"type":"text","text":"old message content"}"#,
        );
        insert_part(
            &conn,
            "prt_003",
            "msg_003",
            "ses_001",
            1705314602000,
            r#"{"type":"text","text":"latest message content"}"#,
        );

        let filters = vec![crate::session_detect::SessionFilter {
            depth: 0,
            criteria: vec![crate::session_detect::FilterCriterion::TextContains(
                "old message".to_string(),
            )],
        }];

        // Window of 1 should NOT find old message (only msg_003 is in window)
        assert!(!session_matches_filters_from_conn(
            &conn, "ses_001", &filters, 1, "/proj"
        ));

        // Window of 3 SHOULD find it
        assert!(session_matches_filters_from_conn(
            &conn, "ses_001", &filters, 3, "/proj"
        ));
    }

    // ---- delete_session_rows tests ----------------------------------

    fn count(conn: &Connection, table: &str, predicate: &str, arg: &str) -> usize {
        let sql = format!("SELECT COUNT(*) FROM {} WHERE {}", table, predicate);
        conn.query_row(&sql, [arg], |row| row.get::<_, i64>(0))
            .unwrap() as usize
    }

    #[test]
    fn delete_session_rows_wipes_all_three_tables() {
        let mut conn = setup_test_db();
        insert_session(&conn, "ses_a", None, "/p", "A", 1, 2);
        insert_session(&conn, "ses_b", None, "/p", "B", 3, 4);
        insert_message(&conn, "msg_a1", "ses_a", "user", 10);
        insert_message(&conn, "msg_a2", "ses_a", "assistant", 11);
        insert_message(&conn, "msg_b1", "ses_b", "user", 12);
        insert_part(&conn, "prt_a1", "msg_a1", "ses_a", 20, r#"{"type":"text"}"#);
        insert_part(&conn, "prt_a2", "msg_a2", "ses_a", 21, r#"{"type":"text"}"#);
        insert_part(&conn, "prt_b1", "msg_b1", "ses_b", 22, r#"{"type":"text"}"#);

        let report = delete_session_rows(&mut conn, "ses_a").unwrap();
        assert_eq!(report.session_rows, 1);
        assert_eq!(report.message_rows, 2);
        assert_eq!(report.part_rows, 2);
        assert_eq!(report.total(), 5);

        // ses_a fully wiped
        assert_eq!(count(&conn, "session", "id = ?", "ses_a"), 0);
        assert_eq!(count(&conn, "message", "session_id = ?", "ses_a"), 0);
        assert_eq!(count(&conn, "part", "session_id = ?", "ses_a"), 0);

        // ses_b untouched
        assert_eq!(count(&conn, "session", "id = ?", "ses_b"), 1);
        assert_eq!(count(&conn, "message", "session_id = ?", "ses_b"), 1);
        assert_eq!(count(&conn, "part", "session_id = ?", "ses_b"), 1);
    }

    #[test]
    fn delete_session_rows_is_noop_on_missing_id() {
        let mut conn = setup_test_db();
        insert_session(&conn, "ses_a", None, "/p", "A", 1, 2);
        let report = delete_session_rows(&mut conn, "ses_ghost").unwrap();
        assert_eq!(report.total(), 0);
        // ses_a still there
        assert_eq!(count(&conn, "session", "id = ?", "ses_a"), 1);
    }

    #[test]
    fn delete_session_rows_does_not_touch_permission_table() {
        // The `permission` table is per-project, not per-session.
        // ai-audit's delete must NEVER touch it.  We add the table
        // manually here (production schema), populate it, run delete,
        // and assert byte-identical contents.
        let mut conn = Connection::open_in_memory().unwrap();
        create_schema(&conn).unwrap();
        conn.execute_batch(
            "CREATE TABLE permission (
                project_id TEXT PRIMARY KEY,
                data TEXT NOT NULL
            );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO permission (project_id, data) VALUES (?, ?)",
            ["proj_1", "{\"allow\":[\"*\"]}"],
        )
        .unwrap();
        insert_session(&conn, "ses_a", None, "/p", "A", 1, 2);
        insert_message(&conn, "msg_a", "ses_a", "user", 10);
        insert_part(&conn, "prt_a", "msg_a", "ses_a", 20, r#"{"type":"text"}"#);

        let before: String = conn
            .query_row(
                "SELECT data FROM permission WHERE project_id = ?",
                ["proj_1"],
                |row| row.get(0),
            )
            .unwrap();
        delete_session_rows(&mut conn, "ses_a").unwrap();
        let after: String = conn
            .query_row(
                "SELECT data FROM permission WHERE project_id = ?",
                ["proj_1"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn delete_session_rows_rolls_back_on_failure() {
        // Verify transactional integrity: if we corrupt the transaction
        // mid-flight (by manually preparing a bad statement), the
        // partial state should not leak.  Easiest reproducible check:
        // delete twice in a row — second call should see zero rows
        // and not error.
        let mut conn = setup_test_db();
        insert_session(&conn, "ses_a", None, "/p", "A", 1, 2);
        insert_message(&conn, "msg_a", "ses_a", "user", 10);
        insert_part(&conn, "prt_a", "msg_a", "ses_a", 20, r#"{"type":"text"}"#);
        let report1 = delete_session_rows(&mut conn, "ses_a").unwrap();
        assert_eq!(report1.total(), 3);
        let report2 = delete_session_rows(&mut conn, "ses_a").unwrap();
        assert_eq!(report2.total(), 0);
    }

    #[test]
    fn open_db_rw_at_can_write_open_db_at_cannot() {
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("test.db");
        // bootstrap schema with a writable connection (and switch
        // the DB into WAL mode so the read-only PRAGMA in open_db_at
        // is a no-op rather than a write).
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
            create_schema(&conn).unwrap();
            insert_session(&conn, "ses_a", None, "/p", "A", 1, 2);
        }
        // read-only refuses writes
        let ro = open_db_at(&db_path).unwrap();
        let err = ro
            .execute("DELETE FROM session WHERE id = ?", ["ses_a"])
            .unwrap_err();
        assert!(
            err.to_string().to_lowercase().contains("readonly")
                || err.to_string().to_lowercase().contains("read-only")
                || err.to_string().to_lowercase().contains("read only"),
            "expected read-only error, got: {}",
            err
        );
        // read-write succeeds
        let mut rw = open_db_rw_at(&db_path).unwrap();
        let report = delete_session_rows(&mut rw, "ses_a").unwrap();
        assert_eq!(report.session_rows, 1);
    }
}

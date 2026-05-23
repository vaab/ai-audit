//! `ai-audit session delete <session-id | --all | --ids-file>` —
//! wipe one or more sessions across every storage location ai-audit reads.
//!
//! Spec: `doc/admin.org § ai-audit / session delete / wipe sessions
//! across all storage`.
//!
//! ## Pipeline
//!
//! 1. Validate conflict rules (positional + filter, ids-file + others).
//! 2. Resolve the target set:
//!    * Positional `<session-id>` → singleton.
//!    * `--ids-file <PATH>` → parse NUL-separated or NDJSON.
//!    * Filter flags + `--all` → `session_filter::list_filtered(...)`.
//! 3. Self-deletion guard against `*_SESSION_ID` env vars and the
//!    tmux-fingerprint detector.
//! 4. Iterate sessions: detect provider, dispatch to the right
//!    per-harness `delete_session(...)`.  Accumulate per-session
//!    outcomes — per-session failures DO NOT abort the batch.
//! 5. Render output (human / JSON / NUL) and exit 1 iff at least
//!    one session failed.

use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use crate::cli::def::{SessionDeleteArgs, StaticStatusArg};
use crate::provider::{detect_provider, Provider};
use crate::session_filter::{canonicalize_filter_path, list_filtered, SessionFilter};
use crate::OutputFormat;

/// Entry point invoked by `session::run_delete`.
pub fn run(args: SessionDeleteArgs) -> Result<()> {
    validate_arg_conflicts(&args)?;
    let targets = resolve_targets(&args)?;
    guard_self_deletion(&targets)?;
    let format = args.output.format();
    let outcomes = process_targets(&args, &targets);
    render(&outcomes, format, args.dry_run)?;
    if outcomes
        .iter()
        .any(|o| matches!(o.result, Outcome::Failed { .. }))
    {
        bail!("one or more session deletions failed");
    }
    Ok(())
}

// =============================================================================
// Section 1 — Arg validation
// =============================================================================

fn validate_arg_conflicts(args: &SessionDeleteArgs) -> Result<()> {
    let has_filter = args.session_type.is_some()
        || args.session_id.is_some()
        || !args.search.is_empty()
        || args.timespan.is_some()
        || args.last_message_in.is_some()
        || args.project.is_some()
        || args.file.is_some()
        || args.status.is_some();

    // ArgGroup already enforces "exactly one of <session>, --all,
    // --ids-file" at parse time, but the filter-flag combinations
    // need runtime checks (clap can't express "filter requires --all
    // and forbids positional").
    if args.session.is_some() && has_filter {
        bail!(
            "<SESSION-ID> cannot be combined with --type, --search, --project, \
             --file, --timespan, --last-message-in, --status, or --session-id. \
             Use either a positional ID or filters, not both."
        );
    }
    if args.ids_file.is_some() && has_filter {
        bail!(
            "--ids-file cannot be combined with filter flags (--type, --search, \
             etc.).  Filters resolve session IDs; supplying both ids and \
             filters is ambiguous."
        );
    }
    if args.all && !has_filter {
        // --all without any filter would match every session ever
        // recorded.  Refuse: the spec says "the filter is the
        // contract"; --all with no filter is not a contract.
        bail!(
            "--all requires at least one filter flag (e.g. --type, --search, \
             --timespan).  An unfiltered --all would delete every recorded \
             session — refuse."
        );
    }
    Ok(())
}

// =============================================================================
// Section 2 — Target resolution
// =============================================================================

#[derive(Debug, Clone)]
struct Target {
    session_id: String,
    provider: Provider,
}

fn resolve_targets(args: &SessionDeleteArgs) -> Result<Vec<Target>> {
    // Branch 1: positional <session-id> → single target.
    if let Some(id) = args.session.clone() {
        let provider = detect_provider(&id)?;
        return Ok(vec![Target {
            session_id: id,
            provider,
        }]);
    }

    // Branch 2: --ids-file → parse NUL or NDJSON.
    if let Some(path) = args.ids_file.as_deref() {
        let ids = load_ids_file(path)?;
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            let provider = detect_provider(&id)?;
            out.push(Target {
                session_id: id,
                provider,
            });
        }
        return Ok(out);
    }

    // Branch 3: --all + filter flags → list_filtered.
    let filter = build_filter(args)?;
    let enriched = list_filtered(&filter)?;
    let targets = enriched
        .into_iter()
        .map(|e| Target {
            session_id: e.base.session_id,
            provider: e.base.provider,
        })
        .collect();
    Ok(targets)
}

fn build_filter(args: &SessionDeleteArgs) -> Result<SessionFilter> {
    let static_enrich = args
        .status
        .is_some()
        .then(crate::opencode::enrich::make_static_enricher);
    let static_predicate = args.status.as_ref().map(|statuses| {
        let want: std::collections::HashSet<crate::opencode::status::StaticStatus> =
            statuses.iter().copied().map(map_static_status).collect();
        Box::new(move |session: &crate::session_filter::EnrichedSession| {
            crate::opencode::enrich::extract_static(session)
                .map(|ext| want.contains(&ext.status))
                .unwrap_or(false)
        }) as crate::session_filter::SessionPredicate
    });

    Ok(SessionFilter {
        session_type: args.session_type,
        session_id: args.session_id.clone(),
        project: args.project.as_deref().map(canonicalize_filter_path),
        search: args.search.clone(),
        file: args.file.clone(),
        timespan: parse_timespan(args.timespan.as_deref())?,
        last_message_in: parse_timespan(args.last_message_in.as_deref())?,
        all: args.all,
        children_of: None,
        static_enrich,
        static_predicate,
        live_enrich: None,
        live_predicate: None,
    })
}

fn map_static_status(status: StaticStatusArg) -> crate::opencode::status::StaticStatus {
    use crate::opencode::status::StaticStatus;
    match status {
        StaticStatusArg::Completed => StaticStatus::Completed,
        StaticStatusArg::UserPending => StaticStatus::UserPending,
        StaticStatusArg::AssistantEmpty => StaticStatus::AssistantEmpty,
        StaticStatusArg::AssistantPartial => StaticStatus::AssistantPartial,
        StaticStatusArg::AssistantToolStuck => StaticStatus::AssistantToolStuck,
    }
}

fn parse_timespan(input: Option<&str>) -> Result<Option<(i64, i64)>> {
    input
        .map(|value| {
            kal_time::parse_timespan(value)
                .map(|(start, end)| (start.timestamp(), end.timestamp()))
                .map_err(|error| anyhow::anyhow!("Failed to parse timespan '{}': {}", value, error))
        })
        .transpose()
}

// =============================================================================
// Section 3 — ids-file parsing (NUL + NDJSON auto-detect)
// =============================================================================

/// Read session IDs from a file (`-` for stdin).
///
/// Auto-detects the format: if the first non-whitespace byte is
/// `{`, parse as newline-delimited JSON (`session_id` or `id` field
/// per object).  Otherwise treat as NUL-separated plain IDs
/// (matching the convention used by `activity get --categs-file`).
///
/// Empty input yields an empty vec (not an error — the caller can
/// decide whether that's a problem).
fn load_ids_file(path: &Path) -> Result<Vec<String>> {
    let mut buf = Vec::new();
    if path == Path::new("-") {
        io::stdin()
            .lock()
            .read_to_end(&mut buf)
            .context("Failed to read --ids-file from stdin")?;
    } else {
        std::fs::File::open(path)
            .with_context(|| format!("Failed to open --ids-file {}", path.display()))?
            .read_to_end(&mut buf)
            .with_context(|| format!("Failed to read --ids-file {}", path.display()))?;
    }
    if buf.is_empty() {
        return Ok(Vec::new());
    }

    // Format detection — peek the first non-whitespace byte.
    let first = buf.iter().find(|&&b| !b.is_ascii_whitespace()).copied();
    match first {
        Some(b'{') => parse_ndjson(&buf, path),
        _ => parse_nul(&buf, path),
    }
}

fn parse_nul(buf: &[u8], path: &Path) -> Result<Vec<String>> {
    let mut buf = buf.to_vec();
    if buf.last() == Some(&0) {
        buf.pop(); // tolerate single trailing NUL
    }
    let mut out = Vec::new();
    for (idx, chunk) in buf.split(|&b| b == 0).enumerate() {
        if chunk.is_empty() {
            bail!(
                "Empty session ID at NUL-separated position {} in {}",
                idx,
                path.display()
            );
        }
        let id = std::str::from_utf8(chunk)
            .with_context(|| {
                format!(
                    "Non-UTF8 session ID at NUL-separated position {} in {}",
                    idx,
                    path.display()
                )
            })?
            .trim()
            .to_string();
        if id.is_empty() {
            bail!(
                "Whitespace-only session ID at NUL-separated position {} in {}",
                idx,
                path.display()
            );
        }
        out.push(id);
    }
    Ok(out)
}

fn parse_ndjson(buf: &[u8], path: &Path) -> Result<Vec<String>> {
    let text = std::str::from_utf8(buf)
        .with_context(|| format!("Non-UTF8 NDJSON input in {}", path.display()))?;
    let mut out = Vec::new();
    for (lineno, line) in text.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let value: serde_json::Value = serde_json::from_str(trimmed).with_context(|| {
            format!(
                "Failed to parse NDJSON line {} in {}: not valid JSON",
                lineno + 1,
                path.display()
            )
        })?;
        // Accept either `session_id` (our convention) or `id` (raw
        // opencode-style).  Anything else → loud error.
        let id = value
            .get("session_id")
            .and_then(|v| v.as_str())
            .or_else(|| value.get("id").and_then(|v| v.as_str()))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "NDJSON line {} in {} has no 'session_id' or 'id' field",
                    lineno + 1,
                    path.display()
                )
            })?
            .to_string();
        out.push(id);
    }
    Ok(out)
}

// =============================================================================
// Section 4 — Self-deletion guard
// =============================================================================

fn guard_self_deletion(targets: &[Target]) -> Result<()> {
    let env_ids: Vec<(String, &'static str)> =
        ["OPENCODE_SESSION_ID", "CLAUDE_SESSION_ID", "PI_SESSION_ID"]
            .iter()
            .filter_map(|var| std::env::var(var).ok().map(|id| (id, *var)))
            .collect();

    // Auto-detected session (tmux fingerprint).  Best-effort: errors
    // here are NOT fatal — we don't want a busted detector to block
    // a delete.  If detection fails we silently skip the second
    // check; the env-var check still applies.
    let detected_id = crate::session_detect::detect_current_session()
        .ok()
        .map(|d| d.session_id);

    for target in targets {
        for (env_id, var_name) in &env_ids {
            if target.session_id == *env_id {
                bail!(
                    "refuse to delete current session {} (${} is set to this ID). \
                     Run the delete from a different shell or unset ${}.",
                    target.session_id,
                    var_name,
                    var_name
                );
            }
        }
        if detected_id.as_deref() == Some(target.session_id.as_str()) {
            bail!(
                "refuse to delete session {} — it appears to be the current \
                 session in this tmux pane (auto-detected). Run the delete from \
                 a different pane.",
                target.session_id
            );
        }
    }
    Ok(())
}

// =============================================================================
// Section 5 — Per-session processing
// =============================================================================

#[derive(Debug, Clone)]
struct SessionOutcome {
    session_id: String,
    provider: Provider,
    result: Outcome,
}

#[derive(Debug, Clone)]
enum Outcome {
    Deleted {
        db_rows: usize,
        file_paths: Vec<PathBuf>,
    },
    WouldDelete {
        db_rows: usize,
        file_paths: Vec<PathBuf>,
    },
    Failed(String),
}

fn process_targets(args: &SessionDeleteArgs, targets: &[Target]) -> Vec<SessionOutcome> {
    let mut outcomes = Vec::with_capacity(targets.len());
    for target in targets {
        let result = match target.provider {
            Provider::ClaudeCode => process_claudecode(target, args.dry_run),
            Provider::OpenCode => process_opencode(target, args.dry_run),
            Provider::Pi => process_pi(target, args.cascade, args.dry_run),
        };
        outcomes.push(SessionOutcome {
            session_id: target.session_id.clone(),
            provider: target.provider,
            result,
        });
    }
    outcomes
}

fn process_claudecode(target: &Target, dry_run: bool) -> Outcome {
    match crate::claudecode::delete_session(&target.session_id, dry_run) {
        Ok(report) => {
            if dry_run {
                Outcome::WouldDelete {
                    db_rows: 0,
                    file_paths: report.paths,
                }
            } else {
                Outcome::Deleted {
                    db_rows: 0,
                    file_paths: report.paths,
                }
            }
        }
        Err(e) => Outcome::Failed(e.to_string()),
    }
}

fn process_opencode(target: &Target, dry_run: bool) -> Outcome {
    match crate::opencode::delete_session(&target.session_id, dry_run) {
        Ok(report) => {
            let db_rows = report.db.total();
            if dry_run {
                Outcome::WouldDelete {
                    db_rows,
                    file_paths: report.paths,
                }
            } else {
                Outcome::Deleted {
                    db_rows,
                    file_paths: report.paths,
                }
            }
        }
        Err(e) => Outcome::Failed(e.to_string()),
    }
}

fn process_pi(target: &Target, cascade: bool, dry_run: bool) -> Outcome {
    match crate::pi::delete_session(&target.session_id, cascade, dry_run) {
        Ok(report) => {
            if dry_run {
                Outcome::WouldDelete {
                    db_rows: 0,
                    file_paths: report.paths,
                }
            } else {
                Outcome::Deleted {
                    db_rows: 0,
                    file_paths: report.paths,
                }
            }
        }
        // Cascade refusal surfaces here as an error.  This is the
        // user-facing message describing the child IDs.
        Err(e) => Outcome::Failed(e.to_string()),
    }
}

// =============================================================================
// Section 6 — Output rendering
// =============================================================================

fn render(outcomes: &[SessionOutcome], format: OutputFormat, dry_run: bool) -> Result<()> {
    match format {
        OutputFormat::Human => render_human(outcomes, dry_run),
        OutputFormat::Json => render_json(outcomes),
        OutputFormat::Nul => render_nul(outcomes),
    }
}

fn render_human(outcomes: &[SessionOutcome], dry_run: bool) -> Result<()> {
    let mut deleted = 0usize;
    let mut would = 0usize;
    let mut failed = 0usize;

    for outcome in outcomes {
        match &outcome.result {
            Outcome::Deleted {
                db_rows,
                file_paths,
            } => {
                deleted += 1;
                println!(
                    "deleted {} ({}, {} db row{} + {} file{})",
                    outcome.session_id,
                    outcome.provider.as_str(),
                    db_rows,
                    if *db_rows == 1 { "" } else { "s" },
                    file_paths.len(),
                    if file_paths.len() == 1 { "" } else { "s" },
                );
            }
            Outcome::WouldDelete {
                db_rows,
                file_paths,
            } => {
                would += 1;
                println!(
                    "would-delete {} ({}, {} db row{} + {} file{})",
                    outcome.session_id,
                    outcome.provider.as_str(),
                    db_rows,
                    if *db_rows == 1 { "" } else { "s" },
                    file_paths.len(),
                    if file_paths.len() == 1 { "" } else { "s" },
                );
            }
            Outcome::Failed(error) => {
                failed += 1;
                println!(
                    "failed {} ({}): {}",
                    outcome.session_id,
                    outcome.provider.as_str(),
                    error
                );
            }
        }
    }
    if dry_run {
        println!("Would-delete: {}. Skipped: 0. Failed: {}.", would, failed);
    } else {
        println!("Deleted: {}. Skipped: 0. Failed: {}.", deleted, failed);
    }
    Ok(())
}

#[derive(Serialize)]
struct JsonOutcome<'a> {
    session_id: &'a str,
    harness: &'a str,
    result: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    wiped: Option<JsonWiped>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'a str>,
}

#[derive(Serialize)]
struct JsonWiped {
    db_rows: usize,
    file_paths: Vec<String>,
}

fn render_json(outcomes: &[SessionOutcome]) -> Result<()> {
    let stdout = io::stdout();
    let mut handle = stdout.lock();
    for outcome in outcomes {
        let (result_str, wiped, error) = match &outcome.result {
            Outcome::Deleted {
                db_rows,
                file_paths,
            } => (
                "deleted",
                Some(JsonWiped {
                    db_rows: *db_rows,
                    file_paths: file_paths.iter().map(path_to_string).collect(),
                }),
                None,
            ),
            Outcome::WouldDelete {
                db_rows,
                file_paths,
            } => (
                "would-delete",
                Some(JsonWiped {
                    db_rows: *db_rows,
                    file_paths: file_paths.iter().map(path_to_string).collect(),
                }),
                None,
            ),
            Outcome::Failed(error) => ("failed", None, Some(error.as_str())),
        };

        let payload = JsonOutcome {
            session_id: &outcome.session_id,
            harness: outcome.provider.as_str(),
            result: result_str,
            wiped,
            error,
        };
        let line = serde_json::to_string(&payload)?;
        writeln!(handle, "{}", line)?;
    }
    Ok(())
}

fn render_nul(outcomes: &[SessionOutcome]) -> Result<()> {
    let stdout = io::stdout();
    let mut handle = stdout.lock();
    for outcome in outcomes {
        let (result_str, count, error) = match &outcome.result {
            Outcome::Deleted {
                db_rows,
                file_paths,
            } => ("deleted", db_rows + file_paths.len(), String::new()),
            Outcome::WouldDelete {
                db_rows,
                file_paths,
            } => ("would-delete", db_rows + file_paths.len(), String::new()),
            Outcome::Failed(error) => ("failed", 0, error.clone()),
        };
        // Fixed-field record: <id>\0<harness>\0<result>\0<count>\0<err>\0
        handle.write_all(outcome.session_id.as_bytes())?;
        handle.write_all(&[0])?;
        handle.write_all(outcome.provider.as_str().as_bytes())?;
        handle.write_all(&[0])?;
        handle.write_all(result_str.as_bytes())?;
        handle.write_all(&[0])?;
        handle.write_all(count.to_string().as_bytes())?;
        handle.write_all(&[0])?;
        handle.write_all(error.as_bytes())?;
        handle.write_all(&[0])?;
    }
    Ok(())
}

// `&PathBuf` rather than `&Path` is intentional: it matches the
// iterator yield type from `Vec<PathBuf>::iter()`, avoiding a closure
// or explicit `.as_path()` conversion at every call site.
#[allow(clippy::ptr_arg)]
fn path_to_string(p: &PathBuf) -> String {
    p.to_string_lossy().into_owned()
}

// =============================================================================
// Section 7 — Unit tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    // ---- load_ids_file -----------------------------------------

    fn make_temp_file(content: &[u8]) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(content).unwrap();
        f.flush().unwrap();
        f
    }

    #[test]
    fn load_ids_file_nul_separated_works() {
        let f = make_temp_file(b"ses_aaa\0ses_bbb\0ses_ccc\0");
        let ids = load_ids_file(f.path()).unwrap();
        assert_eq!(ids, vec!["ses_aaa", "ses_bbb", "ses_ccc"]);
    }

    #[test]
    fn load_ids_file_nul_separated_without_trailing_nul() {
        let f = make_temp_file(b"ses_aaa\0ses_bbb");
        let ids = load_ids_file(f.path()).unwrap();
        assert_eq!(ids, vec!["ses_aaa", "ses_bbb"]);
    }

    #[test]
    fn load_ids_file_ndjson_with_session_id_field() {
        let body = b"{\"session_id\":\"ses_aaa\",\"foo\":1}\n{\"session_id\":\"ses_bbb\"}\n";
        let f = make_temp_file(body);
        let ids = load_ids_file(f.path()).unwrap();
        assert_eq!(ids, vec!["ses_aaa", "ses_bbb"]);
    }

    #[test]
    fn load_ids_file_ndjson_with_id_field() {
        let body = b"{\"id\":\"ses_aaa\"}\n{\"id\":\"ses_bbb\"}\n";
        let f = make_temp_file(body);
        let ids = load_ids_file(f.path()).unwrap();
        assert_eq!(ids, vec!["ses_aaa", "ses_bbb"]);
    }

    #[test]
    fn load_ids_file_ndjson_prefers_session_id_over_id() {
        let body = b"{\"session_id\":\"correct\",\"id\":\"wrong\"}\n";
        let f = make_temp_file(body);
        let ids = load_ids_file(f.path()).unwrap();
        assert_eq!(ids, vec!["correct"]);
    }

    #[test]
    fn load_ids_file_ndjson_skips_blank_lines() {
        let body = b"\n{\"session_id\":\"a\"}\n\n  \n{\"session_id\":\"b\"}\n";
        let f = make_temp_file(body);
        let ids = load_ids_file(f.path()).unwrap();
        assert_eq!(ids, vec!["a", "b"]);
    }

    #[test]
    fn load_ids_file_ndjson_errors_when_field_missing() {
        let body = b"{\"foo\":\"bar\"}\n";
        let f = make_temp_file(body);
        let err = load_ids_file(f.path()).unwrap_err();
        assert!(err.to_string().contains("session_id"));
    }

    #[test]
    fn load_ids_file_empty_input_returns_empty() {
        let f = make_temp_file(b"");
        let ids = load_ids_file(f.path()).unwrap();
        assert!(ids.is_empty());
    }

    #[test]
    fn load_ids_file_nul_separated_rejects_empty_id() {
        let f = make_temp_file(b"ses_a\0\0ses_b\0");
        let err = load_ids_file(f.path()).unwrap_err();
        assert!(err.to_string().contains("Empty session ID"));
    }

    // ---- validate_arg_conflicts --------------------------------

    fn args_with_session(id: &str) -> SessionDeleteArgs {
        SessionDeleteArgs {
            session: Some(id.to_string()),
            session_type: None,
            session_id: None,
            search: vec![],
            timespan: None,
            last_message_in: None,
            project: None,
            file: None,
            status: None,
            ids_file: None,
            all: false,
            cascade: false,
            dry_run: false,
            output: crate::cli::def::OutputOpts {
                nul: false,
                json: false,
            },
        }
    }

    #[test]
    fn arg_conflict_positional_with_filter_is_rejected() {
        let mut args = args_with_session("ses_aaa");
        args.search = vec!["hi".to_string()];
        let err = validate_arg_conflicts(&args).unwrap_err();
        assert!(err.to_string().contains("<SESSION-ID> cannot be combined"));
    }

    #[test]
    fn arg_conflict_ids_file_with_filter_is_rejected() {
        let mut args = args_with_session("ignored");
        args.session = None;
        args.ids_file = Some(PathBuf::from("/tmp/x"));
        args.search = vec!["hi".to_string()];
        let err = validate_arg_conflicts(&args).unwrap_err();
        assert!(err.to_string().contains("--ids-file cannot be combined"));
    }

    #[test]
    fn arg_conflict_all_without_filter_is_rejected() {
        let mut args = args_with_session("ignored");
        args.session = None;
        args.all = true;
        let err = validate_arg_conflicts(&args).unwrap_err();
        assert!(err.to_string().contains("--all requires"));
    }

    #[test]
    fn arg_conflict_positional_alone_ok() {
        let args = args_with_session("ses_aaa");
        validate_arg_conflicts(&args).unwrap();
    }

    #[test]
    fn arg_conflict_all_with_filter_ok() {
        let mut args = args_with_session("ignored");
        args.session = None;
        args.all = true;
        args.search = vec!["hi".to_string()];
        validate_arg_conflicts(&args).unwrap();
    }

    // ---- guard_self_deletion -----------------------------------

    #[test]
    fn guard_self_deletion_passes_when_no_env_vars_set() {
        // Save existing env state.
        let save = ["OPENCODE_SESSION_ID", "CLAUDE_SESSION_ID", "PI_SESSION_ID"]
            .iter()
            .map(|v| (v.to_string(), std::env::var(v).ok()))
            .collect::<Vec<_>>();
        for (var, _) in &save {
            unsafe { std::env::remove_var(var) };
        }

        let target = Target {
            session_id: "ses_aaa".to_string(),
            provider: Provider::OpenCode,
        };
        let result = guard_self_deletion(&[target]);

        // Restore.
        for (var, val) in &save {
            unsafe {
                match val {
                    Some(v) => std::env::set_var(var, v),
                    None => std::env::remove_var(var),
                }
            }
        }
        result.unwrap();
    }
}

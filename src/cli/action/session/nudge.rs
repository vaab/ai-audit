use anyhow::{anyhow, bail, Result};
use std::collections::HashMap;
use std::path::Path;

use crate::cli::def::{SessionNudgeArgs, SessionType, StaticStatusArg};
use crate::config::Config;
use crate::opencode::db;
use crate::opencode::enrich::{extract_static, make_static_enricher, StaticExtension};
use crate::opencode::nudge::{execute_plan, NudgePlan, NudgeStrategy};
use crate::opencode::server_client::{
    compute_live, resolve_server_credentials, LiveStatus, ServerClient,
};
use crate::opencode::status::{
    count_prior_completed_assistant_siblings, fetch_last_user_message,
    fetch_user_message_before_last_assistant, StaticStatus,
};
use crate::provider::detect_provider;
use crate::session_filter::{canonicalize_filter_path, list_filtered, SessionFilter};

pub fn run(args: SessionNudgeArgs) -> Result<()> {
    if args.session.is_some() && args.all {
        bail!("SESSION_ID and --all are mutually exclusive")
    }
    if args.session.is_some()
        && (args.project.is_some()
            || !args.search.is_empty()
            || args.timespan.is_some()
            || args.last_message_in.is_some()
            || args.status.is_some())
    {
        bail!(
            "SESSION_ID cannot be combined with --project, --search, --timespan, \
             --last-message-in, or --status"
        )
    }
    if let Some(session_id) = args.session.as_deref() {
        match detect_provider(session_id)? {
            crate::provider::Provider::OpenCode => {}
            crate::provider::Provider::ClaudeCode | crate::provider::Provider::Pi => {
                return super::super::require_opencode_for("session nudge");
            }
        }
    }
    let config = Config::load()?;
    let explicit_static_statuses = args
        .status
        .as_ref()
        .map(|statuses| statuses.iter().copied().map(map_static_status).collect())
        .unwrap_or_else(StaticStatus::resumable_set);
    let sessions = list_filtered(&SessionFilter {
        session_type: Some(SessionType::OpenCode),
        session_id: args.session.clone(),
        project: args.project.as_deref().map(canonicalize_filter_path),
        search: args.search.clone(),
        file: None,
        timespan: parse_timespan(args.timespan.as_deref())?,
        last_message_in: parse_timespan(args.last_message_in.as_deref())?,
        all: args.all || args.session.is_some(),
        children_of: None,
        static_enrich: Some(make_static_enricher()),
        static_predicate: if args.session.is_some() && args.status.is_none() {
            None
        } else {
            Some(Box::new(move |session| {
                extract_static(session)
                    .map(|extension| explicit_static_statuses.contains(&extension.status))
                    .unwrap_or(false)
            }))
        },
        live_enrich: None,
        live_predicate: None,
    })?;
    if sessions.is_empty() {
        if args.session.is_some() {
            bail!("No matching resumable opencode session found")
        }
        println!("Nudged: 0. Skipped (already running): 0. Forced (was running): 0. Failed: 0.");
        return Ok(());
    }
    if args.session.is_some() && args.status.is_none() {
        let static_status = extract_static(&sessions[0])
            .map(|extension| extension.status)
            .ok_or_else(|| anyhow!("missing static status"))?;
        if !static_status.is_resumable() {
            bail!(
                "session {} has static status={}; not a candidate for nudging. Use --status to override.",
                sessions[0].base.session_id,
                static_status.as_str(),
            )
        }
    }

    let orphan_sessions = sessions
        .iter()
        .filter(|session| !Path::new(&session.base.project_dir).exists())
        .map(|session| session.base.session_id.clone())
        .collect::<Vec<_>>();
    if !orphan_sessions.is_empty() && !args.allow_revive_orphan_sessions {
        bail!(render_orphan_error(&orphan_sessions));
    }

    let creds = resolve_server_credentials(
        args.server_url.as_deref(),
        args.server_password.as_deref(),
        &config,
    );
    let client = ServerClient::new(creds.clone());
    let live_map = client.session_status()?;
    let Some(live_map) = live_map else {
        bail!(
            "error: opencode server at {} did not respond. Nudging requires a reachable server. Start opencode (or pass --server-url ...) and retry.",
            creds.url,
        )
    };

    let prepared = sessions
        .into_iter()
        .map(|session| {
            Ok(PreparedNudgeSession {
                session_id: session.base.session_id.clone(),
                static_extension: extract_static(&session)
                    .cloned()
                    .ok_or_else(|| anyhow!("missing static status"))?,
                live_status: compute_live(&session.base.session_id, Some(&live_map), false),
                project_dir: session.base.project_dir,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    // Resolve per-session strategy. CleanResume requires reading the
    // last user message text from the (read-only) DB; ContinuePrompt
    // does not. We do this BEFORE any HTTP work so a DB-side failure
    // surfaces cleanly without leaving the server mid-operation.
    let conn = db::open_db()?;
    let mut strategy_map: HashMap<String, NudgeStrategy> = HashMap::new();
    for session in &prepared {
        let strategy = resolve_strategy(
            &conn,
            &session.session_id,
            session.static_extension.status,
            &args.continue_prompt,
        )?;
        strategy_map.insert(session.session_id.clone(), strategy);
    }
    drop(conn);

    if args.dry_run {
        for session in &prepared {
            let strategy = strategy_map
                .get(&session.session_id)
                .expect("strategy resolved above");
            let fork_suffix = if args.fork { " (via fork)" } else { "" };
            println!(
                "would {}{}",
                dry_run_message(session, strategy, args.force_nudge_already_running),
                fork_suffix
            );
        }
        println!(
            "Nudged: 0. Skipped (already running): {}. Forced (was running): 0. Failed: 0.",
            prepared
                .iter()
                .filter(|session| session.live_status == LiveStatus::Running
                    && !args.force_nudge_already_running)
                .count()
        );
        return Ok(());
    }

    let mut skipped = 0usize;
    let mut forced = 0usize;
    let mut plans = Vec::new();
    for session in &prepared {
        if session.live_status == LiveStatus::Running && !args.force_nudge_already_running {
            skipped += 1;
            println!("skipped {}: already running", session.session_id);
            continue;
        }
        let is_forced = session.live_status == LiveStatus::Running;
        if is_forced {
            forced += 1;
        }
        let strategy = strategy_map
            .remove(&session.session_id)
            .expect("strategy resolved above");
        plans.push(NudgePlan {
            session_id: session.session_id.clone(),
            static_status: session.static_extension.status,
            live_status: session.live_status,
            project_dir: session.project_dir.clone(),
            orphan: !Path::new(&session.project_dir).exists(),
            forced: is_forced,
            strategy,
            fork_first: args.fork,
        });
    }

    let outcomes = execute_plan(&plans, &client, args.concurrency);
    let outcome_map = outcomes
        .into_iter()
        .map(|outcome| (outcome.session_id.clone(), outcome))
        .collect::<HashMap<_, _>>();
    let mut failed = 0usize;
    for plan in &plans {
        let outcome = outcome_map
            .get(&plan.session_id)
            .ok_or_else(|| anyhow!("missing nudge outcome for {}", plan.session_id))?;
        match &outcome.result {
            Ok(()) if plan.forced => {
                let suffix = outcome
                    .fork_id
                    .as_deref()
                    .map(|fork| format!(" -> fork {}", fork))
                    .unwrap_or_default();
                println!(
                    "nudged {} (forced; was already running){}",
                    plan.session_id, suffix
                )
            }
            Ok(()) => {
                let suffix = outcome
                    .fork_id
                    .as_deref()
                    .map(|fork| format!(" -> fork {}", fork))
                    .unwrap_or_default();
                println!("{}{}", success_message(plan), suffix)
            }
            Err(error) => {
                failed += 1;
                println!("failed {}: {}", plan.session_id, error);
            }
        }
    }

    println!(
        "Nudged: {}. Skipped (already running): {}. Forced (was running): {}. Failed: {}.",
        plans.len().saturating_sub(failed),
        skipped,
        forced,
        failed,
    );
    if failed > 0 {
        bail!("one or more nudges failed")
    }
    Ok(())
}

/// Choose the nudge strategy for a single session.
///
/// `UserPending` always uses `CleanResume` (revert + re-fire the
/// existing user turn verbatim — no "continue" pollution).
///
/// `AssistantEmpty` is split based on how many COMPLETED assistant
/// messages share the same `parentID` as the empty last message
/// (mirrors vigil-watch's `priorCompletedAssistantSiblings` field —
/// see `count_prior_completed_assistant_siblings` for the rationale):
///   * `0` siblings → `AssistantEmptyFirst` shape.  `CleanResume` is
///     safe and preferred (nothing was committed yet).
///   * `> 0` siblings → `AssistantEmptyContinuation` shape.  The
///     empty assistant message is a continuation step under a turn
///     whose prior steps already produced side-effects.
///     `CleanResume` would `revert` past the user message and
///     DISCARD those side-effects.  We must use `ContinuePrompt`
///     (abort + post `"continue"`) to preserve the prior work.
///
/// `AssistantPartial` and `AssistantToolStuck` use `ContinuePrompt`
/// (preserve the partial work; let the LLM continue from there).
///
/// `Completed` is a guard: it should have been filtered out, but we
/// fall back to `ContinuePrompt` for safety.
///
/// Both strategies extract the original session's `agent` and `model`
/// from the relevant user message:
///   * For `CleanResume`: the most recent user message (the same one
///     we're about to revert+replay).
///   * For `ContinuePrompt`: the user message PRECEDING the broken
///     assistant turn (the one that drove that turn).
///
/// These are forwarded in the `prompt_async` body so opencode does
/// not fall back to its `default_agent`.
fn resolve_strategy(
    conn: &rusqlite::Connection,
    session_id: &str,
    status: StaticStatus,
    continue_prompt: &str,
) -> Result<NudgeStrategy> {
    match status {
        StaticStatus::UserPending => {
            let payload = fetch_last_user_message(conn, session_id)?.ok_or_else(|| {
                anyhow!(
                    "session {} has static={} but no user message was found in DB",
                    session_id,
                    status.as_str(),
                )
            })?;
            Ok(NudgeStrategy::CleanResume {
                user_msg_id: payload.user_msg_id,
                text: payload.text,
                agent: payload.agent,
                model: payload.model,
            })
        }
        StaticStatus::AssistantEmpty => {
            let prior_siblings = count_prior_completed_assistant_siblings(conn, session_id)?;
            if prior_siblings == 0 {
                // First-attempt empty: revert + replay is safe.
                let payload = fetch_last_user_message(conn, session_id)?.ok_or_else(|| {
                    anyhow!(
                        "session {} has static={} but no user message was found in DB",
                        session_id,
                        status.as_str(),
                    )
                })?;
                Ok(NudgeStrategy::CleanResume {
                    user_msg_id: payload.user_msg_id,
                    text: payload.text,
                    agent: payload.agent,
                    model: payload.model,
                })
            } else {
                // Continuation empty: prior steps committed side
                // effects (tool calls, file edits).  CleanResume
                // would discard them; use ContinuePrompt to preserve
                // the prior work and let the LLM resume mid-turn.
                let payload = fetch_user_message_before_last_assistant(conn, session_id)?;
                Ok(NudgeStrategy::ContinuePrompt {
                    prompt: continue_prompt.to_string(),
                    agent: payload.as_ref().and_then(|p| p.agent.clone()),
                    model: payload.and_then(|p| p.model),
                })
            }
        }
        StaticStatus::AssistantPartial
        | StaticStatus::AssistantToolStuck
        | StaticStatus::Completed => {
            // For ContinuePrompt, the agent/model we want is from
            // the user message that drove the broken assistant turn
            // — not the most recent user message in absolute terms.
            // If lookup fails (no preceding user message), proceed
            // without — opencode will fall back to defaults, which
            // is the pre-fix behavior (still a correct ContinuePrompt
            // dispatch, just without the identity preservation).
            let payload = fetch_user_message_before_last_assistant(conn, session_id)?;
            Ok(NudgeStrategy::ContinuePrompt {
                prompt: continue_prompt.to_string(),
                agent: payload.as_ref().and_then(|p| p.agent.clone()),
                model: payload.and_then(|p| p.model),
            })
        }
    }
}

#[derive(Clone)]
struct PreparedNudgeSession {
    session_id: String,
    static_extension: StaticExtension,
    live_status: LiveStatus,
    project_dir: String,
}

fn parse_timespan(input: Option<&str>) -> Result<Option<(i64, i64)>> {
    input
        .map(|value| {
            kal_time::parse_timespan(value)
                .map(|(start, end)| (start.timestamp(), end.timestamp()))
                .map_err(|error| anyhow!("Failed to parse timespan '{}': {}", value, error))
        })
        .transpose()
}

fn map_static_status(status: StaticStatusArg) -> StaticStatus {
    match status {
        StaticStatusArg::Completed => StaticStatus::Completed,
        StaticStatusArg::UserPending => StaticStatus::UserPending,
        StaticStatusArg::AssistantEmpty => StaticStatus::AssistantEmpty,
        StaticStatusArg::AssistantPartial => StaticStatus::AssistantPartial,
        StaticStatusArg::AssistantToolStuck => StaticStatus::AssistantToolStuck,
    }
}

fn render_orphan_error(session_ids: &[String]) -> String {
    let preview = session_ids
        .iter()
        .take(5)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if session_ids.len() <= 5 {
        return format!(
            "Refusing to nudge orphan sessions: {}. Re-run with --allow-revive-orphan-sessions to override.",
            preview
        );
    }
    format!(
        "Refusing to nudge orphan sessions: {}, ... and {} more. Re-run with --allow-revive-orphan-sessions to override.",
        preview,
        session_ids.len() - 5,
    )
}

fn dry_run_message(
    session: &PreparedNudgeSession,
    strategy: &NudgeStrategy,
    force_running: bool,
) -> String {
    if session.live_status == LiveStatus::Running && !force_running {
        return format!("skip {}: already running", session.session_id);
    }
    let action = strategy_description(strategy);
    format!(
        "nudge {} (static={}, live={}, project={}: {}; {})",
        session.session_id,
        session.static_extension.status.as_str(),
        session.live_status.as_str(),
        session.project_dir,
        action,
        shape_reason(session.static_extension.status, strategy),
    )
}

fn shape_reason(status: StaticStatus, strategy: &NudgeStrategy) -> &'static str {
    match (status, strategy) {
        (StaticStatus::UserPending, _) => {
            "LLM will respond to your existing user turn (no 'continue' marker)"
        }
        // AssistantEmpty splits on prior completed siblings under
        // the same parentID — see `resolve_strategy` and
        // `count_prior_completed_assistant_siblings`.
        (StaticStatus::AssistantEmpty, NudgeStrategy::CleanResume { .. }) => {
            "first-attempt empty: empty assistant stub deleted, original user turn re-fired (no 'continue' marker)"
        }
        (StaticStatus::AssistantEmpty, NudgeStrategy::ContinuePrompt { .. }) => {
            "continuation empty: prior steps' side effects preserved; LLM resumes mid-turn via 'continue'"
        }
        (StaticStatus::AssistantPartial, _) => "continue from truncated response",
        (StaticStatus::AssistantToolStuck, _) => {
            "opencode synthesizes interrupted tools as errors in LLM context"
        }
        (StaticStatus::Completed, _) => "not resumable",
    }
}

fn strategy_description(strategy: &NudgeStrategy) -> String {
    match strategy {
        NudgeStrategy::CleanResume { text, .. } => {
            format!(
                "clean-resume (revert + replay user message {:?})",
                truncate(text, 60)
            )
        }
        NudgeStrategy::ContinuePrompt { prompt, .. } => format!("posted {:?}", prompt),
    }
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let head: String = text.chars().take(max).collect();
    format!("{head}...")
}

fn success_message(plan: &NudgePlan) -> String {
    match (&plan.strategy, plan.static_status) {
        (NudgeStrategy::CleanResume { text, .. }, StaticStatus::UserPending) => format!(
            "nudged {} (user-pending: clean-resume; replayed your existing user turn {:?})",
            plan.session_id,
            truncate(text, 60)
        ),
        (NudgeStrategy::CleanResume { text, .. }, StaticStatus::AssistantEmpty) => format!(
            "nudged {} (assistant-empty [first attempt]: clean-resume; deleted empty stub and replayed your user turn {:?})",
            plan.session_id,
            truncate(text, 60)
        ),
        (NudgeStrategy::ContinuePrompt { prompt, .. }, StaticStatus::AssistantEmpty) => format!(
            "nudged {} (assistant-empty [continuation]: posted {:?}; preserved prior steps' side effects instead of reverting)",
            plan.session_id, prompt
        ),
        (NudgeStrategy::ContinuePrompt { prompt, .. }, StaticStatus::AssistantPartial) => format!(
            "nudged {} (assistant-partial: posted {:?} to continue from truncated response)",
            plan.session_id, prompt
        ),
        (NudgeStrategy::ContinuePrompt { prompt, .. }, StaticStatus::AssistantToolStuck) => format!(
            "nudged {} (assistant-tool-stuck: posted {:?}; opencode synthesizes interrupted tools as errors in LLM context)",
            plan.session_id, prompt
        ),
        (NudgeStrategy::ContinuePrompt { prompt, .. }, StaticStatus::Completed) => format!(
            "nudged {} (completed [forced]: posted {:?})",
            plan.session_id, prompt
        ),
        // Defensive fallbacks: shape/strategy mismatch shouldn't happen
        // (strategy is derived from shape), but stay informative if it does.
        (NudgeStrategy::CleanResume { .. }, status) => format!(
            "nudged {} ({}: clean-resume)",
            plan.session_id,
            status.as_str()
        ),
        (NudgeStrategy::ContinuePrompt { prompt, .. }, status) => format!(
            "nudged {} ({}: posted {:?})",
            plan.session_id,
            status.as_str(),
            prompt
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::opencode::status::LastMessageMeta;

    fn prepared(
        session_id: &str,
        status: StaticStatus,
        live_status: LiveStatus,
    ) -> PreparedNudgeSession {
        PreparedNudgeSession {
            session_id: session_id.to_string(),
            static_extension: StaticExtension {
                status,
                meta: LastMessageMeta {
                    session_id: session_id.to_string(),
                    msg_id: "msg".to_string(),
                    last_msg_ts: 1,
                    session_updated_ts: 1,
                    last_role: "assistant".to_string(),
                    last_completed: false,
                    parts_total: 1,
                    stuck_tools: 0,
                    assistant_errored: false,
                },
            },
            live_status,
            project_dir: "/tmp/project".to_string(),
        }
    }

    #[test]
    fn orphan_error_truncates_to_five() {
        let message = render_orphan_error(
            &[1, 2, 3, 4, 5, 6]
                .into_iter()
                .map(|index| format!("ses_{}", index))
                .collect::<Vec<_>>(),
        );
        assert!(message.contains("ses_1, ses_2, ses_3, ses_4, ses_5"));
        assert!(message.contains("... and 1 more"));
    }

    fn plan(status: StaticStatus, strategy: NudgeStrategy) -> NudgePlan {
        NudgePlan {
            session_id: "ses_1".to_string(),
            static_status: status,
            live_status: LiveStatus::Idle,
            project_dir: "/tmp/project".to_string(),
            orphan: false,
            forced: false,
            strategy,
            fork_first: false,
        }
    }

    fn clean_resume(text: &str) -> NudgeStrategy {
        NudgeStrategy::CleanResume {
            user_msg_id: "msg_1".to_string(),
            text: text.to_string(),
            agent: None,
            model: None,
        }
    }

    fn continue_prompt(prompt: &str) -> NudgeStrategy {
        NudgeStrategy::ContinuePrompt {
            prompt: prompt.to_string(),
            agent: None,
            model: None,
        }
    }

    #[test]
    fn success_messages_match_shapes() {
        assert_eq!(
            success_message(&plan(StaticStatus::UserPending, clean_resume("do X"))),
            "nudged ses_1 (user-pending: clean-resume; replayed your existing user turn \"do X\")"
        );
        assert_eq!(
            success_message(&plan(StaticStatus::AssistantEmpty, clean_resume("do Y"))),
            "nudged ses_1 (assistant-empty [first attempt]: clean-resume; deleted empty stub and replayed your user turn \"do Y\")"
        );
        assert_eq!(
            success_message(&plan(
                StaticStatus::AssistantEmpty,
                continue_prompt("continue")
            )),
            "nudged ses_1 (assistant-empty [continuation]: posted \"continue\"; preserved prior steps' side effects instead of reverting)"
        );
        assert_eq!(
            success_message(&plan(StaticStatus::AssistantPartial, continue_prompt("continue"))),
            "nudged ses_1 (assistant-partial: posted \"continue\" to continue from truncated response)"
        );
        assert_eq!(
            success_message(&plan(StaticStatus::AssistantToolStuck, continue_prompt("continue"))),
            "nudged ses_1 (assistant-tool-stuck: posted \"continue\"; opencode synthesizes interrupted tools as errors in LLM context)"
        );
    }

    #[test]
    fn long_text_is_truncated_in_messages() {
        let long = "x".repeat(200);
        let message = success_message(&plan(StaticStatus::UserPending, clean_resume(&long)));
        assert!(message.contains("..."));
        assert!(message.len() < 200);
    }

    #[test]
    fn dry_run_skips_running_by_default() {
        let strategy = continue_prompt("continue");
        assert_eq!(
            dry_run_message(
                &prepared("ses_1", StaticStatus::UserPending, LiveStatus::Running),
                &strategy,
                false
            ),
            "skip ses_1: already running"
        );
    }

    #[test]
    fn dry_run_includes_strategy_action() {
        let strategy = clean_resume("do the thing");
        let message = dry_run_message(
            &prepared("ses_1", StaticStatus::UserPending, LiveStatus::Idle),
            &strategy,
            false,
        );
        assert!(message.contains("clean-resume"), "got: {message}");
        assert!(message.contains("do the thing"), "got: {message}");
    }

    /// Build an in-memory rusqlite Connection mirroring the opencode
    /// schema, with one user message containing the given text.
    fn db_with_user_message(session_id: &str, msg_id: &str, text: &str) -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
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
        conn.execute(
            "INSERT INTO session (id, parent_id, directory, title, time_created, time_updated) VALUES (?, NULL, '', '', 1000, 2000)",
            [session_id],
        ).unwrap();
        conn.execute(
            "INSERT INTO message (id, session_id, time_created, data) VALUES (?, ?, 1500, '{\"role\":\"user\",\"time\":{\"completed\":null}}')",
            rusqlite::params![msg_id, session_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO part (id, message_id, time_created, data) VALUES (?, ?, 1600, ?)",
            rusqlite::params![
                "prt_text",
                msg_id,
                format!(r#"{{"type":"text","text":"{text}"}}"#)
            ],
        )
        .unwrap();
        conn
    }

    #[test]
    fn resolve_strategy_user_pending_returns_clean_resume() {
        let conn = db_with_user_message("ses_1", "msg_user", "do X");
        let strategy =
            resolve_strategy(&conn, "ses_1", StaticStatus::UserPending, "continue").unwrap();
        match strategy {
            NudgeStrategy::CleanResume {
                user_msg_id, text, ..
            } => {
                assert_eq!(user_msg_id, "msg_user");
                assert_eq!(text, "do X");
            }
            other => panic!("expected CleanResume, got {other:?}"),
        }
    }

    #[test]
    fn resolve_strategy_assistant_empty_first_attempt_returns_clean_resume() {
        // AssistantEmpty with NO prior completed assistant siblings
        // under the same parentID — the "first-attempt empty" shape.
        // CleanResume is safe (nothing was committed yet).
        let conn = db_with_user_message("ses_1", "msg_user", "build it");
        let strategy =
            resolve_strategy(&conn, "ses_1", StaticStatus::AssistantEmpty, "continue").unwrap();
        match strategy {
            NudgeStrategy::CleanResume {
                user_msg_id, text, ..
            } => {
                assert_eq!(user_msg_id, "msg_user");
                assert_eq!(text, "build it");
            }
            other => panic!("expected CleanResume (first-attempt empty), got {other:?}"),
        }
    }

    /// Regression for the postman-outbox / multi-step shape: an
    /// assistant turn that produced ONE OR MORE successful steps
    /// (each its own completed assistant message under the same
    /// parentID as the user message), then died at the start of a
    /// continuation step with `parts_total == 0`.  Reverting past
    /// the user message would DISCARD the completed steps' side
    /// effects — the exact failure mode the split was designed to
    /// prevent.  Must route to ContinuePrompt.
    #[test]
    fn resolve_strategy_assistant_empty_continuation_returns_continue_prompt() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE session (id TEXT PRIMARY KEY, parent_id TEXT, directory TEXT,
                title TEXT, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL);
             CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL,
                time_created INTEGER NOT NULL, data TEXT NOT NULL);
             CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT NOT NULL,
                time_created INTEGER NOT NULL, data TEXT NOT NULL);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session (id, parent_id, directory, title, time_created, time_updated) \
             VALUES ('ses_1', NULL, '', '', 1000, 2000)",
            [],
        )
        .unwrap();
        // User message (the parent of the assistant turn).
        let user_data = serde_json::json!({
            "role": "user",
            "time": { "completed": null },
            "agent": "investigator",
            "model": { "providerID": "anthropic", "modelID": "claude-opus-4-7" },
        })
        .to_string();
        conn.execute(
            "INSERT INTO message (id, session_id, time_created, data) \
             VALUES ('msg_user', 'ses_1', 1000, ?)",
            [user_data],
        )
        .unwrap();
        // Two completed assistant siblings under parentID=msg_user
        // — the "prior steps with side effects" the empty continuation
        // would discard if we ran CleanResume.
        for (id, ts) in [("msg_a1", 1500i64), ("msg_a2", 1600)] {
            let data = serde_json::json!({
                "role": "assistant",
                "parentID": "msg_user",
                "time": { "completed": 1700 },
            })
            .to_string();
            conn.execute(
                "INSERT INTO message (id, session_id, time_created, data) \
                 VALUES (?, 'ses_1', ?, ?)",
                rusqlite::params![id, ts, data],
            )
            .unwrap();
        }
        // The empty continuation step: assistant, parentID=msg_user,
        // NOT completed, no parts.
        let empty_data = serde_json::json!({
            "role": "assistant",
            "parentID": "msg_user",
            "time": { "completed": null },
        })
        .to_string();
        conn.execute(
            "INSERT INTO message (id, session_id, time_created, data) \
             VALUES ('msg_empty', 'ses_1', 1800, ?)",
            [empty_data],
        )
        .unwrap();

        let strategy =
            resolve_strategy(&conn, "ses_1", StaticStatus::AssistantEmpty, "continue").unwrap();
        match strategy {
            NudgeStrategy::ContinuePrompt { prompt, agent, model } => {
                assert_eq!(prompt, "continue");
                // Agent/model recovered from the user message preceding
                // the broken assistant turn (same path as
                // AssistantPartial / AssistantToolStuck).
                assert_eq!(agent.as_deref(), Some("investigator"));
                let model = model.expect("model should be recovered from user message");
                assert_eq!(model.provider_id, "anthropic");
                assert_eq!(model.model_id, "claude-opus-4-7");
            }
            other => panic!(
                "expected ContinuePrompt (continuation empty must NOT revert prior steps' side effects), got {other:?}"
            ),
        }
    }

    /// Errored prior siblings do NOT count as "completed" for the
    /// split — a prior step that died is not committed work to
    /// preserve.  When all siblings under the parentID are errored,
    /// the empty continuation must still route to CleanResume
    /// (matches vigil-watch's `$.error IS NOT NULL` filter).
    #[test]
    fn resolve_strategy_assistant_empty_ignores_errored_siblings() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE session (id TEXT PRIMARY KEY, parent_id TEXT, directory TEXT,
                title TEXT, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL);
             CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL,
                time_created INTEGER NOT NULL, data TEXT NOT NULL);
             CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT NOT NULL,
                time_created INTEGER NOT NULL, data TEXT NOT NULL);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session (id, parent_id, directory, title, time_created, time_updated) \
             VALUES ('ses_1', NULL, '', '', 1000, 2000)",
            [],
        )
        .unwrap();
        let user_data = r#"{"role":"user","time":{"completed":null}}"#;
        conn.execute(
            "INSERT INTO message (id, session_id, time_created, data) \
             VALUES ('msg_user', 'ses_1', 1000, ?)",
            [user_data],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO part (id, message_id, time_created, data) \
             VALUES ('prt_text', 'msg_user', 1100, ?)",
            [r#"{"type":"text","text":"redo it"}"#],
        )
        .unwrap();
        // Prior assistant sibling: completed BUT errored — does not
        // count as preserved work.
        let errored_data = serde_json::json!({
            "role": "assistant",
            "parentID": "msg_user",
            "time": { "completed": 1500 },
            "error": { "name": "APIError", "data": { "message": "boom" } },
        })
        .to_string();
        conn.execute(
            "INSERT INTO message (id, session_id, time_created, data) \
             VALUES ('msg_a1', 'ses_1', 1500, ?)",
            [errored_data],
        )
        .unwrap();
        // Empty continuation.
        let empty_data = serde_json::json!({
            "role": "assistant",
            "parentID": "msg_user",
            "time": { "completed": null },
        })
        .to_string();
        conn.execute(
            "INSERT INTO message (id, session_id, time_created, data) \
             VALUES ('msg_empty', 'ses_1', 1800, ?)",
            [empty_data],
        )
        .unwrap();

        let strategy =
            resolve_strategy(&conn, "ses_1", StaticStatus::AssistantEmpty, "continue").unwrap();
        match strategy {
            NudgeStrategy::CleanResume { text, .. } => {
                assert_eq!(text, "redo it");
            }
            other => panic!(
                "expected CleanResume (no committed siblings — only errored ones), got {other:?}"
            ),
        }
    }

    #[test]
    fn resolve_strategy_assistant_partial_returns_continue_prompt() {
        let conn = db_with_user_message("ses_1", "msg_user", "doesn't matter");
        let strategy =
            resolve_strategy(&conn, "ses_1", StaticStatus::AssistantPartial, "keep going").unwrap();
        match strategy {
            NudgeStrategy::ContinuePrompt { prompt, .. } => {
                assert_eq!(prompt, "keep going");
            }
            other => panic!("expected ContinuePrompt, got {other:?}"),
        }
    }

    #[test]
    fn resolve_strategy_assistant_tool_stuck_returns_continue_prompt() {
        let conn = db_with_user_message("ses_1", "msg_user", "doesn't matter");
        let strategy =
            resolve_strategy(&conn, "ses_1", StaticStatus::AssistantToolStuck, "continue").unwrap();
        assert!(matches!(strategy, NudgeStrategy::ContinuePrompt { .. }));
    }

    /// Phase A2c — resolve_strategy must extract agent + model from
    /// the user message into the CleanResume strategy.
    #[test]
    fn resolve_strategy_user_pending_carries_agent_and_model() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE session (id TEXT PRIMARY KEY, parent_id TEXT, directory TEXT,
                title TEXT, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL);
             CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL,
                time_created INTEGER NOT NULL, data TEXT NOT NULL);
             CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT NOT NULL,
                time_created INTEGER NOT NULL, data TEXT NOT NULL);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session (id, parent_id, directory, title, time_created, time_updated) \
             VALUES ('ses_1', NULL, '', '', 1000, 2000)",
            [],
        )
        .unwrap();
        let data = serde_json::json!({
            "role": "user",
            "time": { "completed": null },
            "agent": "conductor",
            "model": { "providerID": "anthropic", "modelID": "claude-opus-4-5" },
        })
        .to_string();
        conn.execute(
            "INSERT INTO message (id, session_id, time_created, data) \
             VALUES ('msg_user', 'ses_1', 1500, ?)",
            [data],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO part (id, message_id, time_created, data) \
             VALUES ('prt_text', 'msg_user', 1600, ?)",
            [r#"{"type":"text","text":"do X"}"#],
        )
        .unwrap();

        let strategy =
            resolve_strategy(&conn, "ses_1", StaticStatus::UserPending, "continue").unwrap();
        match strategy {
            NudgeStrategy::CleanResume {
                user_msg_id,
                text,
                agent,
                model,
            } => {
                assert_eq!(user_msg_id, "msg_user");
                assert_eq!(text, "do X");
                assert_eq!(agent.as_deref(), Some("conductor"));
                let model = model.expect("model should be present");
                assert_eq!(model.provider_id, "anthropic");
                assert_eq!(model.model_id, "claude-opus-4-5");
            }
            other => panic!("expected CleanResume, got {other:?}"),
        }
    }

    /// Phase A2c — for AssistantPartial, the strategy must extract
    /// agent + model from the user message PRECEDING the broken
    /// assistant turn.
    #[test]
    fn resolve_strategy_assistant_partial_carries_agent_from_preceding_user() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE session (id TEXT PRIMARY KEY, parent_id TEXT, directory TEXT,
                title TEXT, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL);
             CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL,
                time_created INTEGER NOT NULL, data TEXT NOT NULL);
             CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT NOT NULL,
                time_created INTEGER NOT NULL, data TEXT NOT NULL);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session (id, parent_id, directory, title, time_created, time_updated) \
             VALUES ('ses_1', NULL, '', '', 1000, 2000)",
            [],
        )
        .unwrap();
        let user_data = serde_json::json!({
            "role": "user",
            "time": { "completed": null },
            "agent": "drafter",
        })
        .to_string();
        conn.execute(
            "INSERT INTO message (id, session_id, time_created, data) \
             VALUES ('msg_user', 'ses_1', 1000, ?)",
            [user_data],
        )
        .unwrap();
        let assistant_data = r#"{"role":"assistant","time":{"completed":null}}"#;
        conn.execute(
            "INSERT INTO message (id, session_id, time_created, data) \
             VALUES ('msg_asst', 'ses_1', 2000, ?)",
            [assistant_data],
        )
        .unwrap();

        let strategy =
            resolve_strategy(&conn, "ses_1", StaticStatus::AssistantPartial, "continue").unwrap();
        match strategy {
            NudgeStrategy::ContinuePrompt { prompt, agent, .. } => {
                assert_eq!(prompt, "continue");
                assert_eq!(agent.as_deref(), Some("drafter"));
            }
            other => panic!("expected ContinuePrompt, got {other:?}"),
        }
    }

    #[test]
    fn resolve_strategy_clean_resume_errors_when_no_user_message() {
        // user-pending shape with no user message in DB is impossible
        // in practice (the static classifier wouldn't have produced
        // user-pending), but we guard defensively. The error message
        // must mention the session id and the missing user message.
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE session (id TEXT PRIMARY KEY, parent_id TEXT, directory TEXT,
                title TEXT, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL);
             CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL,
                time_created INTEGER NOT NULL, data TEXT NOT NULL);
             CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT NOT NULL,
                time_created INTEGER NOT NULL, data TEXT NOT NULL);",
        )
        .unwrap();

        let error = resolve_strategy(&conn, "ses_ghost", StaticStatus::UserPending, "continue")
            .unwrap_err();
        let msg = error.to_string();
        assert!(msg.contains("ses_ghost"), "got: {msg}");
        assert!(msg.contains("user message"), "got: {msg}");
    }
}

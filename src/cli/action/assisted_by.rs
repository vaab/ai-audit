//! `assisted-by` action: resolve the kernel-canonical
//! `Assisted-by:` trailer for an AI-assisted commit.
//!
//! Spec: <https://www.kernel.org/doc/html/latest/process/coding-assistants.html>
//!
//! Trailer format: `Assisted-by: AGENT_NAME:MODEL_VERSION`
//! where `AGENT_NAME` is the AI vendor brand (e.g. `Claude`, `GPT`),
//! mapped from the harness-reported `llm_provider` via
//! [`crate::provider::brand_for_llm_provider`].
//!
//! ## Multi-session attribution
//!
//! A commit can be assisted by several stacked harnesses at once —
//! the realistic case is pi spawned from inside OpenCode, where the
//! commit-msg hook inherits both `OPENCODE_SESSION_ID` and
//! `PI_SESSION_ID`.  Per the kernel spec a commit can carry multiple
//! `Assisted-by:` lines, so this action emits one line per unique
//! `(brand, model_id)` pair: identical pairs are collapsed (so
//! `pi=GPT:gpt-5.5` + `opencode=GPT:gpt-5.5` yields a single
//! trailer), distinct pairs are emitted in env-var precedence order.
//!
//! See `doc/admin.org` for the full attribution policy.

use anyhow::Result;
use serde_json::json;

use crate::provider::{provider_for_session, ModelAttribution};
use crate::session_detect::DetectedSession;
use crate::OutputFormat;

/// One resolved attribution paired with the session id and trailer
/// string it came from.  Kept together so JSON output can show every
/// row alongside its trailer.
struct ResolvedRow {
    session_id: String,
    attribution: ModelAttribution,
    trailer: String,
}

/// Run the `assisted-by` action.
///
/// * `session` — explicit session id; bypasses detection.  Always
///   yields exactly one trailer.
/// * `quiet_if_no_session` — when true and detection fails, exit 0
///   silently (intended for hooks running in human shells).
pub fn run(session: Option<String>, quiet_if_no_session: bool, format: OutputFormat) -> Result<()> {
    let detected = match session {
        Some(id) => {
            // Explicit session id: synthesise a one-element vec so the
            // rest of the pipeline is uniform.  Provider is detected
            // from the id format the same way `provider_for_session`
            // does it below.
            let provider = crate::provider::detect_provider(&id)?;
            vec![DetectedSession {
                session_id: id,
                provider,
            }]
        }
        None => match crate::session_detect::detect_current_sessions() {
            Ok(found) => {
                for s in &found {
                    log::info!("Auto-detected session: {} ({:?})", s.session_id, s.provider);
                }
                found
            }
            Err(err) => {
                if quiet_if_no_session {
                    log::info!(
                        "No current session detected ({}); --quiet-if-no-session set, exiting 0",
                        err
                    );
                    return Ok(());
                }
                return Err(err);
            }
        },
    };

    // Resolve attribution for every detected session, then dedup on
    // (brand, model_id) — the two fields that appear in the trailer.
    // `seen` preserves first-seen ordering, which matches env-var
    // precedence (OpenCode → ClaudeCode → pi) so trailer rendering
    // is deterministic.
    let mut rows: Vec<ResolvedRow> = Vec::new();
    let mut seen: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
    for ds in &detected {
        let provider = provider_for_session(&ds.session_id)?;
        let attribution = provider.resolve_attribution(&ds.session_id)?;
        let trailer = attribution.trailer()?;
        // (llm_provider, model_id) is the dedup key: same brand
        // mapping + same model id renders to the same trailer string.
        let key = (
            attribution.llm_provider.clone().unwrap_or_default(),
            attribution.model_id.clone(),
        );
        if seen.insert(key) {
            rows.push(ResolvedRow {
                session_id: ds.session_id.clone(),
                attribution,
                trailer,
            });
        } else {
            log::debug!(
                "skipping duplicate attribution for session {} (already covered \
                 by an earlier session with the same brand+model)",
                ds.session_id
            );
        }
    }

    match format {
        OutputFormat::Human => {
            for row in &rows {
                println!("{}", row.trailer);
            }
        }
        OutputFormat::Nul => {
            for row in &rows {
                print!("{}\0", row.trailer);
            }
        }
        OutputFormat::Json => {
            // One JSON object per emitted trailer.  Newline-delimited
            // for easy `| jq` consumption; matches the convention
            // used by other `--json` outputs in this tool.
            for row in &rows {
                let attr = &row.attribution;
                let out = json!({
                    "session_id": row.session_id,
                    "harness": attr.harness.as_str(),
                    "llm_provider": attr.llm_provider,
                    "llm_provider_inferred": attr.llm_provider_inferred,
                    "model_id": attr.model_id,
                    "access_surface": attr.access_surface,
                    "agent": attr.agent,
                    "mode": attr.mode,
                    "variant": attr.variant,
                    "trailer": row.trailer,
                });
                println!("{}", out);
            }
        }
    }
    Ok(())
}

//! Sanity check on raw LLM output.
//!
//! Single tripwire that runs at the harness boundary
//! (`ai_audit::pi::run::run`) before the caller ever sees the
//! AI's text. Its job is to detect outputs that are obviously cut
//! short — network drop, agent crash, preamble-only — by asking
//! the caller a single yes/no question:
//!
//! > "Does this output roughly look like what we asked for?"
//!
//! It does NOT parse the output. Strict structural parsers
//! (``parse_markdown_output``, ``parse_classify_output``,
//! ``parse_subagent_line``) stay where they are and run only after
//! the tripwire passes.
//!
//! ## Why this is at the harness layer
//!
//! Every place that calls ``run::run`` had been silently accepting
//! whatever bytes came back, then trusting downstream parsers to
//! fail loudly. They didn't: parsers had loose fallbacks, empty-OK
//! paths, and legacy-format escape hatches. The result was that
//! truncated runs landed in the on-disk cache as if they were
//! successful. The fix is to gate the bytes one layer earlier, in
//! the single function every AI call already goes through.
//!
//! ## Shape of the check
//!
//! The caller hands in an [`AiTaskSpec`] containing:
//!
//! - ``shape``: a one-line human-readable description of what a
//!   complete answer must contain. Echoed in the error message.
//! - ``looks_complete``: a cheap predicate that returns
//!   ``Result<(), String>``. ``Ok(())`` means "looks fine, proceed".
//!   ``Err(reason)`` means "the bytes do not match the contract;
//!   here is why". The reason is included verbatim in the error.
//!
//! Predicates are deliberately loose — substring/regex tripwires,
//! not parsers. Their only job is to catch the "agent died
//! mid-sentence" failure mode.

use std::error::Error;
use std::fmt;

/// Caller-supplied sanity-check spec for a single AI task.
///
/// See module docs.
pub struct AiTaskSpec<'a> {
    /// Human-readable description of what a complete answer must
    /// contain. Used verbatim in error messages.
    ///
    /// Example: ``"a '## Timeline' section"``.
    pub shape: &'a str,
    /// Cheap predicate. ``Ok(())`` if the output looks complete.
    /// ``Err(reason)`` to reject with a specific reason.
    pub looks_complete: &'a (dyn Fn(&str) -> Result<(), String> + Sync),
}

/// Error returned when the harness sanity check rejects an output.
///
/// Carries the session id, length, and head/tail snippets so the
/// caller (and the user reading logs) can correlate the failure
/// with upstream API logs.  Pi sessions are ephemeral when called
/// from insight-cli (``--no-session``) so the session id is a
/// trace identifier only — no on-disk transcript exists.
#[derive(Debug)]
pub struct LlmOutputCutShort {
    /// Session id of the pi run that produced this output.
    pub session_id: Option<String>,
    /// Caller-supplied shape description (echoed from
    /// [`AiTaskSpec::shape`]).
    pub shape: String,
    /// Reason returned by the predicate.
    pub reason: String,
    /// Total output length in bytes.
    pub output_len: usize,
    /// First ``HEAD_TAIL_LIMIT`` characters of the output.
    pub head: String,
    /// Last ``HEAD_TAIL_LIMIT`` characters of the output.
    pub tail: String,
}

/// Maximum head/tail snippet length stored in [`LlmOutputCutShort`].
const HEAD_TAIL_LIMIT: usize = 200;

impl LlmOutputCutShort {
    /// Build a cut-short error from raw output and a spec.
    pub(crate) fn from_output(
        spec: &AiTaskSpec<'_>,
        reason: String,
        session_id: Option<String>,
        output: &str,
    ) -> Self {
        let head = take_chars(output, HEAD_TAIL_LIMIT);
        let tail = take_chars_end(output, HEAD_TAIL_LIMIT);
        Self {
            session_id,
            shape: spec.shape.to_string(),
            reason,
            output_len: output.len(),
            head,
            tail,
        }
    }
}

impl fmt::Display for LlmOutputCutShort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let session = self.session_id.as_deref().unwrap_or("<unknown>");
        write!(
            f,
            "LLM output looks cut short (network drop / agent crash / \
             preamble-only?). Expected: {expected}. Reason: {reason}. \
             Pi session: {session} ({len} bytes). \
             Rerun with --recompute once the upstream is healthy.\n\
             --- head ---\n{head}\n--- tail ---\n{tail}",
            expected = self.shape,
            reason = self.reason,
            session = session,
            len = self.output_len,
            head = self.head,
            tail = self.tail,
        )
    }
}

impl Error for LlmOutputCutShort {}

/// Apply [`AiTaskSpec`] to raw output. Returns ``Ok(())`` if the
/// output passes the tripwire, ``Err(Box<LlmOutputCutShort>)``
/// otherwise. The error is boxed because [`LlmOutputCutShort`]
/// carries 200-char head/tail snippets and is therefore too large
/// to live inline in a hot ``Result`` path (clippy::result_large_err).
pub fn check(
    spec: &AiTaskSpec<'_>,
    session_id: Option<&str>,
    output: &str,
) -> Result<(), Box<LlmOutputCutShort>> {
    match (spec.looks_complete)(output) {
        Ok(()) => Ok(()),
        Err(reason) => Err(Box::new(LlmOutputCutShort::from_output(
            spec,
            reason,
            session_id.map(str::to_string),
            output,
        ))),
    }
}

/// Take the first ``n`` characters of ``s`` (not bytes — preserves
/// UTF-8 boundaries).
fn take_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Take the last ``n`` characters of ``s`` (not bytes — preserves
/// UTF-8 boundaries).
fn take_chars_end(s: &str, n: usize) -> String {
    let total = s.chars().count();
    if total <= n {
        return s.to_string();
    }
    s.chars().skip(total - n).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn predicate_pass_returns_ok() {
        let spec = AiTaskSpec {
            shape: "a '## Timeline' section",
            looks_complete: &|s| {
                if s.contains("## Timeline") {
                    Ok(())
                } else {
                    Err("missing '## Timeline'".into())
                }
            },
        };
        let output = "preamble\n\n## Timeline\n| 09:00 | 10:00 | work | yes | … |\n";
        assert!(check(&spec, Some("ses_test"), output).is_ok());
    }

    /// Regression: the literal preamble emitted by
    /// ``ses_222865b2dffeshd3LqPtseHRzy`` (the network-killed deep-seg
    /// session referenced in ``doc/admin.org``) must be rejected.
    #[test]
    fn predicate_fail_returns_cut_short_with_session_id() {
        let spec = AiTaskSpec {
            shape: "a '## Timeline' section",
            looks_complete: &|s| {
                if s.contains("## Timeline") {
                    Ok(())
                } else {
                    Err("missing '## Timeline'".into())
                }
            },
        };
        let preamble = "I'll perform deep segmentation following the prompt \
            strictly. Let me gather evidence in parallel.";
        let err = *check(&spec, Some("ses_222865b2dffeshd3LqPtseHRzy"), preamble)
            .expect_err("cut-short preamble must be rejected");
        assert_eq!(
            err.session_id.as_deref(),
            Some("ses_222865b2dffeshd3LqPtseHRzy")
        );
        assert_eq!(err.shape, "a '## Timeline' section");
        assert_eq!(err.reason, "missing '## Timeline'");
        assert_eq!(err.output_len, preamble.len());
        let rendered = err.to_string();
        assert!(rendered.contains("ses_222865b2dffeshd3LqPtseHRzy"));
        assert!(rendered.contains("Pi session: ses_222865b2dffeshd3LqPtseHRzy"));
        assert!(rendered.contains("a '## Timeline' section"));
        assert!(rendered.contains("--recompute"));
    }

    #[test]
    fn cut_short_carries_head_and_tail_snippets() {
        let spec = AiTaskSpec {
            shape: "anything",
            looks_complete: &|_| Err("never matches".into()),
        };
        let big: String = "abcdefghij".repeat(50); // 500 chars
        let err = *check(&spec, None, &big).unwrap_err();
        // 200 chars head, 200 chars tail
        assert_eq!(err.head.chars().count(), HEAD_TAIL_LIMIT);
        assert_eq!(err.tail.chars().count(), HEAD_TAIL_LIMIT);
        assert!(big.starts_with(&err.head));
        assert!(big.ends_with(&err.tail));
    }

    #[test]
    fn short_output_head_and_tail_collapse_to_full_text() {
        let spec = AiTaskSpec {
            shape: "anything",
            looks_complete: &|_| Err("never".into()),
        };
        let short = "tiny";
        let err = *check(&spec, None, short).unwrap_err();
        assert_eq!(err.head, "tiny");
        assert_eq!(err.tail, "tiny");
    }

    #[test]
    fn utf8_boundaries_are_preserved_in_head_tail() {
        let spec = AiTaskSpec {
            shape: "anything",
            looks_complete: &|_| Err("never".into()),
        };
        // Multi-byte chars; if we sliced bytes naively this would panic.
        let s: String = "é".repeat(300); // 600 bytes, 300 chars
        let err = *check(&spec, None, &s).unwrap_err();
        assert_eq!(err.head.chars().count(), HEAD_TAIL_LIMIT);
        assert_eq!(err.tail.chars().count(), HEAD_TAIL_LIMIT);
    }
}

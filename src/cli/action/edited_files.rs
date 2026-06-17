//! `session edited-files` action: inspect, replay, or reconstruct the
//! files written/edited during a recorded AI session.
//!
//! The action operates in four mutually-exclusive modes (enforced by
//! clap `conflicts_with`):
//!
//! * **stat** (default) — git-`--stat`-style per-file summary with
//!   insertion/deletion line counts and a histogram bar.
//! * **`-p` / `--patch`** — dump the raw recorded ops in chronological
//!   order: write content verbatim, edit `old`/`new` blocks with
//!   minimal framing.  NOT a unified diff.
//! * **`--apply <DIR>`** — replay every [`crate::edits::FileOp`] onto
//!   `DIR`, materialising a self-contained snapshot of the session's
//!   filesystem effects.  Recorded absolute paths are re-rooted under
//!   `DIR` (e.g. `/home/u/a.rs` → `<DIR>/home/u/a.rs`) so replay never
//!   escapes the destination.
//! * **`--extract <FILE>`** — reconstruct and emit the final content
//!   of a single file, by replaying just the ops touching that file.
//!   Requires at least one in-session `Write` op (purely edited
//!   pre-existing files can't be reconstructed).

use anyhow::{bail, Context, Result};
use colored::Colorize;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::edits::{self, EditedFile, FileOp};
use crate::OutputFormat;

/// Maximum width (in characters) of the histogram bar in the stat
/// output.  Mirrors git's default `--stat-graph-width`.
const STAT_BAR_BUDGET: usize = 60;

/// Entry point wired by [`super::dispatch`].
pub fn run(
    session: &str,
    apply: Option<PathBuf>,
    extract: Option<String>,
    out: Option<PathBuf>,
    patch: bool,
    format: OutputFormat,
) -> Result<()> {
    let provider = crate::provider::provider_for_session(session)?;
    let entries = provider.parse_transcript(session)?;
    let ops = edits::extract_ops(&entries);

    if let Some(dir) = apply {
        return apply_ops(&ops, &dir);
    }
    if let Some(target) = extract {
        return extract_file(&ops, &target, out.as_deref());
    }
    if patch {
        return print_patch(&ops, format);
    }
    print_stat(&ops, format)
}

// ---------------------------------------------------------------------------
// stat mode
// ---------------------------------------------------------------------------

fn print_stat(ops: &[FileOp], format: OutputFormat) -> Result<()> {
    let files = edits::stat(ops);
    let stdout = io::stdout();
    let mut handle = stdout.lock();

    match format {
        OutputFormat::Human => {
            let rendered = render_stat_human_string(&files);
            handle.write_all(rendered.as_bytes())?;
        }
        OutputFormat::Json => {
            for f in &files {
                writeln!(handle, "{}", serde_json::to_string(f)?)?;
            }
        }
        OutputFormat::Nul => {
            for f in &files {
                write!(handle, "{}\0", f.path)?;
            }
        }
    }
    Ok(())
}

/// Render the Human-mode git-`--stat`-style block as a single string.
///
/// Layout per file:
///
/// ```text
///  <path padded>  | <N> <histogram>
/// ```
///
/// followed by a `<K> file(s) changed[, <I> insertion(s)(+)][, <D>
/// deletion(s)(-)]` summary line.  `<N>` is `insertions + deletions`,
/// the histogram is `+` (green) then `-` (red) with widths scaled so
/// the widest row fits in [`STAT_BAR_BUDGET`].  Any file with at
/// least one changed line is guaranteed at least one bar character.
///
/// Color is applied via the `colored` crate; the global override
/// (initialised by `cli::color::init`) decides whether ANSI escapes
/// are actually emitted, so tests (which run with color off) get
/// plain text and can diff exactly.
fn render_stat_human_string(files: &[EditedFile]) -> String {
    let mut out = String::new();
    if files.is_empty() {
        out.push_str(" 0 files changed\n");
        return out;
    }

    let path_width = files.iter().map(|f| f.path.len()).max().unwrap_or(0);
    let n_per_file: Vec<usize> = files.iter().map(|f| f.insertions + f.deletions).collect();
    let max_n = *n_per_file.iter().max().unwrap_or(&0);
    let n_width = max_n.to_string().len();
    let scale = if max_n > STAT_BAR_BUDGET {
        STAT_BAR_BUDGET as f64 / max_n as f64
    } else {
        1.0
    };

    for (f, &n) in files.iter().zip(n_per_file.iter()) {
        let (plus, minus) = scale_bar(f.insertions, f.deletions, scale);
        let bar = format!("{}{}", "+".repeat(plus).green(), "-".repeat(minus).red());
        out.push_str(&format!(
            " {:<pw$} | {:>nw$} {}\n",
            f.path,
            n,
            bar,
            pw = path_width,
            nw = n_width,
        ));
    }

    let total_ins: usize = files.iter().map(|f| f.insertions).sum();
    let total_del: usize = files.iter().map(|f| f.deletions).sum();
    out.push_str(&summary_line(files.len(), total_ins, total_del));
    out.push('\n');
    out
}

/// Scale a single file's insertions/deletions to bar-character counts,
/// guaranteeing that any file with `>0` changes shows at least one
/// bar character.
fn scale_bar(insertions: usize, deletions: usize, scale: f64) -> (usize, usize) {
    let total = insertions + deletions;
    if total == 0 {
        return (0, 0);
    }
    let plus_raw = insertions as f64 * scale;
    let minus_raw = deletions as f64 * scale;
    let mut plus = plus_raw.round() as usize;
    let mut minus = minus_raw.round() as usize;
    // Preserve the "has any insertion / deletion" signal even when
    // rounding would drop a tiny slice to zero.
    if insertions > 0 && plus == 0 {
        plus = 1;
    }
    if deletions > 0 && minus == 0 {
        minus = 1;
    }
    (plus, minus)
}

/// Build the `<K> file(s) changed[, <I> insertion(s)(+)][, <D>
/// deletion(s)(-)]` summary line.  Mirrors git's plural agreement and
/// omits the insertion / deletion clauses when their count is zero.
fn summary_line(files: usize, insertions: usize, deletions: usize) -> String {
    let file_word = if files == 1 { "file" } else { "files" };
    let mut parts: Vec<String> = Vec::new();
    parts.push(format!("{} {} changed", files, file_word));
    if insertions > 0 {
        let word = if insertions == 1 {
            "insertion(+)"
        } else {
            "insertions(+)"
        };
        parts.push(format!("{} {}", insertions, word));
    }
    if deletions > 0 {
        let word = if deletions == 1 {
            "deletion(-)"
        } else {
            "deletions(-)"
        };
        parts.push(format!("{} {}", deletions, word));
    }
    parts.join(", ")
}

// ---------------------------------------------------------------------------
// patch mode
// ---------------------------------------------------------------------------

/// Dump the recorded ops in chronological order — write content
/// verbatim, edit `old`/`new` blocks with minimal framing.  Not a
/// unified diff; raw old/new text with thin headers so a human (or a
/// pipeline) can see exactly what the AI claimed to write.
fn print_patch(ops: &[FileOp], format: OutputFormat) -> Result<()> {
    if ops.is_empty() {
        println!("no file ops recorded in this session");
        return Ok(());
    }

    let stdout = io::stdout();
    let mut handle = stdout.lock();

    match format {
        OutputFormat::Human => {
            for (i, op) in ops.iter().enumerate() {
                if i > 0 {
                    writeln!(handle)?;
                }
                match op {
                    FileOp::Write { path, content } => {
                        let header = format!("=== {} (write) ===", path);
                        writeln!(handle, "{}", header.cyan().bold())?;
                        handle.write_all(content.as_bytes())?;
                        if !content.ends_with('\n') {
                            writeln!(handle)?;
                        }
                    }
                    FileOp::Edit {
                        path,
                        old,
                        new,
                        replace_all,
                    } => {
                        let suffix = if *replace_all { ", replace-all" } else { "" };
                        let header = format!("=== {} (edit{}) ===", path, suffix);
                        writeln!(handle, "{}", header.cyan().bold())?;
                        writeln!(handle, "{}", "--- old".red())?;
                        handle.write_all(old.as_bytes())?;
                        if !old.ends_with('\n') {
                            writeln!(handle)?;
                        }
                        writeln!(handle, "{}", "--- new".green())?;
                        handle.write_all(new.as_bytes())?;
                        if !new.ends_with('\n') {
                            writeln!(handle)?;
                        }
                    }
                }
            }
        }
        OutputFormat::Json => {
            for op in ops {
                writeln!(handle, "{}", serde_json::to_string(op)?)?;
            }
        }
        OutputFormat::Nul => {
            for op in ops {
                match op {
                    FileOp::Write { path, content } => {
                        write!(handle, "write\0{}\0{}\0", path, content)?;
                    }
                    FileOp::Edit {
                        path,
                        old,
                        new,
                        replace_all,
                    } => {
                        write!(
                            handle,
                            "edit\0{}\0{}\0{}\0{}\0",
                            path, old, new, replace_all
                        )?;
                    }
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// apply mode
// ---------------------------------------------------------------------------

/// Replay every recorded op onto `dir`.
///
/// Each op's target is computed by [`reroot_under`] so that absolute
/// recorded paths (e.g. `/home/u/a.rs`) land under `dir` rather than
/// modifying the real source tree.  Failures are reported per-op and
/// do NOT stop the replay (continue-on-error semantics) — the final
/// summary plus a non-zero exit code (via [`bail!`]) communicates
/// partial failure to the caller.
fn apply_ops(ops: &[FileOp], dir: &Path) -> Result<()> {
    if ops.is_empty() {
        println!("no file ops recorded in this session");
        return Ok(());
    }

    let mut applied = 0usize;
    let mut failed = 0usize;
    let stdout = io::stdout();
    let mut handle = stdout.lock();

    for op in ops {
        let target = reroot_under(dir, op.path());
        match apply_one(op, &target) {
            Ok(label) => {
                applied += 1;
                writeln!(handle, "ok    {} {}", label, target.display())?;
            }
            Err(err) => {
                failed += 1;
                writeln!(
                    handle,
                    "fail  {} {}: {}",
                    op_label(op),
                    target.display(),
                    err
                )?;
            }
        }
    }

    writeln!(handle, "applied {applied} ops, {failed} failed")?;
    if failed > 0 {
        bail!("{failed} op(s) failed during replay");
    }
    Ok(())
}

/// Apply a single op onto a fully-resolved target path.  Returns the
/// short label used in the success line.
fn apply_one(op: &FileOp, target: &Path) -> Result<&'static str> {
    match op {
        FileOp::Write { content, .. } => {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("create parent dir {}", parent.display()))?;
            }
            fs::write(target, content).with_context(|| format!("write {}", target.display()))?;
            Ok("write")
        }
        FileOp::Edit {
            old,
            new,
            replace_all,
            ..
        } => {
            let existing =
                fs::read_to_string(target).with_context(|| format!("read {}", target.display()))?;
            let updated = apply_edit(&existing, old, new, *replace_all)?;
            fs::write(target, updated).with_context(|| format!("write {}", target.display()))?;
            Ok("edit ")
        }
    }
}

fn op_label(op: &FileOp) -> &'static str {
    match op {
        FileOp::Write { .. } => "write",
        FileOp::Edit { .. } => "edit ",
    }
}

/// Apply one in-place substitution.  Mirrors the Claude Code / OpenCode
/// `Edit` tool contract: when `replace_all` is false, `old` MUST
/// appear EXACTLY once.
fn apply_edit(existing: &str, old: &str, new: &str, replace_all: bool) -> Result<String> {
    if replace_all {
        if !existing.contains(old) {
            bail!("old_string not found");
        }
        Ok(existing.replace(old, new))
    } else {
        let count = existing.matches(old).count();
        match count {
            0 => bail!("old_string not found"),
            1 => Ok(existing.replacen(old, new, 1)),
            n => bail!("old_string not unique ({n} matches)"),
        }
    }
}

/// Re-root a recorded path under `dir`.  Absolute paths have their
/// leading `/` stripped so the result is always a strict descendant
/// of `dir`; relative paths are joined directly.  Windows-style
/// drive prefixes (`C:\…`) are also stripped to a relative form.
fn reroot_under(dir: &Path, recorded: &str) -> PathBuf {
    let p = Path::new(recorded);
    let relative = if p.is_absolute() {
        // Strip the root component(s).  On Unix this is `/`; on
        // Windows this would be drive+prefix.  `Path::strip_prefix`
        // would need to know the exact root, so we walk components
        // and drop any leading RootDir / Prefix.
        let mut rel = PathBuf::new();
        for comp in p.components() {
            use std::path::Component::*;
            match comp {
                Prefix(_) | RootDir => continue,
                Normal(s) => rel.push(s),
                CurDir => continue,
                ParentDir => rel.push(".."),
            }
        }
        rel
    } else {
        p.to_path_buf()
    };
    dir.join(relative)
}

// ---------------------------------------------------------------------------
// extract mode
// ---------------------------------------------------------------------------

/// Reconstruct the final content of `target` by replaying the ops
/// that touch it, in order.
fn extract_file(ops: &[FileOp], target: &str, out: Option<&Path>) -> Result<()> {
    let matching: Vec<&FileOp> = ops
        .iter()
        .filter(|op| path_matches(op.path(), target))
        .collect();
    if matching.is_empty() {
        bail!("no recorded edits for {target}");
    }

    let mut buf: Option<String> = None;
    for op in matching {
        match op {
            FileOp::Write { content, .. } => {
                buf = Some(content.clone());
            }
            FileOp::Edit {
                old,
                new,
                replace_all,
                ..
            } => match buf.as_ref() {
                Some(existing) => {
                    let updated = apply_edit(existing, old, new, *replace_all)
                        .with_context(|| format!("replaying edit for {target}"))?;
                    buf = Some(updated);
                }
                None => {
                    // Edit before any Write: can't reconstruct
                    // because we have no base content.
                    bail!(
                        "{target} was never written in-session; cannot reconstruct \
                         (only edited)"
                    );
                }
            },
        }
    }

    let content = match buf {
        Some(c) => c,
        None => bail!("{target} has no usable ops"),
    };

    match out {
        Some(path) => {
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() {
                    fs::create_dir_all(parent)
                        .with_context(|| format!("create parent dir {}", parent.display()))?;
                }
            }
            fs::write(path, content).with_context(|| format!("write {}", path.display()))?;
        }
        None => {
            io::stdout().write_all(content.as_bytes())?;
        }
    }
    Ok(())
}

/// Decide whether a recorded `op_path` is the file the user named
/// with `target`.  Tries exact equality first, then component-suffix
/// matching in both directions so `src/main.rs` can match the
/// recorded `/home/u/proj/src/main.rs` and vice versa.
fn path_matches(op_path: &str, target: &str) -> bool {
    if op_path == target {
        return true;
    }
    crate::file_path_matches(target, op_path) || crate::file_path_matches(op_path, target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edits::FileOp;
    use indoc::indoc;
    use similar::TextDiff;
    use tempfile::tempdir;

    fn write(path: &str, content: &str) -> FileOp {
        FileOp::Write {
            path: path.into(),
            content: content.into(),
        }
    }

    fn edit(path: &str, old: &str, new: &str, replace_all: bool) -> FileOp {
        FileOp::Edit {
            path: path.into(),
            old: old.into(),
            new: new.into(),
            replace_all,
        }
    }

    fn assert_output_eq(actual: &str, expected: &str) {
        if actual != expected {
            let diff = TextDiff::from_lines(expected, actual);
            eprintln!();
            for line in diff
                .unified_diff()
                .header("expected", "actual")
                .to_string()
                .lines()
            {
                if line.starts_with('-') {
                    eprintln!("\x1b[31m{}\x1b[0m", line);
                } else if line.starts_with('+') {
                    eprintln!("\x1b[32m{}\x1b[0m", line);
                } else if line.starts_with('@') {
                    eprintln!("\x1b[36m{}\x1b[0m", line);
                } else {
                    eprintln!("{}", line);
                }
            }
            panic!("Output mismatch - see diff above");
        }
    }

    // ---- reroot_under -------------------------------------------------

    #[test]
    fn test_reroot_under_absolute() {
        let got = reroot_under(Path::new("/tmp/snap"), "/home/u/a.rs");
        assert_eq!(got, PathBuf::from("/tmp/snap/home/u/a.rs"));
    }

    #[test]
    fn test_reroot_under_relative() {
        let got = reroot_under(Path::new("/tmp/snap"), "src/a.rs");
        assert_eq!(got, PathBuf::from("/tmp/snap/src/a.rs"));
    }

    // ---- apply_edit ---------------------------------------------------

    #[test]
    fn test_apply_edit_single_match() {
        let got = apply_edit("hello world", "world", "rust", false).unwrap();
        assert_eq!(got, "hello rust");
    }

    #[test]
    fn test_apply_edit_not_found() {
        let err = apply_edit("hello", "world", "rust", false).unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn test_apply_edit_not_unique() {
        let err = apply_edit("foo foo foo", "foo", "bar", false).unwrap_err();
        assert!(err.to_string().contains("not unique"));
        assert!(err.to_string().contains("3"));
    }

    #[test]
    fn test_apply_edit_replace_all() {
        let got = apply_edit("foo foo", "foo", "bar", true).unwrap();
        assert_eq!(got, "bar bar");
    }

    #[test]
    fn test_apply_edit_replace_all_not_found() {
        let err = apply_edit("hello", "foo", "bar", true).unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    // ---- apply_ops ----------------------------------------------------

    #[test]
    fn test_apply_ops_write_creates_file_with_parent_dirs() {
        let dir = tempdir().unwrap();
        let ops = vec![write("/abs/sub/a.rs", "hi")];
        apply_ops(&ops, dir.path()).unwrap();
        let got = fs::read_to_string(dir.path().join("abs/sub/a.rs")).unwrap();
        assert_eq!(got, "hi");
    }

    #[test]
    fn test_apply_ops_edit_after_write() {
        let dir = tempdir().unwrap();
        let ops = vec![
            write("/a.rs", "hello world"),
            edit("/a.rs", "world", "rust", false),
        ];
        apply_ops(&ops, dir.path()).unwrap();
        let got = fs::read_to_string(dir.path().join("a.rs")).unwrap();
        assert_eq!(got, "hello rust");
    }

    #[test]
    fn test_apply_ops_edit_missing_file_reports_failure() {
        let dir = tempdir().unwrap();
        let ops = vec![edit("/never_written.rs", "x", "y", false)];
        let err = apply_ops(&ops, dir.path()).unwrap_err();
        assert!(err.to_string().contains("1 op(s) failed"));
    }

    #[test]
    fn test_apply_ops_edit_old_not_found_reports_failure() {
        let dir = tempdir().unwrap();
        let ops = vec![
            write("/a.rs", "hello"),
            edit("/a.rs", "missing", "y", false),
        ];
        let err = apply_ops(&ops, dir.path()).unwrap_err();
        assert!(err.to_string().contains("1 op(s) failed"));
        // The earlier write must have succeeded — continue-on-error.
        assert!(dir.path().join("a.rs").exists());
    }

    #[test]
    fn test_apply_ops_empty_input() {
        let dir = tempdir().unwrap();
        apply_ops(&[], dir.path()).unwrap();
    }

    // ---- extract_file --------------------------------------------------

    #[test]
    fn test_extract_file_write_then_edits() {
        let dir = tempdir().unwrap();
        let out = dir.path().join("final.rs");
        let ops = vec![
            write("/a.rs", "hello world\nfn main() {}\n"),
            edit("/a.rs", "world", "rust", false),
        ];
        extract_file(&ops, "/a.rs", Some(&out)).unwrap();
        let got = fs::read_to_string(&out).unwrap();
        assert_eq!(got, "hello rust\nfn main() {}\n");
    }

    #[test]
    fn test_extract_file_re_write_drops_earlier_edits() {
        // A later `Write` should reset the buffer — the final content
        // is whatever the LAST Write+Edit chain produces.
        let dir = tempdir().unwrap();
        let out = dir.path().join("final.rs");
        let ops = vec![
            write("/a.rs", "v1"),
            edit("/a.rs", "v1", "v1-edited", false),
            write("/a.rs", "v2"),
        ];
        extract_file(&ops, "/a.rs", Some(&out)).unwrap();
        let got = fs::read_to_string(&out).unwrap();
        assert_eq!(got, "v2");
    }

    #[test]
    fn test_extract_file_no_matching_ops_errors() {
        let ops = vec![write("/a.rs", "hi")];
        let err = extract_file(&ops, "/b.rs", None).unwrap_err();
        assert!(err.to_string().contains("no recorded edits"));
    }

    #[test]
    fn test_extract_file_only_edits_errors() {
        let ops = vec![edit("/a.rs", "x", "y", false)];
        let err = extract_file(&ops, "/a.rs", None).unwrap_err();
        assert!(err.to_string().contains("was never written in-session"));
    }

    #[test]
    fn test_extract_file_path_suffix_match() {
        // User asks for `a.rs` while the recorded op uses an absolute
        // path — the suffix-based matcher must connect the two.
        let dir = tempdir().unwrap();
        let out = dir.path().join("final.rs");
        let ops = vec![write("/home/u/proj/a.rs", "ok")];
        extract_file(&ops, "a.rs", Some(&out)).unwrap();
        assert_eq!(fs::read_to_string(&out).unwrap(), "ok");
    }

    // ---- summary_line / scale_bar --------------------------------------

    #[test]
    fn test_summary_line_full() {
        assert_eq!(
            summary_line(7, 412, 88),
            "7 files changed, 412 insertions(+), 88 deletions(-)"
        );
    }

    #[test]
    fn test_summary_line_singular_file_and_insertion() {
        assert_eq!(summary_line(1, 1, 0), "1 file changed, 1 insertion(+)");
    }

    #[test]
    fn test_summary_line_omits_zero_insertions() {
        assert_eq!(summary_line(2, 0, 5), "2 files changed, 5 deletions(-)");
    }

    #[test]
    fn test_summary_line_omits_zero_deletions() {
        assert_eq!(summary_line(1, 3, 0), "1 file changed, 3 insertions(+)");
    }

    #[test]
    fn test_summary_line_zero_zero() {
        assert_eq!(summary_line(0, 0, 0), "0 files changed");
    }

    #[test]
    fn test_scale_bar_no_scale() {
        assert_eq!(scale_bar(3, 2, 1.0), (3, 2));
    }

    #[test]
    fn test_scale_bar_preserves_signal_when_tiny() {
        // Heavy file dominates the scale; tiny file's 1 insertion
        // would round to zero but is bumped to 1.
        let (plus, minus) = scale_bar(1, 0, 0.1);
        assert_eq!((plus, minus), (1, 0));
    }

    #[test]
    fn test_scale_bar_zero_changes() {
        assert_eq!(scale_bar(0, 0, 1.0), (0, 0));
    }

    // ---- print_stat ----------------------------------------------------
    //
    // The human formatter goes through `colored::ColoredString`, which
    // emits ANSI escapes only when the global override is on.  Tests
    // run in the default (off) mode, so we can compare against plain
    // text.

    fn render_stat_human(ops: &[FileOp]) -> String {
        let files = edits::stat(ops);
        render_stat_human_string(&files)
    }

    #[test]
    fn test_render_stat_layout() {
        // /a: write "1" (1 ins) + edit 1→2 (1 ins, 1 del) + edit 2→3
        // (1 ins, 1 del) = 3 ins, 2 del, N=5.
        // /b: write "x" (1 ins) = 1 ins, 0 del, N=1.
        let ops = vec![
            write("/a", "1"),
            edit("/a", "1", "2", false),
            edit("/a", "2", "3", false),
            write("/b", "x"),
        ];
        let got = render_stat_human(&ops);
        // File lines start with one extra space (relative to the
        // summary line) which `indoc!` preserves after stripping the
        // common indent — that one space is git's per-file prefix.
        let expected = indoc! {"
              /a | 5 +++--
              /b | 1 +
             2 files changed, 4 insertions(+), 2 deletions(-)
        "};
        assert_output_eq(&got, expected);
    }

    #[test]
    fn test_render_stat_edited_only() {
        // Pure edit: del 1, ins 1 → N=2.
        let ops = vec![edit("/a", "x", "y", false)];
        let got = render_stat_human(&ops);
        let expected = indoc! {"
              /a | 2 +-
             1 file changed, 1 insertion(+), 1 deletion(-)
        "};
        assert_output_eq(&got, expected);
    }

    #[test]
    fn test_render_stat_insertions_only_omits_deletions_clause() {
        let ops = vec![write("/a", "x\ny\nz\n")];
        let got = render_stat_human(&ops);
        let expected = indoc! {"
              /a | 3 +++
             1 file changed, 3 insertions(+)
        "};
        assert_output_eq(&got, expected);
    }

    #[test]
    fn test_render_stat_deletions_only_omits_insertions_clause() {
        // Edit `old` is multi-line, `new` is empty → pure deletion.
        let ops = vec![edit("/a", "a\nb\nc", "", false)];
        let got = render_stat_human(&ops);
        let expected = indoc! {"
              /a | 3 ---
             1 file changed, 3 deletions(-)
        "};
        assert_output_eq(&got, expected);
    }

    #[test]
    fn test_render_stat_path_column_padding_aligns() {
        // Two files of different path lengths: shorter one must be
        // padded to the longer one's width.
        let ops = vec![write("/short", "a"), write("/much/longer/path", "b")];
        let got = render_stat_human(&ops);
        let expected = indoc! {"
              /short            | 1 +
              /much/longer/path | 1 +
             2 files changed, 2 insertions(+)
        "};
        assert_output_eq(&got, expected);
    }

    #[test]
    fn test_render_stat_empty() {
        let got = render_stat_human(&[]);
        let expected = " 0 files changed\n";
        assert_output_eq(&got, expected);
    }

    // ---- print_patch ---------------------------------------------------

    fn render_patch_human(ops: &[FileOp]) -> String {
        // Mirror print_patch's Human branch into a string buffer so we
        // can diff exactly.  Color is off in tests so plain text comes
        // through.
        let mut out = String::new();
        if ops.is_empty() {
            out.push_str("no file ops recorded in this session\n");
            return out;
        }
        for (i, op) in ops.iter().enumerate() {
            if i > 0 {
                out.push('\n');
            }
            match op {
                FileOp::Write { path, content } => {
                    out.push_str(&format!("=== {} (write) ===\n", path));
                    out.push_str(content);
                    if !content.ends_with('\n') {
                        out.push('\n');
                    }
                }
                FileOp::Edit {
                    path,
                    old,
                    new,
                    replace_all,
                } => {
                    let suffix = if *replace_all { ", replace-all" } else { "" };
                    out.push_str(&format!("=== {} (edit{}) ===\n", path, suffix));
                    out.push_str("--- old\n");
                    out.push_str(old);
                    if !old.ends_with('\n') {
                        out.push('\n');
                    }
                    out.push_str("--- new\n");
                    out.push_str(new);
                    if !new.ends_with('\n') {
                        out.push('\n');
                    }
                }
            }
        }
        out
    }

    #[test]
    fn test_render_patch_write_and_edit() {
        let ops = vec![
            write("/a.rs", "fn main() {}\n"),
            edit("/a.rs", "main", "run", false),
        ];
        let got = render_patch_human(&ops);
        let expected = indoc! {"
            === /a.rs (write) ===
            fn main() {}

            === /a.rs (edit) ===
            --- old
            main
            --- new
            run
        "};
        assert_output_eq(&got, expected);
    }

    #[test]
    fn test_render_patch_replace_all_header() {
        let ops = vec![edit("/a.rs", "foo", "bar", true)];
        let got = render_patch_human(&ops);
        let expected = indoc! {"
            === /a.rs (edit, replace-all) ===
            --- old
            foo
            --- new
            bar
        "};
        assert_output_eq(&got, expected);
    }

    #[test]
    fn test_render_patch_empty() {
        assert_eq!(
            render_patch_human(&[]),
            "no file ops recorded in this session\n"
        );
    }
}

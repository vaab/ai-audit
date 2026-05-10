use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;
use std::time::UNIX_EPOCH;

const DAY_SECS: i64 = 86_400;
/// On-disk cache schema version.  Bumped to 2 alongside the
/// session-index v2 restructure: multi-cwd attribution now spreads
/// events across all the cwds a session touched, which can change
/// the computed ``t_first`` / ``t_last`` for a given category even
/// when the contributing session files are byte-identical.  The
/// fingerprint mechanism only catches file-content changes, so
/// algorithm changes must be expressed via this version bump to
/// invalidate caches written under the previous attribution.
const SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bounds {
    pub t_first: i64,
    pub t_last: i64,
    pub days: BTreeSet<i64>,
}

impl Bounds {
    pub fn from_timestamps(timestamps: &[i64]) -> Option<Self> {
        let t_first = *timestamps.iter().min()?;
        let t_last = *timestamps.iter().max()?;
        let days = timestamps.iter().map(|&ts| day_of(ts)).collect();
        Some(Self {
            t_first,
            t_last,
            days,
        })
    }
}

/// Cached state for one ident.
///
/// `bounds` is `None` when the ident has zero events ever — we still
/// remember the fingerprint so subsequent runs with unchanged files
/// skip the (possibly expensive) "no events found" rescan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheEntry {
    pub fingerprint: String,
    pub bounds: Option<Bounds>,
    pub last_run_at: i64,
}

#[derive(Debug, Clone)]
pub struct Cache {
    root: PathBuf,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheFile {
    schema_version: u32,
    fingerprint: String,
    /// `false` when the ident has zero events ever.  Defaults to `true`
    /// for backward-compat with cache files written before this field
    /// existed (those files always represented a non-empty bound set).
    #[serde(default = "default_true")]
    has_events: bool,
    t_first: i64,
    t_last: i64,
    day_ranges: Vec<[i64; 2]>,
    last_run_at: i64,
}

pub fn day_of(ts: i64) -> i64 {
    ts.div_euclid(DAY_SECS)
}

pub fn set_to_ranges(days: &BTreeSet<i64>) -> Vec<[i64; 2]> {
    let mut ranges = Vec::new();
    let mut iter = days.iter().copied();
    let Some(mut start) = iter.next() else {
        return ranges;
    };
    let mut end = start;
    for day in iter {
        if day == end + 1 {
            end = day;
        } else {
            ranges.push([start, end]);
            start = day;
            end = day;
        }
    }
    ranges.push([start, end]);
    ranges
}

pub fn ranges_to_set(ranges: &[[i64; 2]]) -> BTreeSet<i64> {
    let mut days = BTreeSet::new();
    for [start, end] in ranges {
        for day in *start..=*end {
            days.insert(day);
        }
    }
    days
}

pub fn intervals_for(bounds: &Bounds, now: i64) -> Vec<(Option<i64>, Option<i64>)> {
    if bounds.days.is_empty() {
        return Vec::new();
    }

    let mut intervals = vec![(None, Some(bounds.t_first))];
    let mut iter = bounds.days.iter().copied();
    let Some(mut previous_day) = iter.next() else {
        return Vec::new();
    };

    for current_day in iter {
        if current_day - previous_day > 1 {
            intervals.push((
                Some((previous_day + 1) * DAY_SECS),
                Some(current_day * DAY_SECS),
            ));
        }
        previous_day = current_day;
    }

    // Trailing empty zone starts strictly AFTER the last event.  The
    // wire spec uses half-open `[start, end)` with start INCLUSIVE, so
    // emitting `[t_last, now)` would falsely claim the event at t_last
    // is empty — fyl's safety check would then refuse the merge.  The
    // tightened guard also drops degenerate intervals when now is one
    // tick (or less) past t_last.
    let trailing_start = bounds.t_last + 1;
    if trailing_start < now {
        intervals.push((Some(trailing_start), Some(now)));
    }

    intervals
}

pub fn format_record(ident: &str, intervals: &[(Option<i64>, Option<i64>)]) -> String {
    assert!(
        !intervals.is_empty(),
        "format_record requires non-empty intervals"
    );

    let mut out = String::from("\0");
    out.push_str(ident);
    out.push_str("\0{\"kind\":\"empty\",\"intervals\":[");
    for (i, (a, b)) in intervals.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('[');
        match a {
            Some(v) => out.push_str(&v.to_string()),
            None => out.push_str("null"),
        }
        out.push(',');
        match b {
            Some(v) => out.push_str(&v.to_string()),
            None => out.push_str("null"),
        }
        out.push(']');
    }
    out.push_str("]}\0");
    out
}

pub fn sanitize_ident(ident: &str) -> String {
    ident.replace('@', "__").replace('/', "_")
}

impl Cache {
    pub fn new() -> Result<Self> {
        let root = dirs::cache_dir()
            .context("Could not find cache directory")?
            .join("ai-audit")
            .join("empty-segments");
        Ok(Self { root })
    }

    pub fn load(&self, ident: &str) -> Option<CacheEntry> {
        let path = self.path_for(ident);
        let content = fs::read_to_string(path).ok()?;
        let file: CacheFile = serde_json::from_str(&content).ok()?;
        if file.schema_version != SCHEMA_VERSION {
            return None;
        }
        let bounds = if file.has_events {
            Some(Bounds {
                t_first: file.t_first,
                t_last: file.t_last,
                days: ranges_to_set(&file.day_ranges),
            })
        } else {
            None
        };
        Some(CacheEntry {
            fingerprint: file.fingerprint,
            bounds,
            last_run_at: file.last_run_at,
        })
    }

    pub fn save(&self, ident: &str, entry: &CacheEntry) -> Result<()> {
        fs::create_dir_all(&self.root)
            .with_context(|| format!("Failed to create {}", self.root.display()))?;

        let path = self.path_for(ident);
        let tmp_path = self.root.join(format!(
            ".{}.tmp-{}-{}",
            sanitize_ident(ident),
            std::process::id(),
            entry.last_run_at
        ));
        let (has_events, t_first, t_last, day_ranges) = match &entry.bounds {
            Some(b) => (true, b.t_first, b.t_last, set_to_ranges(&b.days)),
            None => (false, 0, 0, Vec::new()),
        };
        let payload = CacheFile {
            schema_version: SCHEMA_VERSION,
            fingerprint: entry.fingerprint.clone(),
            has_events,
            t_first,
            t_last,
            day_ranges,
            last_run_at: entry.last_run_at,
        };
        let json = serde_json::to_string(&payload)?;
        fs::write(&tmp_path, json)
            .with_context(|| format!("Failed to write {}", tmp_path.display()))?;
        fs::rename(&tmp_path, &path).with_context(|| {
            format!(
                "Failed to rename {} to {}",
                tmp_path.display(),
                path.display()
            )
        })?;
        Ok(())
    }

    pub fn fingerprint_for_files(files: &[PathBuf]) -> Result<String> {
        let mut tuples = Vec::new();
        for path in files {
            let metadata =
                fs::metadata(path).with_context(|| format!("Failed to stat {}", path.display()))?;
            let modified = metadata
                .modified()
                .with_context(|| format!("Failed to read mtime for {}", path.display()))?;
            let mtime_ns = modified
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            tuples.push(format!(
                "{}\t{}\t{}\n",
                path.display(),
                mtime_ns,
                metadata.len()
            ));
        }
        tuples.sort();
        Ok(blake3::hash(tuples.concat().as_bytes())
            .to_hex()
            .to_string())
    }

    pub fn path_for(&self, ident: &str) -> PathBuf {
        self.root.join(format!("{}.json", sanitize_ident(ident)))
    }

    #[cfg(test)]
    fn new_at(root: PathBuf) -> Self {
        Self { root }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;
    use similar::TextDiff;
    use std::collections::BTreeSet;
    use tempfile::tempdir;

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

    fn bounds(times: &[i64]) -> Bounds {
        Bounds::from_timestamps(times).unwrap_or(Bounds {
            t_first: 0,
            t_last: 0,
            days: BTreeSet::new(),
        })
    }

    #[test]
    fn day_of_uses_euclidean_division() {
        assert_eq!(day_of(0), 0);
        assert_eq!(day_of(86_399), 0);
        assert_eq!(day_of(86_400), 1);
        assert_eq!(day_of(-1), -1);
        let ts = -86_400 + 123;
        assert_eq!(day_of(ts) * DAY_SECS, -86_400);
    }

    #[test]
    fn ranges_round_trip_for_empty_set() {
        let days = BTreeSet::new();
        assert_eq!(set_to_ranges(&days), Vec::<[i64; 2]>::new());
        assert_eq!(ranges_to_set(&[]), days);
    }

    #[test]
    fn ranges_round_trip_for_single_day() {
        let days = BTreeSet::from([42]);
        let ranges = set_to_ranges(&days);
        assert_eq!(ranges, vec![[42, 42]]);
        assert_eq!(ranges_to_set(&ranges), days);
    }

    #[test]
    fn ranges_round_trip_for_consecutive_run() {
        let days = BTreeSet::from([7, 8, 9]);
        let ranges = set_to_ranges(&days);
        assert_eq!(ranges, vec![[7, 9]]);
        assert_eq!(ranges_to_set(&ranges), days);
    }

    #[test]
    fn ranges_round_trip_for_disjoint_runs() {
        let days = BTreeSet::from([1, 2, 5, 8, 9]);
        let ranges = set_to_ranges(&days);
        assert_eq!(ranges, vec![[1, 2], [5, 5], [8, 9]]);
        assert_eq!(ranges_to_set(&ranges), days);
    }

    #[test]
    fn intervals_for_empty_bounds_is_empty() {
        let bounds = Bounds {
            t_first: 0,
            t_last: 0,
            days: BTreeSet::new(),
        };
        assert!(intervals_for(&bounds, 100).is_empty());
    }

    #[test]
    fn intervals_for_single_event_has_leading_and_trailing() {
        // Trailing empty zone starts at t + 1 (exclusive of the event ts).
        let t = 86_400 + 43_200;
        let now = 2 * DAY_SECS + 32_400;
        assert_eq!(
            intervals_for(&bounds(&[t]), now),
            vec![(None, Some(t)), (Some(t + 1), Some(now))]
        );
    }

    #[test]
    fn intervals_for_single_event_drops_trailing_when_now_equals_last() {
        let t = 100;
        assert_eq!(intervals_for(&bounds(&[t]), t), vec![(None, Some(t))]);
    }

    #[test]
    fn intervals_for_single_event_drops_trailing_when_now_before_last() {
        let t = 200;
        assert_eq!(intervals_for(&bounds(&[t]), 199), vec![(None, Some(t))]);
    }

    #[test]
    fn intervals_for_two_events_same_day_has_no_gap() {
        let bounds = bounds(&[100, 200]);
        assert_eq!(
            intervals_for(&bounds, 300),
            vec![(None, Some(100)), (Some(201), Some(300))]
        );
    }

    #[test]
    fn intervals_for_adjacent_days_has_no_gap() {
        let bounds = bounds(&[43_200, DAY_SECS + 10]);
        assert_eq!(
            intervals_for(&bounds, 2 * DAY_SECS),
            vec![
                (None, Some(43_200)),
                (Some(DAY_SECS + 11), Some(2 * DAY_SECS))
            ]
        );
    }

    #[test]
    fn intervals_for_three_day_gap_uses_exact_midnights() {
        let first = 12 * 3_600;
        let last = 3 * DAY_SECS + 1;
        let bounds = bounds(&[first, last]);
        assert_eq!(
            intervals_for(&bounds, 4 * DAY_SECS),
            vec![
                (None, Some(first)),
                (Some(DAY_SECS), Some(3 * DAY_SECS)),
                (Some(last + 1), Some(4 * DAY_SECS)),
            ]
        );
    }

    /// Spec-pin: the trailing empty zone must NOT include the timestamp
    /// of the last event itself.  fyl's wire spec is half-open
    /// `[start, end)` with start INCLUSIVE; emitting `[t_last, now)`
    /// would falsely claim the event at t_last is empty, and fyl's
    /// safety check rejects the merge with "empty-zone assertion
    /// contradicts existing event data".
    ///
    /// Regression test for the fyl read failure observed on
    /// `ai-audit:claudecode-msg@/home/vaab` — the category had a single
    /// event at `2026-03-30T23:18:05Z`; activity-org's emission of
    /// `[1774912685, now)` collided with the stored event at the same
    /// timestamp.  After the fix, the emitted lower bound is
    /// `t_last + 1`, which the safety check accepts.
    #[test]
    fn intervals_for_trailing_empty_zone_excludes_last_event_timestamp() {
        let t_last = 1_774_912_685; // 2026-03-30T23:18:05Z
        let now = 1_779_157_519; // 2026-05-10T02:25:19Z
        let bounds = bounds(&[t_last]);
        let intervals = intervals_for(&bounds, now);
        // Trailing interval starts at t_last + 1, never at t_last.
        assert_eq!(
            intervals,
            vec![(None, Some(t_last)), (Some(t_last + 1), Some(now))]
        );
        // And explicitly: t_last itself is NOT inside any asserted
        // empty interval.
        for (start, end) in &intervals {
            let start = start.unwrap_or(i64::MIN);
            let end = end.unwrap_or(i64::MAX);
            assert!(
                t_last < start || t_last >= end,
                "t_last ({t_last}) lies inside asserted empty interval [{start}, {end})"
            );
        }
    }

    /// After the fix, the trailing interval is omitted entirely when
    /// `now == t_last + 1` (would otherwise produce a degenerate
    /// `[t_last + 1, t_last + 1)` zero-width interval).
    #[test]
    fn intervals_for_no_trailing_zone_when_now_is_one_past_last_event() {
        let t_last = 1000;
        let now = t_last + 1;
        let bounds = bounds(&[t_last]);
        assert_eq!(
            intervals_for(&bounds, now),
            vec![(None, Some(t_last))],
            "no trailing empty zone should be emitted when now == t_last + 1"
        );
    }

    #[test]
    fn intervals_for_multiple_gaps_are_ordered() {
        let bounds = bounds(&[100, 3 * DAY_SECS + 1, 6 * DAY_SECS + 2]);
        assert_eq!(
            intervals_for(&bounds, 7 * DAY_SECS),
            vec![
                (None, Some(100)),
                (Some(DAY_SECS), Some(3 * DAY_SECS)),
                (Some(4 * DAY_SECS), Some(6 * DAY_SECS)),
                (Some(6 * DAY_SECS + 3), Some(7 * DAY_SECS)),
            ]
        );
    }

    #[test]
    fn format_record_single_interval_is_exact() {
        assert_output_eq(
            &format_record("X", &[(None, Some(100))]),
            "\0X\0{\"kind\":\"empty\",\"intervals\":[[null,100]]}\0",
        );
    }

    #[test]
    fn format_record_mixed_bounds_is_exact() {
        assert_output_eq(
            &format_record("Y", &[(Some(1), None)]),
            "\0Y\0{\"kind\":\"empty\",\"intervals\":[[1,null]]}\0",
        );
    }

    #[test]
    fn format_record_multiple_intervals_have_no_trailing_comma() {
        assert_output_eq(
            &format_record("Z", &[(None, Some(1)), (Some(2), Some(3))]),
            "\0Z\0{\"kind\":\"empty\",\"intervals\":[[null,1],[2,3]]}\0",
        );
    }

    #[test]
    fn format_record_matches_reference_golden() {
        assert_output_eq(
            &format_record(
                "deep-seg-2025-06-06",
                &[(None, Some(1_717_545_600)), (Some(1_717_631_999), None)],
            ),
            "\0deep-seg-2025-06-06\0{\"kind\":\"empty\",\"intervals\":[[null,1717545600],[1717631999,null]]}\0",
        );
    }

    #[test]
    fn sanitize_ident_replaces_special_chars() {
        assert_eq!(
            sanitize_ident("claudecode-msg@rs/ai-audit"),
            "claudecode-msg__rs_ai-audit"
        );
        assert_eq!(sanitize_ident("pi-msg@unknown"), "pi-msg__unknown");
        let once = sanitize_ident("claudecode-msg@rs/ai-audit");
        assert_eq!(sanitize_ident(&once), once);
    }

    #[test]
    fn fingerprint_for_files_is_deterministic_and_order_independent() {
        let dir = tempdir().unwrap();
        let first = dir.path().join("a.txt");
        let second = dir.path().join("b.txt");
        fs::write(
            &first,
            indoc! {"
            alpha
        "},
        )
        .unwrap();
        fs::write(
            &second,
            indoc! {"
            beta
        "},
        )
        .unwrap();

        let hash_a = Cache::fingerprint_for_files(&[first.clone(), second.clone()]).unwrap();
        let hash_b = Cache::fingerprint_for_files(&[second, first]).unwrap();
        assert_eq!(hash_a, hash_b);
    }

    #[test]
    fn fingerprint_for_files_changes_with_mtime_and_size() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("a.txt");
        fs::write(&file, "one").unwrap();
        let initial = Cache::fingerprint_for_files(std::slice::from_ref(&file)).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(5));
        fs::write(&file, "two-two").unwrap();
        let changed = Cache::fingerprint_for_files(std::slice::from_ref(&file)).unwrap();
        assert_ne!(initial, changed);
    }

    #[test]
    fn fingerprint_for_files_handles_empty_input() {
        assert_eq!(
            Cache::fingerprint_for_files(&[]).unwrap(),
            blake3::hash(&[]).to_hex().to_string()
        );
    }

    #[test]
    fn cache_round_trip_and_directory_creation() {
        let dir = tempdir().unwrap();
        let cache = Cache::new_at(dir.path().join("nested/cache"));
        let entry = CacheEntry {
            fingerprint: "abc".to_string(),
            bounds: Some(bounds(&[100, 300, 400])),
            last_run_at: 500,
        };
        cache.save("claudecode-msg@proj", &entry).unwrap();
        assert_eq!(cache.load("claudecode-msg@proj"), Some(entry));
    }

    #[test]
    fn cache_round_trip_for_zero_event_ident() {
        // A cache entry with no bounds remembers "this ident has zero
        // events ever for the recorded fingerprint" — so subsequent
        // runs skip the rescan entirely.
        let dir = tempdir().unwrap();
        let cache = Cache::new_at(dir.path().to_path_buf());
        let entry = CacheEntry {
            fingerprint: "deadbeef".to_string(),
            bounds: None,
            last_run_at: 42,
        };
        cache.save("opencode-msg@empty", &entry).unwrap();
        assert_eq!(cache.load("opencode-msg@empty"), Some(entry));
    }

    #[test]
    fn cache_load_treats_missing_has_events_field_as_true() {
        // The serde default on ``has_events`` keeps current-schema
        // files readable when an early-2 binary wrote them before
        // the field was added.  Without the default, those files
        // would silently be rejected as malformed.
        let dir = tempdir().unwrap();
        let cache = Cache::new_at(dir.path().to_path_buf());
        let path = cache.path_for("no-has-events");
        fs::write(
            &path,
            r#"{"schema_version":2,"fingerprint":"x","t_first":100,"t_last":200,"day_ranges":[[0,0]],"last_run_at":300}"#,
        )
        .unwrap();
        let loaded = cache.load("no-has-events").expect("file should load");
        assert!(loaded.bounds.is_some());
        let b = loaded.bounds.unwrap();
        assert_eq!(b.t_first, 100);
        assert_eq!(b.t_last, 200);
    }

    #[test]
    fn cache_load_rejects_schema_v1_caches() {
        // v1 caches were written under different attribution rules
        // (single-cwd-per-session) and may hold bounds that no longer
        // match what the current algorithm computes.  They MUST be
        // rejected on load so the next run does a fresh recompute.
        let dir = tempdir().unwrap();
        let cache = Cache::new_at(dir.path().to_path_buf());
        let path = cache.path_for("v1-stale");
        fs::write(
            &path,
            r#"{"schema_version":1,"fingerprint":"x","t_first":100,"t_last":200,"day_ranges":[[0,0]],"last_run_at":300}"#,
        )
        .unwrap();
        assert!(
            cache.load("v1-stale").is_none(),
            "v1 caches must be invalidated"
        );
    }

    #[test]
    fn cache_load_missing_corrupt_and_wrong_schema_are_misses() {
        let dir = tempdir().unwrap();
        let cache = Cache::new_at(dir.path().to_path_buf());
        assert!(cache.load("missing").is_none());

        let corrupt = cache.path_for("corrupt");
        fs::write(&corrupt, "not-json").unwrap();
        assert!(cache.load("corrupt").is_none());

        let wrong = cache.path_for("wrong");
        fs::write(
            &wrong,
            r#"{"schema_version":999,"fingerprint":"x","t_first":1,"t_last":2,"day_ranges":[[1,1]],"last_run_at":3}"#,
        )
        .unwrap();
        assert!(cache.load("wrong").is_none());
    }

    #[test]
    fn cache_save_leaves_no_partial_files() {
        let dir = tempdir().unwrap();
        let cache = Cache::new_at(dir.path().to_path_buf());
        let entry = CacheEntry {
            fingerprint: "abc".to_string(),
            bounds: Some(bounds(&[100])),
            last_run_at: 123,
        };
        cache.save("pi-msg@proj", &entry).unwrap();

        let entries = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
            .collect::<Vec<_>>();
        assert_eq!(entries, vec!["pi-msg__proj.json".to_string()]);
    }
}

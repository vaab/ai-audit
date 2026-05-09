use ai_audit::activity::{self, ActivityData};
use ai_audit::config::Config;
use ai_audit::empty_segments::{self, Bounds, Cache, CacheEntry};
use serde::Serialize;
use similar::TextDiff;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tempfile::tempdir;

static ENV_LOCK: Mutex<()> = Mutex::new(());

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

struct EnvGuard {
    home: Option<String>,
    xdg_cache_home: Option<String>,
    xdg_config_home: Option<String>,
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.home {
            Some(value) => unsafe {
                std::env::set_var("HOME", value);
            },
            None => unsafe {
                std::env::remove_var("HOME");
            },
        }
        match &self.xdg_cache_home {
            Some(value) => unsafe {
                std::env::set_var("XDG_CACHE_HOME", value);
            },
            None => unsafe {
                std::env::remove_var("XDG_CACHE_HOME");
            },
        }
        match &self.xdg_config_home {
            Some(value) => unsafe {
                std::env::set_var("XDG_CONFIG_HOME", value);
            },
            None => unsafe {
                std::env::remove_var("XDG_CONFIG_HOME");
            },
        }
    }
}

fn with_temp_home(home: &Path) -> EnvGuard {
    let guard = EnvGuard {
        home: std::env::var("HOME").ok(),
        xdg_cache_home: std::env::var("XDG_CACHE_HOME").ok(),
        xdg_config_home: std::env::var("XDG_CONFIG_HOME").ok(),
    };
    unsafe {
        std::env::set_var("HOME", home);
        std::env::set_var("XDG_CACHE_HOME", home.join(".cache"));
        std::env::set_var("XDG_CONFIG_HOME", home.join(".config"));
    }
    guard
}

fn write_claude_session(
    home: &Path,
    project_dir: &str,
    session_id: &str,
    messages: &[(&str, &str)],
) -> PathBuf {
    let dir = home
        .join(".claude/projects")
        .join(format!("fixture-{}", session_id));
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{}.jsonl", session_id));
    let mut lines = Vec::new();
    for (timestamp, content) in messages {
        lines.push(format!(
            r#"{{"type":"user","timestamp":"{}","message":{{"role":"user","content":"{}"}},"cwd":"{}"}}"#,
            timestamp, content, project_dir
        ));
    }
    fs::write(&path, lines.join("\n")).unwrap();
    path
}

fn full_timespan() -> (
    chrono::DateTime<chrono::FixedOffset>,
    chrono::DateTime<chrono::FixedOffset>,
) {
    (
        chrono::DateTime::parse_from_rfc3339("1970-01-01T00:00:00Z").unwrap(),
        chrono::DateTime::parse_from_rfc3339("2100-01-01T00:00:00Z").unwrap(),
    )
}

#[derive(Serialize)]
struct TestNulPayload<'a> {
    session_id: &'a str,
    #[serde(flatten)]
    data: &'a ActivityData,
}

fn assemble_nul_stream(
    config: &Config,
    requested_idents: &[String],
    now: i64,
    scan_count: &mut usize,
) -> String {
    let (start, end) = full_timespan();
    let events = activity::fetch_activities(config, start, end, requested_idents, &[]).unwrap();
    let session_index = activity::build_full_session_index(config);
    let cache = Cache::new().unwrap();
    let mut cached_bounds = std::collections::HashMap::new();
    let mut misses = Vec::new();

    for ident in requested_idents {
        let files = activity::enumerate_files_for_ident_with_index(ident, &session_index);
        let fingerprint = Cache::fingerprint_for_files(&files).unwrap();
        if let Some(entry) = cache.load(ident) {
            if entry.fingerprint == fingerprint {
                cached_bounds.insert(ident.clone(), entry.bounds);
                continue;
            }
        }
        misses.push((ident.clone(), fingerprint));
    }

    let fresh_timestamps = if misses.is_empty() {
        std::collections::HashMap::new()
    } else {
        *scan_count += 1;
        activity::fetch_all_event_timestamps_with_index(
            config,
            &misses
                .iter()
                .map(|(ident, _)| ident.clone())
                .collect::<Vec<_>>(),
            &session_index,
        )
        .unwrap()
    };

    let mut out = String::new();
    for ident in requested_idents {
        let bounds_opt: Option<Bounds> = match cached_bounds.remove(ident) {
            Some(b) => b,
            None => fresh_timestamps
                .get(ident)
                .and_then(|timestamps| Bounds::from_timestamps(timestamps)),
        };
        if let Some((_, fingerprint)) = misses.iter().find(|(name, _)| name == ident) {
            cache
                .save(
                    ident,
                    &CacheEntry {
                        fingerprint: fingerprint.clone(),
                        bounds: bounds_opt.clone(),
                        last_run_at: now,
                    },
                )
                .unwrap();
        }
        let Some(bounds) = bounds_opt else {
            continue;
        };
        let intervals = empty_segments::intervals_for(&bounds, now);
        if !intervals.is_empty() {
            out.push_str(&empty_segments::format_record(ident, &intervals));
        }
    }

    for event in events {
        let json = serde_json::to_string(&TestNulPayload {
            session_id: &event.session_id,
            data: &event.data,
        })
        .unwrap();
        out.push_str(&format!("{}\0{}\0{}\0", event.timestamp, event.ident, json));
    }
    out
}

#[test]
fn empty_segment_control_records_precede_events_and_cache_hits() {
    let _lock = ENV_LOCK.lock().unwrap();
    let temp = tempdir().unwrap();
    let _guard = with_temp_home(temp.path());

    let _one = write_claude_session(
        temp.path(),
        "/proj-one",
        "session-one",
        &[
            ("1970-01-02T12:00:00Z", "alpha"),
            ("1970-01-05T12:00:00Z", "beta"),
        ],
    );
    let _two = write_claude_session(
        temp.path(),
        "/proj-two",
        "session-two",
        &[("1970-01-03T06:00:00Z", "gamma")],
    );

    let config = Config::default();
    let requested_idents = vec![
        "claudecode-msg@/proj-one".to_string(),
        "claudecode-msg@/proj-two".to_string(),
        "claudecode-msg@/proj-zero".to_string(),
    ];
    let now = 432_000;

    let mut first_scan_count = 0;
    let first = assemble_nul_stream(&config, &requested_idents, now, &mut first_scan_count);
    assert_eq!(first_scan_count, 1);

    let expected_prefix =
        "\0claudecode-msg@/proj-one\0{\"kind\":\"empty\",\"intervals\":[[null,129600],[172800,345600],[388800,432000]]}\0";
    assert!(first.starts_with(expected_prefix));
    assert!(first.contains("\0claudecode-msg@/proj-two\0{\"kind\":\"empty\""));
    assert!(!first.contains("claudecode-msg@/proj-zero\0{\"kind\":\"empty\""));

    let first_event_marker = "129600\0claudecode-msg@/proj-one\0";
    let pos_control = first.find("\0claudecode-msg@/proj-two\0").unwrap();
    let pos_event = first.find(first_event_marker).unwrap();
    assert!(
        pos_control < pos_event,
        "control records must precede events"
    );

    let cache_file = temp
        .path()
        .join(".cache/ai-audit/empty-segments/claudecode-msg___proj-one.json");
    assert!(cache_file.exists());

    let mut second_scan_count = 0;
    let second = assemble_nul_stream(&config, &requested_idents[..2], now, &mut second_scan_count);
    assert_eq!(second_scan_count, 0);
    assert_output_eq(&second, &first);
}

#[test]
fn fetch_all_event_timestamps_and_control_record_are_exact_for_single_ident() {
    let _lock = ENV_LOCK.lock().unwrap();
    let temp = tempdir().unwrap();
    let _guard = with_temp_home(temp.path());

    write_claude_session(
        temp.path(),
        "/proj-gap",
        "session-gap",
        &[
            ("1970-01-02T12:00:00Z", "alpha"),
            ("1970-01-05T12:00:00Z", "beta"),
        ],
    );

    let config = Config::default();
    let ident = "claudecode-msg@/proj-gap".to_string();
    let timestamps =
        activity::fetch_all_event_timestamps(&config, std::slice::from_ref(&ident)).unwrap();
    assert_eq!(timestamps.get(&ident), Some(&vec![129_600, 388_800]));

    let bounds = Bounds::from_timestamps(timestamps.get(&ident).unwrap()).unwrap();
    let record =
        empty_segments::format_record(&ident, &empty_segments::intervals_for(&bounds, 432_000));
    assert_output_eq(
        &record,
        "\0claudecode-msg@/proj-gap\0{\"kind\":\"empty\",\"intervals\":[[null,129600],[172800,345600],[388800,432000]]}\0",
    );
}

#[test]
fn enumerate_is_called_with_shared_index() {
    let _lock = ENV_LOCK.lock().unwrap();
    let temp = tempdir().unwrap();
    let _guard = with_temp_home(temp.path());

    for n in 0..100 {
        let project = format!("/proj-{n}");
        let session = format!("session-{n}");
        write_claude_session(
            temp.path(),
            &project,
            &session,
            &[("1970-01-02T12:00:00Z", "alpha")],
        );
    }

    let requested_idents = (0..100)
        .map(|n| format!("claudecode-msg@/proj-{n}"))
        .collect::<Vec<_>>();
    let mut scan_count = 0;
    let out = assemble_nul_stream(
        &Config::default(),
        &requested_idents,
        432_000,
        &mut scan_count,
    );

    assert_eq!(scan_count, 1);
    assert!(out.starts_with("\0claudecode-msg@/proj-0\0"));
    assert!(out.contains("claudecode-msg@/proj-99\0{\"session_id\":\"session-99\""));
}

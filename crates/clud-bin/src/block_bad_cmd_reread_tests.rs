use super::*;
use serde_json::json;
use tempfile::tempdir;

fn whole(at: u64) -> Span {
    Span {
        start: 1,
        end: u64::MAX,
        at,
    }
}

fn lines(start: u64, end: u64, at: u64) -> Span {
    Span { start, end, at }
}

fn entry(len: u64, secs: u64, nanos: u32, at: u64, span: Span) -> Entry {
    Entry {
        len,
        secs,
        nanos,
        at,
        name: "service.py".to_string(),
        spans: vec![span],
        refusals: 0,
    }
}

#[test]
fn first_read_is_recorded_then_the_unchanged_repeat_is_denied() {
    let current = entry(79_497, 1_700_000_000, 5, 100, whole(100));
    let (verdict, kept) = advance(None, current.clone(), whole(100));
    assert_eq!(verdict, Verdict::Record);
    assert_eq!(kept.spans, vec![whole(100)]);

    let previous = entry(79_497, 1_700_000_000, 5, 40, whole(40));
    let (verdict, kept) = advance(Some(&previous), current, whole(100));
    assert_eq!(
        verdict,
        Verdict::Deny {
            ago_secs: 60,
            kb: 77,
            span: whole(0)
        }
    );
    // The refusal itself is not a read, but it spends one unit of budget.
    assert_eq!(kept.refusals, 1);
}

#[test]
fn a_changed_file_allows_the_read_again() {
    let previous = entry(79_497, 1_700_000_000, 5, 40, whole(40));
    // mtime moved, so the agent edited the file: never refuse that read.
    let moved = entry(79_497, 1_700_000_099, 0, 100, whole(100));
    let (verdict, kept) = advance(Some(&previous), moved, whole(100));
    assert_eq!(verdict, Verdict::Record);
    assert_eq!(kept.refusals, 0);
    assert_eq!(kept.spans, vec![whole(100)]);

    // Same mtime but a different length is also a change.
    let regrown = entry(79_400, 1_700_000_099, 0, 100, whole(100));
    let (verdict, _) = advance(Some(&previous), regrown, whole(100));
    assert_eq!(verdict, Verdict::Record);
}

#[test]
fn an_overlapping_window_is_refused_even_though_the_key_differs() {
    // The habit: read 1-570, then 1-420, then 420-840. Both of the first two
    // windows are the same information; only the second is new ground.
    let first = entry(45_000, 7, 0, 100, lines(1, 420, 100));
    let wider = entry(45_000, 7, 0, 160, lines(1, 570, 160));
    let (verdict, kept) = advance(Some(&first), wider, lines(1, 570, 160));
    assert_eq!(verdict, Verdict::Record, "a wider window reads new lines");
    assert_eq!(kept.spans, vec![lines(1, 570, 100)]);

    let narrower = entry(45_000, 7, 0, 200, lines(1, 420, 200));
    let (verdict, _) = advance(Some(&kept), narrower, lines(1, 420, 200));
    assert_eq!(
        verdict,
        Verdict::Deny {
            ago_secs: 100,
            kb: 43,
            span: lines(1, 420, 0)
        }
    );
}

#[test]
fn a_window_that_starts_before_the_read_one_is_new_ground() {
    let previous = entry(45_000, 7, 0, 100, lines(120, 200, 100));
    let before = entry(45_000, 7, 0, 120, lines(60, 140, 120));
    let (verdict, kept) = advance(Some(&previous), before, lines(60, 140, 120));
    assert_eq!(verdict, Verdict::Record);
    assert_eq!(kept.spans, vec![lines(60, 200, 100)]);
    // And the merged interval now covers the whole span, not just its parts.
    let inside = entry(45_000, 7, 0, 140, lines(150, 190, 140));
    let (verdict, _) = advance(Some(&kept), inside, lines(150, 190, 140));
    assert!(matches!(verdict, Verdict::Deny { .. }));
}

#[test]
fn the_refusal_budget_hands_the_read_back() {
    let mut previous = entry(45_000, 7, 0, 100, whole(100));
    let mut now = 200;
    for _ in 0..MAX_REFUSALS {
        let asked = entry(45_000, 7, 0, now, lines(1, 40, now));
        let (verdict, kept) = advance(Some(&previous), asked, lines(1, 40, now));
        assert!(matches!(verdict, Verdict::Deny { .. }), "{verdict:?}");
        previous = kept;
        now += 60;
    }
    let asked = entry(45_000, 7, 0, now, lines(1, 40, now));
    let (verdict, _) = advance(Some(&previous), asked, lines(1, 40, now));
    assert_eq!(
        verdict,
        Verdict::Allow,
        "budget spent: compaction can restore"
    );
}

#[test]
fn spans_merge_adjacent_and_keep_the_earliest_read_time() {
    let mut spans = vec![
        lines(100, 140, 300),
        lines(1, 99, 100),
        lines(141, 200, 400),
    ];
    trim_spans(&mut spans);
    assert_eq!(spans, vec![lines(1, 200, 100)]);
}

#[test]
fn spans_are_capped_by_dropping_the_narrowest() {
    let mut spans: Vec<Span> = (0..u64::try_from(MAX_SPANS + 5).unwrap())
        .map(|index| lines(index * 100 + 1, index * 100 + 90, 1))
        .collect();
    spans.push(lines(1_000_000, 1_100_000, 1));
    trim_spans(&mut spans);
    assert_eq!(spans.len(), MAX_SPANS);
    // The widest interval survives, and the result is still sorted.
    assert!(spans.iter().any(|span| span.width() == 100_000));
    assert!(spans.windows(2).all(|pair| pair[0].start < pair[1].start));
}

#[test]
fn read_tool_spellings_are_guarded_and_shell_tools_are_not() {
    for tool in ["Read", "read", "read_file", "ReadFile"] {
        assert!(applies_to(tool), "{tool}");
    }
    for tool in ["Bash", "Edit", "Write", "Grep", "Glob", "apply_patch"] {
        assert!(!applies_to(tool), "{tool}");
    }
}

#[test]
fn kill_switch_accepts_zero_and_off_only() {
    for value in [Some("0"), Some("off"), Some("OFF"), Some(" off ")] {
        assert!(disabled_from(value), "{value:?}");
    }
    for value in [Some("1"), Some("on"), Some(""), None] {
        assert!(!disabled_from(value), "{value:?}");
    }
}

#[test]
fn a_request_needs_a_path() {
    let dir = tempdir().unwrap();
    assert!(request_for("Read", Some(&json!({"offset": 10}))).is_none());
    assert!(request_for("Read", None).is_none());
    let file = dir.path().join("a.py");
    std::fs::write(&file, "x").unwrap();
    let built =
        request_for("Read", Some(&json!({"file_path": file.to_str().unwrap()}))).expect("path");
    assert_eq!(built.path, file);
    assert_eq!(built.stamp.expect("file exists").len, 1);
}

#[test]
fn a_request_carries_the_line_interval_it_would_print() {
    assert_eq!(span_from(None), whole(0));
    assert_eq!(
        span_from(Some(&json!({"offset": 10}))),
        lines(10, u64::MAX, 0)
    );
    assert_eq!(span_from(Some(&json!({"limit": 50}))), lines(1, 50, 0));
    assert_eq!(
        span_from(Some(&json!({"offset": 100, "limit": 50}))),
        lines(100, 149, 0)
    );
    // A range read is allowed in every direction; only the interval matters.
    let whole_request = request_for("Read", Some(&json!({"file_path": "/tmp/a.py"})));
    let ranged = request_for(
        "Read",
        Some(&json!({"file_path": "/tmp/a.py", "offset": 100, "limit": 50})),
    );
    assert_eq!(
        whole_request.as_ref().map(|r| &r.key),
        ranged.as_ref().map(|r| &r.key),
        "one entry per file, not per offset/limit"
    );
}

#[test]
fn the_two_spellings_of_a_home_path_share_one_entry() {
    let Some(home) = std::env::var_os("HOME") else {
        return;
    };
    let spelled = request_for("Read", Some(&json!({"file_path": "~/dev/a.py"}))).expect("req");
    let absolute = request_for(
        "Read",
        Some(&json!({"file_path": format!("{}/dev/a.py", home.to_string_lossy())})),
    )
    .expect("req");
    assert_eq!(spelled.path, absolute.path);
    assert_eq!(spelled.key, absolute.key);
}

fn big_file(dir: &Path, body: &str) -> PathBuf {
    let path = dir.join("service.py");
    std::fs::write(&path, body).unwrap();
    path
}

#[test]
fn check_at_denies_only_the_unchanged_second_read() {
    let state = tempdir().unwrap();
    let work = tempdir().unwrap();
    let path = big_file(work.path(), &"x".repeat(9_000));
    let request =
        request_for("Read", Some(&json!({"file_path": path.to_str().unwrap()}))).expect("req");

    assert_eq!(
        check_at(state.path(), "sess", &request, 1_000),
        Verdict::Record
    );
    assert_eq!(
        check_at(state.path(), "sess", &request, 1_060),
        Verdict::Deny {
            ago_secs: 60,
            kb: 8,
            span: whole(0)
        }
    );
    // Another session has its own ledger.
    assert_eq!(
        check_at(state.path(), "other", &request, 1_060),
        Verdict::Record
    );
}

#[test]
fn check_at_denies_a_covered_window_from_a_previous_call() {
    let state = tempdir().unwrap();
    let work = tempdir().unwrap();
    let path = big_file(work.path(), &"x".repeat(20_000));
    let name = path.to_str().unwrap();
    let wide = request_for("Read", Some(&json!({"file_path": name}))).expect("req");
    assert_eq!(
        check_at(state.path(), "sess", &wide, 1_000),
        Verdict::Record
    );
    // Same file, an overlapping window: a different call, the same lines.
    let narrow = request_for(
        "Read",
        Some(&json!({"file_path": name, "offset": 20, "limit": 100})),
    )
    .expect("req");
    assert_eq!(
        check_at(state.path(), "sess", &narrow, 1_030),
        Verdict::Deny {
            ago_secs: 30,
            kb: 19,
            span: lines(20, 119, 0)
        }
    );
}

#[test]
fn check_at_allows_after_an_edit_and_for_small_files() {
    let state = tempdir().unwrap();
    let work = tempdir().unwrap();
    let path = big_file(work.path(), &"x".repeat(9_000));
    let request =
        request_for("Read", Some(&json!({"file_path": path.to_str().unwrap()}))).expect("req");
    assert_eq!(
        check_at(state.path(), "sess", &request, 1_000),
        Verdict::Record
    );

    // An edit moves both the length and the mtime.
    std::fs::write(&path, "y".repeat(9_100)).unwrap();
    let edited =
        request_for("Read", Some(&json!({"file_path": path.to_str().unwrap()}))).expect("req");
    assert_eq!(
        check_at(state.path(), "sess", &edited, 1_030),
        Verdict::Record
    );

    let small = work.path().join("small.py");
    std::fs::write(&small, "y".repeat(1_000)).unwrap();
    let small_request =
        request_for("Read", Some(&json!({"file_path": small.to_str().unwrap()}))).expect("req");
    assert_eq!(
        check_at(state.path(), "sess", &small_request, 1_040),
        Verdict::Allow
    );
    assert_eq!(
        check_at(state.path(), "sess", &small_request, 1_050),
        Verdict::Allow
    );
}

#[test]
fn a_missing_file_fails_open() {
    let state = tempdir().unwrap();
    let request = request_for(
        "Read",
        Some(&json!({"file_path": "/nonexistent/does-not-exist.py"})),
    )
    .expect("req");
    assert_eq!(
        check_at(state.path(), "sess", &request, 1_000),
        Verdict::Allow
    );
    assert_eq!(
        check_at(state.path(), "sess", &request, 1_001),
        Verdict::Allow
    );
}

#[test]
fn a_legacy_exact_key_ledger_still_parses() {
    let state = tempdir().unwrap();
    let work = tempdir().unwrap();
    let path = big_file(work.path(), &"x".repeat(9_000));
    let request =
        request_for("Read", Some(&json!({"file_path": path.to_str().unwrap()}))).expect("req");
    let legacy = json!({"entries": {"c81a312ce1d19198": {"len": 38344, "secs": 1, "nanos": 484783150, "at": 1, "name": "build.rs"}}});
    let dir = state.path().join(DIR_NAME);
    std::fs::create_dir_all(&dir).unwrap();
    let file = super::session_file(state.path(), "sess");
    std::fs::write(&file, serde_json::to_vec(&legacy).unwrap()).unwrap();
    assert_eq!(
        check_at(state.path(), "sess", &request, 1_000),
        Verdict::Record,
        "an entry without spans can only allow"
    );
}

#[test]
fn the_ledger_is_capped() {
    let mut ledger = Ledger::default();
    for index in 0..(MAX_ENTRIES + 10) {
        ledger.entries.insert(
            format!("key-{index}"),
            entry(9_000, 1, 0, index as u64, whole(1)),
        );
    }
    trim_entries(&mut ledger);
    assert_eq!(ledger.entries.len(), MAX_ENTRIES);
    assert!(!ledger.entries.contains_key("key-0"));
    assert!(ledger
        .entries
        .contains_key(&format!("key-{}", MAX_ENTRIES + 9)));
}

#[test]
fn the_refusal_names_the_remedies_and_stays_small() {
    let request = Request {
        key: "k".to_string(),
        path: PathBuf::from("/dev/zach/src/booking/service.py"),
        stamp: None,
        span: whole(1),
    };
    let message = deny_message(&request, lines(1, 420, 1), 90, 77);
    assert!(message.starts_with("[clud reread-guard] service.py (77 kB): lines 1-420"));
    assert!(
        message.contains("is already in this conversation"),
        "{message}"
    );
    assert!(message.contains("1min ago"));
    assert!(message.contains("offset"));
    assert!(message.contains("code/outline.py"));
    assert!(message.contains("CLUD_REREAD_GUARD=0"));
    // The refusal is the whole cost of the call: keep it under 700 bytes.
    assert!(message.len() < 700, "{} bytes", message.len());

    let whole_message = deny_message(&request, whole(1), 0, 77);
    assert!(whole_message.contains("the whole file"), "{whole_message}");
    assert!(whole_message.contains("just now"), "{whole_message}");
}

#[test]
fn span_text_is_readable() {
    assert_eq!(span_text(whole(0)), "the whole file");
    assert_eq!(span_text(lines(1, 420, 0)), "lines 1-420");
    assert_eq!(span_text(lines(120, u64::MAX, 0)), "lines 120-end");
}

#[test]
fn ago_text_is_compact() {
    assert_eq!(ago_text(5), "just now");
    assert_eq!(ago_text(30), "30s");
    assert_eq!(ago_text(90), "1min");
    assert_eq!(ago_text(7_200), "2h");
    assert_eq!(ago_text(172_800), "2d");
    assert_eq!(ago_phrase(3), "just now");
    assert_eq!(ago_phrase(90), "1min ago");
}

#[test]
fn the_hook_path_denies_through_the_state_dir_env() {
    let state = tempdir().unwrap();
    let work = tempdir().unwrap();
    let path = big_file(work.path(), &"x".repeat(9_000));
    let input = json!({"file_path": path.to_str().unwrap()});
    // SAFETY: single-threaded assertion window; the guard reads the env once
    // per call and this test owns both calls.
    unsafe { std::env::set_var("CLUD_DAEMON_STATE_DIR", state.path()) };
    let first = reason("Read", Some(&input), Some("sess-hook"));
    assert_eq!(first, None, "first read must pass");
    let second = reason("Read", Some(&input), Some("sess-hook"));
    unsafe { std::env::remove_var("CLUD_DAEMON_STATE_DIR") };
    let second = second.expect("unchanged second read must be refused");
    assert!(second.contains("reread-guard"), "{second}");
}

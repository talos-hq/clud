//! Decision table and command parsing for the multi-range read-hunt guard
//! (#2233).

use super::*;
use tempfile::tempdir;

const LIMIT: u32 = DEFAULT_LIMIT;

fn fixture(dir: &Path, name: &str, bytes: usize) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, "x".repeat(bytes)).expect("fixture");
    path
}

fn hunt_for(paths: &[(&str, u32, u32)]) -> Hunt {
    Hunt {
        ranges: paths
            .iter()
            .map(|(path, start, end)| Range {
                path: (*path).to_string(),
                start: *start,
                end: *end,
            })
            .collect(),
    }
}

/// Feed one hunt shape through the table `n` times, `gap` seconds apart, and
/// return the 1-based indices that were refused.
fn refused(n: u32, gap: u64, limit: u32) -> Vec<u32> {
    let hunt = hunt_for(&[("/w/a.rs", 40, 100), ("/w/b.rs", 120, 200)]);
    let mut state: Option<State> = None;
    let mut out = Vec::new();
    for index in 0..n {
        let now = 1_000 + gap * u64::from(index);
        let (verdict, state_now) = decide(state.as_ref(), &hunt, now, limit);
        if verdict != Verdict::Allow {
            out.push(index + 1);
        }
        state = Some(state_now);
    }
    out
}

#[test]
fn hunts_up_to_the_limit_are_allowed_and_the_next_distinct_hunt_is_refused() {
    assert!(refused(LIMIT, 0, LIMIT).is_empty());
    assert_eq!(refused(LIMIT + 1, 0, LIMIT), vec![LIMIT + 1]);
}

#[test]
fn the_same_hunt_twice_is_refused_the_first_time_and_allowed_on_retry() {
    let hunt = hunt_for(&[("/w/a.rs", 40, 100), ("/w/a.rs", 400, 460)]);
    let mut state: Option<State> = None;
    let mut verdicts = Vec::new();
    for index in 0..(LIMIT + 3) {
        let (verdict, next) = decide(state.as_ref(), &hunt, 1_000 + u64::from(index), LIMIT);
        verdicts.push(verdict != Verdict::Allow);
        state = Some(next);
    }
    assert_eq!(verdicts, vec![false, false, false, true, false, false]);
}

#[test]
fn a_later_hunt_after_the_reset_window_starts_a_new_count() {
    let hunt = hunt_for(&[("/w/a.rs", 40, 100), ("/w/b.rs", 120, 200)]);
    let limit = 1;
    let (_, state) = decide(None, &hunt, 1_000, limit);
    assert_eq!(state.hunts, 1);
    let (verdict, state) = decide(Some(&state), &hunt, 1_000 + RESET_SECS + 1, limit);
    assert_eq!(verdict, Verdict::Allow);
    assert_eq!(state.hunts, 1);
}

#[test]
fn a_zero_limit_disables_the_guard_but_still_counts_nothing_into_a_refusal() {
    assert!(refused(10, 0, 0).is_empty());
}

#[test]
fn hunt_detection_requires_two_existing_large_source_ranges() {
    let dir = tempdir().expect("tempdir");
    let a = fixture(dir.path(), "a.rs", 20_000);
    let b = fixture(dir.path(), "b.rs", 20_000);
    let a = a.to_string_lossy();
    let b = b.to_string_lossy();
    let command = format!("sed -n '40,100p' {a}; sed -n 120,200p {b}");
    let hunt = detect(&command, dir.path()).expect("a two-range hunt");
    assert_eq!(hunt.ranges.len(), 2);
    assert_eq!(hunt.lines(), 142);

    // One range is a targeted read, not a hunt.
    assert!(detect(&format!("sed -n '40,100p' {a}"), dir.path()).is_none());
    // Unknown files fail open.
    assert!(detect(
        &format!("sed -n '1,40p' {a}/nope.rs; sed -n 1,40p {b}"),
        dir.path()
    )
    .is_none());
    // Small files are never counted.
    let small = fixture(dir.path(), "small.rs", 1_000);
    let small = small.to_string_lossy();
    let short = format!("sed -n '1,40p' {small}; sed -n '1,40p' {a}");
    assert!(detect(&short, dir.path()).is_none());
}

#[test]
fn a_short_confirmation_read_is_not_a_hunt() {
    let dir = tempdir().expect("tempdir");
    let a = fixture(dir.path(), "a.rs", 20_000);
    let a = a.to_string_lossy();
    let command = format!("sed -n '1,10p' {a}; sed -n '20,30p' {a}");
    assert!(hunt_ranges(&command).len() == 2);
    assert!(
        detect(&command, dir.path()).is_none(),
        "22 lines is not a hunt"
    );
}

#[test]
fn the_measured_hunt_shape_is_parsed_into_all_of_its_ranges() {
    let dir = tempdir().expect("tempdir");
    let a = fixture(dir.path(), "signal.rs", 20_000);
    let b = fixture(dir.path(), "interleave.rs", 20_000);
    let a = a.to_string_lossy();
    let b = b.to_string_lossy();
    let command = format!("cd /w && sed -n 140,413p {a}; sed -n 300,345p {a}; sed -n 1,80p {b}");
    let hunt = detect(&command, dir.path()).expect("three ranges");
    assert_eq!(
        hunt.ranges,
        vec![
            Range {
                path: a.to_string(),
                start: 140,
                end: 413
            },
            Range {
                path: a.to_string(),
                start: 300,
                end: 345
            },
            Range {
                path: b.to_string(),
                start: 1,
                end: 80
            },
        ]
    );
}

#[test]
fn piped_and_redirected_segments_are_not_ranges() {
    let dir = tempdir().expect("tempdir");
    let a = fixture(dir.path(), "a.rs", 20_000);
    let b = fixture(dir.path(), "b.rs", 20_000);
    let a = a.to_string_lossy();
    let b = b.to_string_lossy();
    // A pipe ends the read: `sed -n 1,40p f | head` is already bounded.
    assert!(hunt_ranges(&format!("sed -n '1,40p' {a} | head -5")).is_empty());
    // Redirects are writes.
    assert!(hunt_ranges(&format!("sed -n '1,40p' {a} > out.txt; cat {b}")).is_empty());
    // `sed -i` never lands in the transcript.
    assert!(hunt_ranges(&format!("sed -ni '1,40p' {a}")).is_empty());
}

#[test]
fn quoted_and_unquoted_ranges_both_parse() {
    assert_eq!(parse_range("40,60p"), Some((40, 60)));
    assert_eq!(parse_range("'40,60p'"), Some((40, 60)));
    assert_eq!(parse_range("\"40,60p\""), Some((40, 60)));
    assert_eq!(parse_range("40,60"), None);
    assert_eq!(parse_range("0,60p"), None);
    assert_eq!(parse_range("60,40p"), None);
    assert_eq!(parse_range("a,bp"), None);
}

#[test]
fn non_source_targets_are_not_hunts() {
    let dir = tempdir().expect("tempdir");
    let log = fixture(dir.path(), "build.log", 40_000);
    let log = log.to_string_lossy();
    assert!(hunt_ranges(&format!("sed -n '1,40p' {log}; sed -n '50,90p' {log}")).is_empty());
}

#[test]
fn the_advice_names_every_range_in_one_slice_call() {
    let hunt = hunt_for(&[
        ("/w/a.rs", 40, 100),
        ("/w/a.rs", 400, 460),
        ("/w/b.rs", 1, 40),
    ]);
    let command = slice_command(&hunt);
    // One command per file: the tool reads a single file per call.
    assert_eq!(
        command,
        "clud tool run code/slice.py /w/a.rs --lines 40-100 --lines 400-460 ; \
         clud tool run code/slice.py /w/b.rs --lines 1-40"
    );
    assert!(!command.contains("sed"));
}

#[test]
fn a_long_hunt_is_advised_with_the_first_ranges_and_says_so() {
    let ranges: Vec<(&str, u32, u32)> = (0..9)
        .map(|i| ("/w/a.rs", i * 50 + 1, i * 50 + 40))
        .collect();
    let hunt = hunt_for(&ranges);
    let command = slice_command(&hunt);
    assert_eq!(command.matches(" --lines ").count(), MAX_ADVISED_RANGES);
    assert!(command.contains("(+3 more --lines)"));
}

#[test]
fn the_denial_names_the_count_the_limit_and_the_override() {
    let hunt = hunt_for(&[("/w/a.rs", 40, 100), ("/w/b.rs", 120, 200)]);
    let message = deny_message(&hunt, 5, LIMIT);
    assert!(message.contains("read-hunt-guard"));
    assert!(message.contains("5 multi-range shell reads"));
    assert!(message.contains("limit 3"));
    assert!(message.contains("code/slice.py"));
    assert!(message.contains(ALLOW_TOKEN));
}

#[test]
fn the_override_token_must_stand_as_a_whole_word() {
    assert!(opts_out("CLUD_ALLOW_READ_HUNT=1 sed -n '1,40p' a.rs"));
    assert!(opts_out("X=1 CLUD_ALLOW_READ_HUNT=1 sed -n '1,40p' a.rs"));
    assert!(!opts_out("CLUD_ALLOW_READ_HUNT=10 sed -n '1,40p' a.rs"));
    assert!(!opts_out("MYCLUD_ALLOW_READ_HUNT=1 sed -n '1,40p' a.rs"));
}

#[test]
fn the_limit_defaults_and_can_be_disabled() {
    assert_eq!(limit_from(None), DEFAULT_LIMIT);
    assert_eq!(limit_from(Some("7")), 7);
    assert_eq!(limit_from(Some("0")), 0);
    assert_eq!(limit_from(Some("soon")), DEFAULT_LIMIT);
}

#[test]
fn only_shell_tools_are_guarded() {
    for tool in ["Bash", "bash", "Shell", "shell"] {
        assert!(shell_tool(tool), "{tool}");
    }
    for tool in ["Read", "Grep", "Edit", "Write", "apply_patch"] {
        assert!(!shell_tool(tool), "{tool}");
    }
}

#[test]
fn state_is_persisted_per_session_and_a_second_session_starts_clean() {
    let dir = tempdir().expect("tempdir");
    let hunt = hunt_for(&[("/w/a.rs", 40, 100), ("/w/b.rs", 120, 200)]);
    let mut verdicts = Vec::new();
    for index in 0..(LIMIT + 1) {
        let verdict = check_at(
            dir.path(),
            "session-a",
            &hunt,
            1_000 + u64::from(index),
            LIMIT,
        );
        verdicts.push(verdict != Verdict::Allow);
    }
    assert_eq!(verdicts.last(), Some(&true));
    assert_eq!(
        check_at(dir.path(), "session-b", &hunt, 1_100, LIMIT),
        Verdict::Allow
    );
}

#[test]
fn an_unwritable_state_dir_allows_the_call() {
    let dir = tempdir().expect("tempdir");
    let missing = dir.path().join("missing");
    let hunt = hunt_for(&[("/w/a.rs", 40, 100), ("/w/b.rs", 120, 200)]);
    // The parent exists as a file, so `create_dir_all` cannot succeed.
    std::fs::write(&missing, b"not a dir").expect("file");
    assert_eq!(check_at(&missing, "s", &hunt, 1_000, 1), Verdict::Allow);
}

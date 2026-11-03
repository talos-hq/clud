//! Decision table, size prediction and command parsing for the oversized
//! single-range read guard (#2233).

use super::*;
use tempfile::tempdir;

const LIMIT: u32 = DEFAULT_LIMIT;

/// A source file whose every line is `width` bytes, `lines` long.
fn source(dir: &Path, name: &str, lines: usize, width: usize) -> PathBuf {
    let path = dir.join(name);
    let text: String = (0..lines)
        .map(|index| format!("{:0width$}\n", index, width = width))
        .collect();
    std::fs::write(&path, text).expect("fixture");
    path
}

fn slice_for(path: &str, start: u32, end: u32) -> Slice {
    Slice {
        path: path.to_string(),
        start,
        end,
        bytes: 0,
    }
}

/// Feed one read shape through the table `n` times, `gap` seconds apart, and
/// return the 1-based indices that were refused.
fn refused(n: u32, gap: u64, limit: u32) -> Vec<u32> {
    let slice = slice_for("/w/a.rs", 1, 400);
    let mut state: Option<State> = None;
    let mut out = Vec::new();
    for index in 0..n {
        let now = 1_000 + gap * u64::from(index);
        let (verdict, state_now) = decide(state.as_ref(), &slice, now, limit);
        if verdict != Verdict::Allow {
            out.push(index + 1);
        }
        state = Some(state_now);
    }
    out
}

#[test]
fn reads_up_to_the_limit_are_allowed_and_the_next_distinct_read_is_refused() {
    assert!(refused(LIMIT, 0, LIMIT).is_empty());
    assert_eq!(refused(LIMIT + 1, 0, LIMIT), vec![LIMIT + 1]);
}

#[test]
fn the_same_read_twice_is_refused_once_and_allowed_on_retry() {
    let slice = slice_for("/w/a.rs", 120, 720);
    let mut state: Option<State> = None;
    let mut verdicts = Vec::new();
    for index in 0..(LIMIT + 3) {
        let (verdict, next) = decide(state.as_ref(), &slice, 1_000 + u64::from(index), LIMIT);
        verdicts.push(verdict != Verdict::Allow);
        state = Some(next);
    }
    assert_eq!(
        verdicts,
        vec![false, false, true, false, false],
        "the third identical read is the only refusal; retries always pass"
    );
}

#[test]
fn a_later_read_after_the_reset_window_starts_a_new_count() {
    let slice = slice_for("/w/a.rs", 1, 400);
    let limit = 1;
    let (_, state) = decide(None, &slice, 1_000, limit);
    assert_eq!(state.slices, 1);
    let (verdict, state) = decide(Some(&state), &slice, 1_000 + RESET_SECS + 1, limit);
    assert_eq!(verdict, Verdict::Allow);
    assert_eq!(state.slices, 1, "the count restarted");
}

#[test]
fn a_limit_of_zero_disables_the_guard() {
    assert!(refused(12, 0, 0).is_empty());
}

#[test]
fn limit_from_parses_and_falls_back() {
    assert_eq!(limit_from(None), DEFAULT_LIMIT);
    assert_eq!(limit_from(Some(" 5 ")), 5);
    assert_eq!(limit_from(Some("0")), 0);
    assert_eq!(limit_from(Some("nonsense")), DEFAULT_LIMIT);
}

#[test]
fn detect_counts_a_single_wide_range_above_the_cap() {
    let dir = tempdir().expect("dir");
    source(dir.path(), "big.rs", 400, 60);
    let slice = detect("sed -n '200,400p' big.rs", dir.path()).expect("detected");
    assert_eq!((slice.start, slice.end), (200, 400));
    assert!(slice.bytes >= CAP_BYTES, "predicted {} B", slice.bytes);
    assert!(slice.path.ends_with("big.rs"));
}

#[test]
fn detect_ignores_a_range_whose_printed_text_fits_the_cap() {
    let dir = tempdir().expect("dir");
    source(dir.path(), "small_window.rs", 400, 20);
    assert!(
        detect("sed -n '1,200p' small_window.rs", dir.path()).is_none(),
        "a read under the cap is cheaper with sed; the slice reader would be larger"
    );
}

#[test]
fn detect_ignores_multi_range_calls_that_belong_to_the_hunt_guard() {
    let dir = tempdir().expect("dir");
    source(dir.path(), "two.rs", 800, 60);
    assert!(detect(
        "sed -n '1,400p' two.rs; sed -n '500,800p' two.rs",
        dir.path()
    )
    .is_none());
}

#[test]
fn detect_ignores_a_piped_read_whose_output_is_bounded() {
    let dir = tempdir().expect("dir");
    source(dir.path(), "piped.rs", 800, 60);
    assert!(detect("sed -n '1,700p' piped.rs | head -20", dir.path()).is_none());
}

#[test]
fn detect_ignores_redirects_and_in_place_edits() {
    let dir = tempdir().expect("dir");
    source(dir.path(), "w.rs", 800, 60);
    assert!(detect("sed -n '1,700p' w.rs > out.txt", dir.path()).is_none());
    assert!(detect("sed -i '1,700p' w.rs", dir.path()).is_none());
}

#[test]
fn detect_ignores_files_below_the_size_floor() {
    let dir = tempdir().expect("dir");
    source(dir.path(), "tiny.rs", 40, 10);
    assert!(detect("sed -n '1,40p' tiny.rs", dir.path()).is_none());
}

#[test]
fn detect_ignores_non_source_targets_like_logs_and_data() {
    let dir = tempdir().expect("dir");
    source(dir.path(), "build.log", 800, 60);
    assert!(detect("sed -n '1,700p' build.log", dir.path()).is_none());
}

#[test]
fn detect_ignores_a_missing_file_and_a_range_past_the_end() {
    let dir = tempdir().expect("dir");
    source(dir.path(), "short.rs", 100, 60);
    assert!(detect("sed -n '1,700p' missing.rs", dir.path()).is_none());
    assert!(
        detect("sed -n '400,700p' short.rs", dir.path()).is_none(),
        "nothing to print past the end of the file"
    );
}

#[test]
fn detect_resolves_relative_targets_against_the_declared_cwd() {
    let dir = tempdir().expect("dir");
    std::fs::create_dir_all(dir.path().join("crates")).expect("nested");
    source(&dir.path().join("crates"), "deep.rs", 400, 60);
    let slice = detect("sed -n '1,400p' crates/deep.rs", dir.path()).expect("detected");
    assert!(slice.path.ends_with("crates/deep.rs"));
}

#[test]
fn printed_bytes_counts_every_line_of_the_range_plus_its_newline() {
    let dir = tempdir().expect("dir");
    let path = source(dir.path(), "count.rs", 100, 40);
    // 40-byte lines, range 10..19 == ten lines.
    assert_eq!(printed_bytes(&path, 10, 19), Some(10 * 41));
    assert_eq!(printed_bytes(&path, 100, 140), Some(41));
    assert_eq!(printed_bytes(&path, 101, 140), None);
}

#[test]
fn signature_ignores_the_predicted_size_and_distinguishes_ranges() {
    let mut first = slice_for("/w/a.rs", 1, 400);
    let second = slice_for("/w/a.rs", 1, 400);
    let other = slice_for("/w/a.rs", 2, 400);
    first.bytes = 10;
    assert_eq!(first.signature(), second.signature());
    assert_ne!(first.signature(), other.signature());
}

#[test]
fn opts_out_matches_the_token_as_a_whole_word_only() {
    assert!(opts_out("CLUD_ALLOW_SLICE_BUDGET=1 sed -n '1,700p' a.rs"));
    assert!(!opts_out("echo CLUD_ALLOW_SLICE_BUDGET=1x"));
    assert!(!opts_out("sed -n '1,700p' a.rs"));
}

#[test]
fn non_shell_tools_and_missing_session_allow() {
    let dir = tempdir().expect("dir");
    let input = Some(serde_json::json!({"command": "sed -n '1,700p' a.rs"}));
    assert!(reason("Read", input.as_ref(), Some("s1"), dir.path()).is_none());
    assert!(reason("Bash", input.as_ref(), None, dir.path()).is_none());
}

#[test]
fn the_refusal_names_the_exact_bounded_command() {
    let dir = tempdir().expect("dir");
    let path = source(dir.path(), "msg.rs", 900, 60);
    let slice = detect("sed -n '300,900p' msg.rs", dir.path()).expect("detected");
    let message = deny_message(&slice, 3, LIMIT);
    assert!(message.contains("read-slice-guard"));
    assert!(message.contains(path.to_string_lossy().as_ref()));
    assert!(message.contains("--lines 300-900"));
    assert!(message.contains(ALLOW_TOKEN));
    assert!(message.contains("already made 3 oversized ranged reads"));
    assert!(message.contains(&format!("{} B", slice.bytes)));
}

#[test]
fn corrupt_state_and_state_errors_fail_open() {
    let dir = tempdir().expect("dir");
    let slice = slice_for("/w/a.rs", 1, 400);
    std::fs::create_dir_all(dir.path().join(DIR_NAME)).expect("dir");
    let path = session_file(dir.path(), "s1");
    std::fs::write(&path, b"{not json").expect("write");
    assert_eq!(check_at(dir.path(), "s1", &slice, 1_000, 1), Verdict::Allow);
}

#[test]
fn check_at_counts_across_calls_and_refuses_the_third_distinct_read() {
    let dir = tempdir().expect("dir");
    let path = source(dir.path(), "walk.rs", 900, 60);
    let first = detect("sed -n '1,400p' walk.rs", dir.path()).expect("first");
    let second = detect("sed -n '1,300p' walk.rs", dir.path()).expect("second");
    let third = detect("sed -n '1,200p' walk.rs", dir.path()).expect("third");
    assert_eq!(
        check_at(dir.path(), "s1", &first, 1_000, LIMIT),
        Verdict::Allow
    );
    assert_eq!(
        check_at(dir.path(), "s1", &second, 1_001, LIMIT),
        Verdict::Allow
    );
    assert!(matches!(
        check_at(dir.path(), "s1", &third, 1_002, LIMIT),
        Verdict::Deny { .. }
    ));
    // The retry of the refused read passes, and the earlier reads stay allowed.
    assert_eq!(
        check_at(dir.path(), "s1", &third, 1_003, LIMIT),
        Verdict::Allow
    );
    assert!(second.path.ends_with("walk.rs") && path.ends_with("walk.rs"));
}

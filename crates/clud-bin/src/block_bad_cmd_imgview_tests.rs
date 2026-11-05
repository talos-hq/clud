//! Decision table, path rules and persistence for the image-view budget
//! (#2233).

use super::*;

const LIMIT: u32 = DEFAULT_LIMIT;
const PATH: &str = "/w/cur.png";

/// Feed one path through the table `n` times, `gap` seconds apart, and return
/// the verdicts in order.
fn walk(n: u32, gap: u64, limit: u32) -> Vec<Verdict> {
    let mut state: Option<State> = None;
    let mut out = Vec::new();
    for index in 0..n {
        let now = 1_000 + gap * u64::from(index);
        let (verdict, next) = decide(state.as_ref(), PATH, now, limit);
        out.push(verdict);
        state = Some(next);
    }
    out
}

fn refused(history: &[Verdict]) -> Vec<usize> {
    history
        .iter()
        .enumerate()
        .filter(|(_, verdict)| matches!(verdict, Verdict::DenyOnce { .. }))
        .map(|(index, _)| index + 1)
        .collect()
}

fn nudged(history: &[Verdict]) -> Vec<usize> {
    history
        .iter()
        .enumerate()
        .filter(|(_, verdict)| matches!(verdict, Verdict::Nudge { .. }))
        .map(|(index, _)| index + 1)
        .collect()
}

#[test]
fn early_reads_are_silent_and_the_note_starts_past_half_the_limit() {
    let history = walk(LIMIT, 0, LIMIT);
    let first = usize::try_from(LIMIT).unwrap() / 2 + 1;
    let expected: Vec<usize> = (first..=usize::try_from(LIMIT).unwrap()).collect();
    assert_eq!(nudged(&history), expected);
    assert!(refused(&history).is_empty());
}

#[test]
fn the_limit_is_refused_once_and_the_retry_runs() {
    // The measured session opened one scratchpad path 46 times; the 13th view
    // is where the agent should stop and think about what it is capturing.
    let history = walk(LIMIT + 5, 0, LIMIT);
    assert_eq!(refused(&history), vec![usize::try_from(LIMIT).unwrap() + 1]);
    assert_eq!(
        history[usize::try_from(LIMIT).unwrap()],
        Verdict::DenyOnce { count: LIMIT + 1 }
    );
    assert_eq!(history[usize::try_from(LIMIT).unwrap() + 1], Verdict::Allow);
}

#[test]
fn a_zero_limit_disables_the_guard() {
    let history = walk(LIMIT * 2, 0, 0);
    assert!(history.iter().all(|verdict| *verdict == Verdict::Allow));
}

#[test]
fn an_idle_gap_longer_than_the_window_starts_a_new_count() {
    let history = walk(LIMIT + 1, 0, LIMIT);
    assert_eq!(refused(&history).len(), 1);
    // A fresh look an hour later is a new task, not a poll.
    let mut state: Option<State> = None;
    for index in 0..LIMIT {
        let (verdict, next) = decide(state.as_ref(), PATH, 1_000u64 + u64::from(index), LIMIT);
        state = Some(next);
        assert!(!matches!(verdict, Verdict::DenyOnce { .. }));
    }
    let late = 1_000u64 + u64::from(LIMIT) + 1 + RESET_SECS + 1;
    let (verdict, _) = decide(state.as_ref(), PATH, late, LIMIT);
    assert_eq!(verdict, Verdict::Allow);
}

#[test]
fn a_gap_inside_the_window_keeps_counting() {
    let mut state: Option<State> = None;
    for index in 0..LIMIT + 1 {
        let now = 1_000 + u64::from(index) * RESET_SECS;
        let (verdict, next) = decide(state.as_ref(), PATH, now, LIMIT);
        state = Some(next);
        if index < LIMIT {
            assert!(!matches!(verdict, Verdict::DenyOnce { .. }));
        } else {
            assert_eq!(verdict, Verdict::DenyOnce { count: LIMIT + 1 });
        }
    }
}

#[test]
fn two_image_paths_get_independent_counts() {
    let mut state: Option<State> = None;
    for _ in 0..LIMIT {
        let (_, next) = decide(state.as_ref(), "/w/a.png", 1_000, LIMIT);
        state = Some(next);
    }
    let (verdict, _) = decide(state.as_ref(), "/w/b.png", 1_000, LIMIT);
    assert_eq!(verdict, Verdict::Allow);
}

#[test]
fn the_state_map_is_bounded() {
    let mut state: Option<State> = None;
    for index in 0..MAX_TRACKED * 3 {
        let path = format!("/w/{index}.png");
        let (_, next) = decide(state.as_ref(), &path, 1_000 + index as u64, LIMIT);
        state = Some(next);
    }
    assert!(state.unwrap_or_default().views.len() <= MAX_TRACKED);
}

#[test]
fn the_refusal_is_remembered_across_a_serialized_round_trip() {
    let (verdict, state) = decide(None, PATH, 1_000, LIMIT);
    assert_eq!(verdict, Verdict::Allow);
    let mut state = Some(state);
    for _ in 1..LIMIT {
        let (verdict, next) = decide(state.as_ref(), PATH, 1_000, LIMIT);
        assert!(!matches!(verdict, Verdict::DenyOnce { .. }));
        state = Some(next);
    }
    let bytes = serde_json::to_vec(&state.unwrap_or_default()).unwrap_or_default();
    let reloaded: State = serde_json::from_slice(&bytes).unwrap_or_default();
    let (verdict, _) = decide(Some(&reloaded), PATH, 1_000, LIMIT);
    assert_eq!(verdict, Verdict::DenyOnce { count: LIMIT + 1 });
}

#[test]
fn the_limit_env_is_read_defensively() {
    assert_eq!(limit_from(None), DEFAULT_LIMIT);
    assert_eq!(limit_from(Some("0")), 0);
    assert_eq!(limit_from(Some(" 4 ")), 4);
    assert_eq!(limit_from(Some("many")), DEFAULT_LIMIT);
    assert_eq!(limit_from(Some("-1")), DEFAULT_LIMIT);
}

#[test]
fn only_read_class_tools_are_inspected() {
    assert!(applies_to("Read"));
    assert!(applies_to("read_file"));
    assert!(applies_to("ReadFile"));
    assert!(!applies_to("Bash"));
    assert!(!applies_to("Edit"));
}

#[test]
fn only_image_extensions_are_counted() {
    assert!(is_image(Path::new("/w/cur.png")));
    assert!(is_image(Path::new("/w/shot.PNG")));
    assert!(is_image(Path::new("/w/shot.jpeg")));
    assert!(is_image(Path::new("/w/shot.webp")));
    assert!(!is_image(Path::new("/w/main.rs")));
    assert!(!is_image(Path::new("/w/notes.md")));
    assert!(!is_image(Path::new("/w/png")));
    assert!(!is_image(Path::new("/w/cur.png.bak")));
}

#[test]
fn equivalent_spellings_resolve_to_one_path() {
    let cwd = Path::new("/w/sub");
    let home = Path::new("/home/u");
    assert_eq!(
        resolve(Path::new("cur.png"), cwd, Some(home)),
        Path::new("/w/sub/cur.png")
    );
    assert_eq!(
        resolve(Path::new("./cur.png"), cwd, Some(home)),
        Path::new("/w/sub/cur.png")
    );
    assert_eq!(
        resolve(Path::new("/x/cur.png"), cwd, Some(home)),
        Path::new("/x/cur.png")
    );
    assert_eq!(
        resolve(Path::new("~/cur.png"), cwd, Some(home)),
        Path::new("/home/u/cur.png")
    );
}

fn temp_state_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("clud-imgview-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[test]
fn a_session_ledger_persists_across_calls() {
    let dir = temp_state_dir("persist");
    let mut verdicts = Vec::new();
    for _ in 0..LIMIT + 2 {
        verdicts.push(record(&dir, "sess-1", PATH, 1_000, LIMIT));
    }
    assert_eq!(
        refused(&verdicts),
        vec![usize::try_from(LIMIT).unwrap() + 1]
    );
    // A different session starts clean.
    assert_eq!(record(&dir, "sess-2", PATH, 1_000, LIMIT), Verdict::Allow);
    assert!(dir.join(DIR_NAME).is_dir());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_zero_limit_writes_no_state() {
    let dir = temp_state_dir("disabled");
    assert_eq!(record(&dir, "sess-1", PATH, 1_000, 0), Verdict::Allow);
    assert!(!dir.exists());
}

#[test]
fn the_notes_name_the_file_the_cost_and_the_way_out() {
    let deny = deny_message("cur.png", LIMIT + 1, LIMIT, 190);
    assert!(deny.contains("cur.png"));
    assert!(deny.contains("13 times"));
    assert!(deny.contains("once per file per session"));
    assert!(deny.contains(LIMIT_ENV));
    assert!(deny.contains("2280 KB"), "{deny}");

    let nudge = nudge_message("cur.png", LIMIT - 1, LIMIT, 190);
    assert!(nudge.contains("cur.png"));
    assert!(nudge.contains("190 KB"));
    assert!(
        nudge.contains("1 more views before clud asks once"),
        "{nudge}"
    );

    let last = nudge_message("cur.png", LIMIT, LIMIT, 190);
    assert!(
        last.contains("The next view of this file asks once"),
        "{last}"
    );
}

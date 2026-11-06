//! Decision table, path rules and persistence for the image-view budget
//! (#2233).

use super::*;

const LIMIT: u32 = DEFAULT_LIMIT;
const PATH: &str = "/w/cur.png";
/// A frame of 180 KB, the size the measured audit session captured.
const FRAME: u64 = 180 * 1024;

fn facts<'a>(path: &'a str, hash: Option<&'a str>) -> Facts<'a> {
    Facts {
        path,
        hash,
        size: FRAME,
    }
}

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
        verdicts.push(record(
            &dir,
            "sess-1",
            &facts(PATH, None),
            1_000,
            LIMIT,
            SESSION_LIMIT,
        ));
    }
    assert_eq!(
        refused(&verdicts),
        vec![usize::try_from(LIMIT).unwrap() + 1]
    );
    // A different session starts clean.
    assert_eq!(
        record(
            &dir,
            "sess-2",
            &facts(PATH, None),
            1_000,
            LIMIT,
            SESSION_LIMIT
        ),
        Verdict::Allow
    );
    assert!(dir.join(DIR_NAME).is_dir());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_zero_limit_writes_no_state() {
    let dir = temp_state_dir("disabled");
    assert_eq!(
        record(&dir, "sess-1", &facts(PATH, None), 1_000, 0, SESSION_LIMIT),
        Verdict::Allow
    );
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

/// Feed `count` distinct frames, one path each, one second apart, and return
/// the verdicts in order.
fn walk_session(count: u32) -> Vec<Verdict> {
    let mut state: Option<State> = None;
    let mut out = Vec::new();
    for index in 0..count {
        let path = format!("/w/frame{index}.png");
        let hash = format!("h{index}");
        let (verdict, next) = decide_frame(
            state.as_ref(),
            &facts(&path, Some(&hash)),
            1_000 + u64::from(index),
            LIMIT,
            SESSION_LIMIT,
        );
        out.push(verdict);
        state = Some(next);
    }
    out
}

fn session_nudged(history: &[Verdict]) -> usize {
    history
        .iter()
        .filter(|verdict| matches!(verdict, Verdict::SessionNudge { .. }))
        .count()
}

fn session_refused(history: &[Verdict]) -> Vec<usize> {
    history
        .iter()
        .enumerate()
        .filter(|(_, verdict)| matches!(verdict, Verdict::SessionDeny { .. }))
        .map(|(index, _)| index + 1)
        .collect()
}

#[test]
fn many_paths_get_a_session_budget_the_per_path_count_cannot_see() {
    // The measured audit session: 31 frames over 29 paths, 2 views per path, so
    // the per-path budget stayed silent for all 31.
    let history = walk_session(SESSION_LIMIT + 7);
    assert_eq!(
        session_refused(&history),
        vec![usize::try_from(SESSION_LIMIT).unwrap() + 1]
    );
    // Every later frame is nudged: the note keeps riding after the refusal.
    let late = usize::try_from(SESSION_LIMIT + 7).unwrap();
    let quiet = usize::try_from(SESSION_NUDGE).unwrap();
    assert_eq!(session_nudged(&history), late - quiet - 1);
    // The retry runs and the session refusal is spent.
    assert!(!matches!(
        history[usize::try_from(SESSION_LIMIT).unwrap() + 1],
        Verdict::SessionDeny { .. }
    ));
}

#[test]
fn a_session_denial_names_the_frames_and_the_payload() {
    let history = walk_session(SESSION_LIMIT + 1);
    match &history[usize::try_from(SESSION_LIMIT).unwrap()] {
        Verdict::SessionDeny { frames, kb } => {
            assert_eq!(*frames, SESSION_LIMIT + 1);
            // 25 frames of 180 KB, counted in whole KB.
            assert_eq!(*kb, u64::from(SESSION_LIMIT + 1) * 180);
        }
        other => panic!("expected a session denial, got {other:?}"),
    }
}

#[test]
fn the_same_bytes_twice_are_one_frame() {
    // A re-read or a copy is not a new frame: identity must not inflate the
    // session budget (block_bad_cmd_reread refuses the read before it lands).
    let mut state: Option<State> = None;
    let mut verdicts = Vec::new();
    for index in 0..SESSION_LIMIT * 3 {
        let (verdict, next) = decide_frame(
            state.as_ref(),
            &facts("/w/one.png", Some("same")),
            1_000 + u64::from(index),
            LIMIT,
            0,
        );
        verdicts.push(verdict);
        state = Some(next);
    }
    assert_eq!(state.as_ref().unwrap().frames.len(), 1);
    assert!(verdicts.iter().all(|verdict| !matches!(
        verdict,
        Verdict::SessionDeny { .. } | Verdict::SessionNudge { .. }
    )));
}

#[test]
fn a_zero_session_limit_keeps_only_the_per_path_budget() {
    let verdicts = walk_session_with(SESSION_LIMIT * 2, 0, Some("h"));
    assert!(verdicts.iter().all(|verdict| !matches!(
        verdict,
        Verdict::SessionDeny { .. } | Verdict::SessionNudge { .. }
    )));
}

fn walk_session_with(count: u32, session_limit: u32, _tag: Option<&str>) -> Vec<Verdict> {
    let mut state: Option<State> = None;
    let mut out = Vec::new();
    for index in 0..count {
        let path = format!("/w/p{index}.png");
        let hash = format!("k{index}");
        let (verdict, next) = decide_frame(
            state.as_ref(),
            &facts(&path, Some(&hash)),
            1_000 + u64::from(index),
            LIMIT,
            session_limit,
        );
        out.push(verdict);
        state = Some(next);
    }
    out
}

#[test]
fn a_frame_without_a_hash_fails_open() {
    // No hash (unreadable, oversized, or hashing skipped) must not disable the
    // session budget: the frame was still returned and is still retained.
    let mut state: Option<State> = None;
    let mut denied = 0;
    for index in 0..SESSION_LIMIT + 2 {
        let path = format!("/w/n{index}.png");
        let (verdict, next) = decide_frame(
            state.as_ref(),
            &facts(&path, None),
            1_000 + u64::from(index),
            LIMIT,
            SESSION_LIMIT,
        );
        if matches!(verdict, Verdict::SessionDeny { .. }) {
            denied += 1;
        }
        state = Some(next);
    }
    // No hash means no frame identity, so this budget stays silent; the
    // per-path budget is what covers that case. Fail open, never fail closed.
    assert_eq!(denied, 0);
}

#[test]
fn a_state_file_without_frames_still_loads() {
    let legacy = r#"{"views":{"/w/cur.png":{"count":2,"last":1000,"denied":false}}}"#;
    let state: State = serde_json::from_str(legacy).unwrap();
    assert_eq!(state.frames.len(), 0);
    assert!(!state.session_denied);
    assert_eq!(state.views.get(PATH).unwrap().count, 2);
}

#[test]
fn the_frame_memory_is_bounded() {
    let mut state: Option<State> = None;
    for index in 0..MAX_FRAMES * 2 {
        let path = format!("/w/b{index}.png");
        let hash = format!("b{index}");
        let (_, next) = decide_frame(
            state.as_ref(),
            &facts(&path, Some(&hash)),
            1_000 + u64::try_from(index).unwrap(),
            LIMIT,
            SESSION_LIMIT,
        );
        state = Some(next);
    }
    assert_eq!(state.unwrap().frames.len(), MAX_FRAMES);
}

#[test]
fn the_session_env_reads_defensively() {
    assert_eq!(session_limit_from(None), SESSION_LIMIT);
    assert_eq!(session_limit_from(Some("40")), 40);
    assert_eq!(session_limit_from(Some(" 0 ")), 0);
    assert_eq!(session_limit_from(Some("nope")), SESSION_LIMIT);
}

#[test]
fn the_session_messages_name_the_cost_and_the_way_out() {
    let nudge = session_nudge_message("cur.png", 13, 2340);
    assert!(nudge.contains("13 image frames"));
    assert!(nudge.contains("2340 KB"));
    assert!(nudge.contains("cur.png"));
    assert!(nudge.contains(SESSION_ENV));

    let deny = session_deny_message("cur.png", 25, 4500);
    assert!(deny.contains("frame 25"));
    assert!(deny.contains("4500 KB"));
    assert!(deny.contains("once per session"));
    assert!(deny.contains(SESSION_ENV));
    assert!(deny.contains(LIMIT_ENV));
}

#[test]
fn the_content_hash_is_stable_and_distinguishes_bytes() {
    let dir = temp_state_dir("hash");
    std::fs::create_dir_all(&dir).unwrap();
    let a = dir.join("a.png");
    let b = dir.join("b.png");
    std::fs::write(&a, vec![7u8; 12 * 1024]).unwrap();
    std::fs::write(&b, vec![7u8; 12 * 1024]).unwrap();
    let ha = content_hash(&a, 12 * 1024).unwrap();
    let hb = content_hash(&b, 12 * 1024).unwrap();
    assert_eq!(ha, hb, "same bytes at two paths must share one hash");
    assert_eq!(ha.len(), 32);
    std::fs::write(&b, vec![8u8; 12 * 1024]).unwrap();
    assert_ne!(ha, content_hash(&b, 12 * 1024).unwrap());
    assert!(content_hash(&dir.join("missing.png"), 1).is_none());
    assert!(content_hash(&a, MAX_HASH_BYTES + 1).is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

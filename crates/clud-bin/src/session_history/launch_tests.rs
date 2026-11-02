use super::*;
use crate::session_history::transcript::tests::{assistant, compact_summary, user};
use serde_json::Value;

fn parse(raw: &[&str]) -> Args {
    Args::parse_from_raw(raw.iter().map(|s| (*s).to_string()).collect())
}

/// A cwd, an empty Claude config dir and a state dir, all temporary.
struct Fixture {
    _root: tempfile::TempDir,
    env: Environment,
}

fn fixture(interactive: bool) -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().join("proj");
    std::fs::create_dir_all(&cwd).unwrap();
    let env = Environment {
        state_dir: root.path().join("state"),
        claude_dir: root.path().join("claude"),
        cwd,
        interactive,
    };
    Fixture { _root: root, env }
}

/// Write a synthetic transcript and index it with an authoritative route.
fn add_session(fx: &Fixture, id: &str, activity: &str, route: Route, records: &[Value]) {
    let path = fx.env.state_dir.join(format!("{id}.jsonl"));
    std::fs::create_dir_all(&fx.env.state_dir).unwrap();
    let lines: Vec<String> = records
        .iter()
        .map(|r| {
            let mut r = r.clone();
            r["sessionId"] = Value::String(id.into());
            r["cwd"] = Value::String(fx.env.cwd.to_string_lossy().into_owned());
            r.to_string()
        })
        .collect();
    std::fs::write(&path, lines.join("\n")).unwrap();
    let cwd = canonical_cwd(&fx.env.cwd);
    index::update(&fx.env.state_dir, &cwd, |index| {
        index.imported = true;
        index.upsert(index::SessionEntry {
            session_id: id.into(),
            transcript_path: path.clone(),
            route: route.clone(),
            route_inferred: false,
            model: None,
            title: Some(format!("title {id}")),
            last_activity: Some(activity.into()),
            compact_checkpoints: 0,
            lineage: None,
        })
    })
    .unwrap();
}

fn simple(fx: &Fixture, id: &str, activity: &str, route: Route) {
    add_session(
        fx,
        id,
        activity,
        route,
        &[user("01", None, "prompt"), assistant("02", "01", "answer")],
    );
}

/// `PENDING_RECOVERY` is process-global; tests that set it take this first.
static PENDING_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn pending_guard() -> std::sync::MutexGuard<'static, ()> {
    PENDING_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn never_pick(_: Vec<Candidate>) -> std::io::Result<PickOutcome> {
    panic!("the picker must not open")
}

#[test]
fn applies_only_to_plain_claude_harness_continues() {
    assert!(applies(&parse(&["clud", "-c"]), None));
    assert!(applies(&parse(&["clud", "--last"]), None));
    assert!(applies(&parse(&["clud", "-c", "--deepseek"]), None));
    assert!(!applies(&parse(&["clud"]), None));
    assert!(!applies(&parse(&["clud", "-c", "-p", "hi"]), None));
    assert!(!applies(&parse(&["clud", "-c", "--codex"]), None));
    assert!(!applies(
        &parse(&["clud", "-c", "--harness", "codex"]),
        None
    ));
    assert!(!applies(&parse(&["clud", "-c", "--dry-run"]), None));
    // A saved Codex harness keeps Codex's own resume unless --claude overrides.
    assert!(!applies(
        &parse(&["clud", "-c"]),
        Some(HarnessSelection::Codex)
    ));
    assert!(applies(
        &parse(&["clud", "-c", "--claude"]),
        Some(HarnessSelection::Codex)
    ));
}

/// RED -> GREEN anchor for #922: before, `-c` only forwarded `--continue`.
/// A non-TTY `-c` still does, byte for byte, so scripts are unaffected.
#[test]
fn non_tty_continue_keeps_native_behavior() {
    let fx = fixture(false);
    simple(&fx, "a", "2026-01-01T00:00:00Z", Route::Claude);
    let mut args = parse(&["clud", "-c"]);
    assert_eq!(prepare(&mut args, &fx.env, never_pick), Ok(None));
    assert!(args.continue_session, "--continue must survive for scripts");
    assert!(args.resume.is_none());
}

#[test]
fn last_resumes_the_newest_session_natively_without_a_picker() {
    let fx = fixture(false);
    simple(&fx, "old", "2026-01-01T00:00:00Z", Route::Claude);
    simple(&fx, "new", "2026-02-01T00:00:00Z", Route::Claude);
    let mut args = parse(&["clud", "--last"]);
    let note = prepare(&mut args, &fx.env, never_pick).unwrap().unwrap();
    assert!(note.contains("title new"), "{note}");
    assert_eq!(args.resume, Some(Some("new".into())));
    assert!(!args.continue_session && !args.last);
    assert_eq!(args.provider, Some(ModelProvider::Claude));
}

/// RED -> GREEN anchor for #2405: `clud -c 0` forwarded `0` as the prompt.
/// `-c N` now resumes the Nth newest session without the picker; 0 is the
/// last save spot.
#[test]
fn continue_index_resumes_the_nth_newest_session_without_a_picker() {
    let fx = fixture(false);
    simple(&fx, "old", "2026-01-01T00:00:00Z", Route::Claude);
    simple(&fx, "new", "2026-02-01T00:00:00Z", Route::Claude);
    for (argv, want) in [
        (vec!["clud", "-c", "0"], "new"),
        (vec!["clud", "--continue", "1"], "old"),
        (vec!["clud", "--continue=0"], "new"),
    ] {
        let mut args = parse(&argv);
        assert!(
            args.passthrough.is_empty(),
            "{argv:?}: {:?}",
            args.passthrough
        );
        assert!(applies(&args, None), "{argv:?}");
        prepare(&mut args, &fx.env, never_pick).unwrap().unwrap();
        assert_eq!(args.resume, Some(Some(want.into())), "{argv:?}");
        assert!(!args.continue_session && args.continue_index.is_none());
    }
    let mut args = parse(&["clud", "-c", "5"]);
    let err = prepare(&mut args, &fx.env, never_pick).unwrap_err();
    assert!(err.contains("no session 5"), "{err}");
    let args = parse(&["clud", "-c", "fix the bug"]);
    assert_eq!(args.continue_index, None);
}

#[test]
fn the_picker_lists_only_this_cwds_sessions_newest_first() {
    let fx = fixture(true);
    simple(&fx, "a", "2026-01-01T00:00:00Z", Route::Claude);
    simple(&fx, "b", "2026-03-01T00:00:00Z", Route::Claude);
    let mut args = parse(&["clud", "-c"]);
    let mut seen = Vec::new();
    prepare(&mut args, &fx.env, |candidates| {
        seen = candidates
            .iter()
            .map(|c| c.entry.session_id.clone())
            .collect();
        Ok(PickOutcome::Selected {
            session_id: "a".into(),
            choice: RecoveryChoice::Full,
        })
    })
    .unwrap();
    assert_eq!(seen, ["b", "a"]);
    assert_eq!(args.resume, Some(Some("a".into())));
}

/// No provider flag: the session's recorded route becomes the launch's
/// provider. A Codex-via-Claude session needs bridge state, so auto recovers
/// portably with a fresh session id and a pending recovery file.
#[test]
fn a_codex_session_launches_on_codex_and_recovers_portably() {
    let _guard = pending_guard();
    let fx = fixture(true);
    simple(
        &fx,
        "cx",
        "2026-01-01T00:00:00Z",
        Route::ViaClaude("Codex".into()),
    );
    let mut args = parse(&["clud", "-c"]);
    let note = prepare(&mut args, &fx.env, |_| {
        Ok(PickOutcome::Selected {
            session_id: "cx".into(),
            choice: RecoveryChoice::Full,
        })
    })
    .unwrap()
    .unwrap();
    assert!(note.contains("recovering"), "{note}");
    assert_eq!(args.provider, Some(ModelProvider::Codex));
    assert_eq!(args.harness, Some(HarnessSelection::Claude));
    let at = args
        .passthrough
        .iter()
        .position(|a| a == "--session-id")
        .unwrap();
    assert_ne!(
        args.passthrough[at + 1],
        "cx",
        "recovery needs a fresh UUID"
    );
    let recovery = take_pending_recovery().expect("recovery file prepared");
    let file: RecoveryFile = serde_json::from_slice(&std::fs::read(&recovery).unwrap()).unwrap();
    assert!(file.context.starts_with(recover::RECOVERY_MARKER));
    assert_eq!(file.lineage.recovered_from, "cx");
    let _ = std::fs::remove_file(recovery);
}

#[test]
fn an_explicit_provider_switch_forks_when_compatible() {
    let fx = fixture(true);
    simple(&fx, "s", "2026-01-01T00:00:00Z", Route::Claude);
    let mut args = parse(&["clud", "-c", "--deepseek"]);
    prepare(&mut args, &fx.env, |_| {
        Ok(PickOutcome::Selected {
            session_id: "s".into(),
            choice: RecoveryChoice::Full,
        })
    })
    .unwrap();
    assert!(args.deepseek, "the explicit provider is kept");
    assert_eq!(args.resume, Some(Some("s".into())));
    assert!(args.passthrough.iter().any(|a| a == "--fork-session"));
}

#[test]
fn choosing_a_checkpoint_recovers_from_it() {
    let _guard = pending_guard();
    let fx = fixture(true);
    add_session(
        &fx,
        "k",
        "2026-01-01T00:00:00Z",
        Route::Claude,
        &[
            user("01", None, "prompt"),
            assistant("02", "01", "answer"),
            compact_summary("03", "02", "THE SUMMARY"),
            assistant("04", "03", "after"),
        ],
    );
    let mut args = parse(&["clud", "-c"]);
    prepare(&mut args, &fx.env, |candidates| {
        assert_eq!(candidates[0].checkpoints.len(), 1);
        Ok(PickOutcome::Selected {
            session_id: "k".into(),
            choice: RecoveryChoice::Checkpoint("03".into()),
        })
    })
    .unwrap();
    let recovery = take_pending_recovery().unwrap();
    let file: RecoveryFile = serde_json::from_slice(&std::fs::read(&recovery).unwrap()).unwrap();
    assert!(file.context.contains("THE SUMMARY"));
    assert_eq!(file.lineage.checkpoint.as_deref(), Some("03"));
    let _ = std::fs::remove_file(recovery);
}

#[test]
fn cancelling_the_picker_aborts_the_launch() {
    let fx = fixture(true);
    simple(&fx, "s", "2026-01-01T00:00:00Z", Route::Claude);
    let mut args = parse(&["clud", "-c"]);
    assert_eq!(
        prepare(&mut args, &fx.env, |_| Ok(PickOutcome::Cancelled)),
        Err("cancelled".to_string())
    );
}

#[test]
fn stale_entries_whose_transcript_is_gone_are_not_offered() {
    let fx = fixture(true);
    simple(&fx, "live", "2026-01-01T00:00:00Z", Route::Claude);
    simple(&fx, "gone", "2026-02-01T00:00:00Z", Route::Claude);
    std::fs::remove_file(fx.env.state_dir.join("gone.jsonl")).unwrap();
    let ids: Vec<String> = candidates(&fx.env.state_dir, &canonical_cwd(&fx.env.cwd))
        .into_iter()
        .map(|c| c.entry.session_id)
        .collect();
    assert_eq!(ids, ["live"]);
}

#[test]
fn native_mode_errors_actionably_for_a_bridge_session() {
    let fx = fixture(true);
    simple(
        &fx,
        "cx",
        "2026-01-01T00:00:00Z",
        Route::ViaClaude("Codex".into()),
    );
    let mut args = parse(&["clud", "-c", "--resume-mode", "native"]);
    let error = prepare(&mut args, &fx.env, |_| {
        Ok(PickOutcome::Selected {
            session_id: "cx".into(),
            choice: RecoveryChoice::Full,
        })
    })
    .unwrap_err();
    assert!(error.contains("--resume-mode portable"), "{error}");
}

#[test]
fn last_with_no_sessions_is_an_error_but_a_bare_continue_falls_back() {
    let fx = fixture(true);
    assert!(prepare(&mut parse(&["clud", "--last"]), &fx.env, never_pick).is_err());
    let mut args = parse(&["clud", "-c"]);
    assert_eq!(prepare(&mut args, &fx.env, never_pick), Ok(None));
    assert!(args.continue_session);
}

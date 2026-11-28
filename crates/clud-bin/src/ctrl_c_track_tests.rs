use super::*;
use kernal_api::platform::fs::TemporaryDirectory;

// Cross-module test serialization lives in `test_state_lock()` so
// `startup::tests` can hold the same mutex while it drives
// `run_ctrl_c_handler` — those tests mutate the same statics.

#[test]
fn invocation_kind_str_round_trips() {
    assert_eq!(InvocationKind::Direct.as_str(), "direct");
    assert_eq!(InvocationKind::Attach.as_str(), "attach");
    assert_eq!(InvocationKind::Centralized.as_str(), "centralized");
}

#[test]
fn ctrl_c_event_round_trips_through_json() {
    let event = CtrlCEvent {
        pid: 1234,
        observed_at_ms: 1_700_000_000_000,
        exit_at_ms: 1_700_000_000_250,
        elapsed_ms: 250,
        kind: InvocationKind::Direct,
        exit_code: 130,
        cwd: Some("/tmp/x".to_string()),
        handed_off: Some(true),
        handoff_reason: Some("ctrl_c_subprocess".to_string()),
        ctrl_event_kind: Some(CtrlEventKind::CtrlBreak),
        forensics: Some(CtrlCForensics {
            captured_at_ms: 1_700_000_000_125,
            current_pid: 1234,
            current_parent_pid: Some(42),
            child_root_pid: Some(5678),
            child_tree_pids: vec![6789],
            ancestor_pids: vec![42],
            console_process_pids: vec![42, 1234, 5678, 6789],
            foreground_window_pid: Some(42),
            processes: vec![CtrlCProcessSnapshot {
                pid: 5678,
                parent_pid: 1234,
                exe: "cmd.exe".to_string(),
                roles: vec!["child_root".to_string(), "same_console".to_string()],
            }],
            source_limit: "win32_console_control_events_do_not_expose_sender_pid".to_string(),
        }),
        press_kind: Some(CtrlPressKind::SecondExit),
        elapsed_since_prior_ms: Some(900),
    };
    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains(r#""kind":"direct""#));
    assert!(json.contains(r#""elapsed_ms":250"#));
    assert!(json.contains(r#""handed_off":true"#));
    assert!(json.contains(r#""handoff_reason":"ctrl_c_subprocess""#));
    assert!(json.contains(r#""ctrl_event_kind":"ctrl_break""#));
    assert!(json.contains(r#""source_limit":"#));
    assert!(json.contains(r#""press_kind":"second_exit""#));
    assert!(json.contains(r#""elapsed_since_prior_ms":900"#));
    let back: CtrlCEvent = serde_json::from_str(&json).unwrap();
    assert_eq!(back.pid, 1234);
    assert_eq!(back.elapsed_ms, 250);
    assert_eq!(back.kind, InvocationKind::Direct);
    assert_eq!(back.cwd.as_deref(), Some("/tmp/x"));
    assert_eq!(back.handed_off, Some(true));
    assert_eq!(back.handoff_reason.as_deref(), Some("ctrl_c_subprocess"));
    assert_eq!(back.ctrl_event_kind, Some(CtrlEventKind::CtrlBreak));
    assert_eq!(back.press_kind, Some(CtrlPressKind::SecondExit));
    assert_eq!(back.elapsed_since_prior_ms, Some(900));
    let forensics = back.forensics.expect("forensics round-tripped");
    assert_eq!(forensics.child_root_pid, Some(5678));
    assert_eq!(forensics.console_process_pids, vec![42, 1234, 5678, 6789]);
    assert_eq!(forensics.processes[0].exe, "cmd.exe");
}

#[test]
fn ctrl_c_event_parses_legacy_files_without_handoff_fields() {
    // Pre-issue-#285 event files have no `handed_off` / `handoff_reason`
    // fields. `#[serde(default)]` must make them parse cleanly so the
    // dashboard doesn't lose history when the binary is upgraded.
    let legacy = r#"{
            "pid": 1234,
            "observed_at_ms": 1700000000000,
            "exit_at_ms": 1700000000250,
            "elapsed_ms": 250,
            "kind": "direct",
            "exit_code": 130,
            "cwd": "/tmp/x"
        }"#;
    let event: CtrlCEvent = serde_json::from_str(legacy).unwrap();
    assert_eq!(event.pid, 1234);
    assert_eq!(event.handed_off, None);
    assert_eq!(event.handoff_reason, None);
    assert_eq!(event.ctrl_event_kind, None);
    assert_eq!(event.forensics, None);
    assert_eq!(event.press_kind, None);
    assert_eq!(event.elapsed_since_prior_ms, None);
}

#[test]
fn ctrl_event_kind_round_trips_through_raw() {
    for kind in [
        CtrlEventKind::CtrlC,
        CtrlEventKind::CtrlBreak,
        CtrlEventKind::CtrlClose,
        CtrlEventKind::CtrlLogoff,
        CtrlEventKind::CtrlShutdown,
        CtrlEventKind::Term,
        CtrlEventKind::Hup,
        CtrlEventKind::Quit,
        CtrlEventKind::Unknown,
    ] {
        let raw = kind.to_raw();
        assert_eq!(
            CtrlEventKind::from_raw(raw),
            kind,
            "round-trip failed for {kind:?} -> {raw}"
        );
    }
}

#[test]
fn ctrl_event_kind_from_raw_maps_undefined_to_unknown() {
    // Windows reserves 3, 4, and 7-99 as undocumented / future-use
    // values (100+ is reserved by this crate for Unix-only variants).
    // Anything outside the known set must funnel into Unknown so a
    // future Windows revision can't crash forensics.
    for raw in [3u32, 4, 7, 99, 103, u32::MAX, u32::MAX - 1] {
        assert_eq!(CtrlEventKind::from_raw(raw), CtrlEventKind::Unknown);
    }
}

#[test]
fn ctrl_event_kind_serializes_as_snake_case() {
    // Lock in the on-disk JSON spelling. Dashboard consumers and
    // downstream telemetry depend on these literal strings.
    assert_eq!(
        serde_json::to_string(&CtrlEventKind::CtrlC).unwrap(),
        "\"ctrl_c\""
    );
    assert_eq!(
        serde_json::to_string(&CtrlEventKind::CtrlBreak).unwrap(),
        "\"ctrl_break\""
    );
    assert_eq!(
        serde_json::to_string(&CtrlEventKind::CtrlClose).unwrap(),
        "\"ctrl_close\""
    );
    assert_eq!(
        serde_json::to_string(&CtrlEventKind::CtrlLogoff).unwrap(),
        "\"ctrl_logoff\""
    );
    assert_eq!(
        serde_json::to_string(&CtrlEventKind::CtrlShutdown).unwrap(),
        "\"ctrl_shutdown\""
    );
    assert_eq!(
        serde_json::to_string(&CtrlEventKind::Term).unwrap(),
        "\"term\""
    );
    assert_eq!(
        serde_json::to_string(&CtrlEventKind::Hup).unwrap(),
        "\"hup\""
    );
    assert_eq!(
        serde_json::to_string(&CtrlEventKind::Quit).unwrap(),
        "\"quit\""
    );
    assert_eq!(
        serde_json::to_string(&CtrlEventKind::Unknown).unwrap(),
        "\"unknown\""
    );
}

#[test]
fn record_event_kind_round_trips_through_observed_event_kind() {
    let _guard = test_state_lock().lock().unwrap_or_else(|e| e.into_inner());
    reset_for_test();
    assert_eq!(observed_event_kind(), None);
    record_event_kind(CtrlEventKind::CtrlClose);
    assert_eq!(observed_event_kind(), Some(CtrlEventKind::CtrlClose));
    // Last writer wins, matching the timestamp semantics.
    record_event_kind(CtrlEventKind::CtrlBreak);
    assert_eq!(observed_event_kind(), Some(CtrlEventKind::CtrlBreak));
}

#[test]
fn build_event_carries_recorded_event_kind() {
    let _guard = test_state_lock().lock().unwrap_or_else(|e| e.into_inner());
    reset_for_test();
    record_observed();
    record_event_kind(CtrlEventKind::CtrlBreak);
    let event = build_event(InvocationKind::Direct, 130).expect("event built");
    assert_eq!(event.ctrl_event_kind, Some(CtrlEventKind::CtrlBreak));
}

#[test]
fn build_event_leaves_event_kind_none_when_probe_never_fired() {
    let _guard = test_state_lock().lock().unwrap_or_else(|e| e.into_inner());
    reset_for_test();
    record_observed();
    // No `record_event_kind` call — emulates Unix or pre-probe Windows.
    let event = build_event(InvocationKind::Direct, 130).expect("event built");
    assert_eq!(event.ctrl_event_kind, None);
}

#[test]
fn read_recent_events_returns_empty_when_dir_missing() {
    let tmp = TemporaryDirectory::new().unwrap();
    let events = read_recent_events(tmp.path(), 10);
    assert!(events.is_empty());
}

#[test]
fn read_recent_events_returns_newest_first_and_respects_limit() {
    let tmp = TemporaryDirectory::new().unwrap();
    let dir = events_dir(tmp.path());
    fs::create_dir_all(&dir).unwrap();
    for i in 0..5u64 {
        let event = CtrlCEvent {
            pid: 100 + i as u32,
            observed_at_ms: 1_700_000_000_000 + i * 1000,
            exit_at_ms: 1_700_000_000_500 + i * 1000,
            elapsed_ms: 500,
            kind: InvocationKind::Direct,
            exit_code: 130,
            cwd: None,
            handed_off: None,
            handoff_reason: None,
            ctrl_event_kind: None,
            forensics: None,
            press_kind: None,
            elapsed_since_prior_ms: None,
        };
        let path = dir.join(format!("{:013}-{}.json", event.exit_at_ms, event.pid));
        fs::write(&path, serde_json::to_vec(&event).unwrap()).unwrap();
    }
    let events = read_recent_events(tmp.path(), 3);
    assert_eq!(events.len(), 3);
    // Newest first means the largest exit_at_ms comes first.
    assert_eq!(events[0].exit_at_ms, 1_700_000_000_500 + 4_000);
    assert_eq!(events[1].exit_at_ms, 1_700_000_000_500 + 3_000);
    assert_eq!(events[2].exit_at_ms, 1_700_000_000_500 + 2_000);
}

#[test]
fn read_recent_events_skips_unparseable_files() {
    let tmp = TemporaryDirectory::new().unwrap();
    let dir = events_dir(tmp.path());
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("garbage.json"), b"not json").unwrap();
    let good = CtrlCEvent {
        pid: 1,
        observed_at_ms: 100,
        exit_at_ms: 200,
        elapsed_ms: 100,
        kind: InvocationKind::Attach,
        exit_code: 130,
        cwd: None,
        handed_off: Some(true),
        handoff_reason: None,
        ctrl_event_kind: None,
        forensics: None,
        press_kind: None,
        elapsed_since_prior_ms: None,
    };
    fs::write(dir.join("good.json"), serde_json::to_vec(&good).unwrap()).unwrap();
    let events = read_recent_events(tmp.path(), 10);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].pid, 1);
    assert_eq!(events[0].handed_off, Some(true));
}

#[test]
fn prune_old_events_keeps_newest() {
    let tmp = TemporaryDirectory::new().unwrap();
    let dir = events_dir(tmp.path());
    fs::create_dir_all(&dir).unwrap();
    // Create 10 files with monotonically-increasing mtime by writing
    // them in order; on most filesystems that's enough to differentiate.
    for i in 0..10u64 {
        let path = dir.join(format!("evt-{i:02}.json"));
        fs::write(&path, b"{}").unwrap();
        // Tiny sleep so mtimes can differ on coarse-grained filesystems.
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    prune_old_events(&dir, 3);
    let remaining = fs::read_dir(&dir).unwrap().count();
    assert_eq!(remaining, 3, "prune must keep exactly the cap");
}

#[test]
fn flush_on_exit_is_noop_when_never_observed() {
    let _guard = test_state_lock().lock().unwrap_or_else(|e| e.into_inner());
    // Reset so prior tests in this module can't pollute the static
    // observation point. After reset, `was_observed` must be false
    // and `flush_on_exit` must write nothing.
    reset_for_test();
    let tmp = TemporaryDirectory::new().unwrap();
    flush_on_exit(tmp.path(), InvocationKind::Direct, 0);
    let dir = events_dir(tmp.path());
    if dir.exists() {
        // Directory creation only happens inside write_event; if it
        // exists, no event files should be inside.
        let count = fs::read_dir(&dir).unwrap().count();
        assert_eq!(count, 0);
    }
}

/// Issue #285 rec 1: every Ctrl+C must re-stamp the observation
/// point. The prior `OnceLock` design only stamped the first press,
/// so a user who pressed Ctrl+C once mid-session would see the
/// entire intervening time attributed to the eventual shutdown.
#[test]
fn record_observed_updates_on_every_call() {
    let _guard = test_state_lock().lock().unwrap_or_else(|e| e.into_inner());
    reset_for_test();
    record_observed();
    let first = OBSERVED_UNIX_MS.load(Ordering::SeqCst);
    assert!(first > 0, "first observation must stamp");
    // Sleep long enough that the wall clock advances at least 1ms
    // even on coarse-grained Windows timers (typically 15ms tick).
    std::thread::sleep(std::time::Duration::from_millis(20));
    record_observed();
    let second = OBSERVED_UNIX_MS.load(Ordering::SeqCst);
    assert!(
        second > first,
        "second observation must overwrite the first (got {second} vs {first})"
    );
}

/// Issue #285 rec 2: the handoff outcome recorded by the teardown
/// site must propagate into the event file so the dashboard can
/// distinguish "daemon adopted" from "synchronous fallback".
#[test]
fn record_handoff_propagates_to_event() {
    let _guard = test_state_lock().lock().unwrap_or_else(|e| e.into_inner());
    reset_for_test();
    record_observed();
    record_handoff(true, Some("ctrl_c_subprocess"));
    let event = build_event(InvocationKind::Direct, 130).expect("event built");
    assert_eq!(event.handed_off, Some(true));
    assert_eq!(event.handoff_reason.as_deref(), Some("ctrl_c_subprocess"));
}

#[test]
fn record_handoff_failure_surfaces_reason() {
    let _guard = test_state_lock().lock().unwrap_or_else(|e| e.into_inner());
    reset_for_test();
    record_observed();
    record_handoff(false, Some("daemon_unreachable"));
    let event = build_event(InvocationKind::Direct, 130).expect("event built");
    assert_eq!(event.handed_off, Some(false));
    assert_eq!(event.handoff_reason.as_deref(), Some("daemon_unreachable"));
}

#[test]
fn build_event_without_handoff_leaves_fields_none() {
    let _guard = test_state_lock().lock().unwrap_or_else(|e| e.into_inner());
    // When neither teardown site fires (e.g. `clud --no-daemon` exits
    // before reaching the teardown helper), the event must still be
    // written but with the handoff fields left as None so the
    // dashboard can show "unknown" rather than claiming a fast path.
    reset_for_test();
    record_observed();
    let event = build_event(InvocationKind::Direct, 130).expect("event built");
    assert_eq!(event.handed_off, None);
    assert_eq!(event.handoff_reason, None);
}

// ---------------------------------------------------------------
// Issue #377: double-Ctrl+C guard infrastructure.
// ---------------------------------------------------------------

#[test]
fn ctrl_press_kind_round_trips_through_raw() {
    for kind in [CtrlPressKind::FirstSoft, CtrlPressKind::SecondExit] {
        let raw = kind.to_raw();
        assert_eq!(
            CtrlPressKind::from_raw(raw),
            Some(kind),
            "round-trip failed for {kind:?} -> {raw}"
        );
    }
    assert_eq!(
        CtrlPressKind::from_raw(PRESS_KIND_UNRECORDED),
        None,
        "sentinel decodes to None"
    );
    // Anything outside the known set collapses to SecondExit — the
    // safe direction, since misreading "second" as "first" would
    // silently suppress a real teardown.
    assert_eq!(CtrlPressKind::from_raw(99), Some(CtrlPressKind::SecondExit));
}

#[test]
fn ctrl_press_kind_serializes_as_snake_case() {
    assert_eq!(
        serde_json::to_string(&CtrlPressKind::FirstSoft).unwrap(),
        "\"first_soft\""
    );
    assert_eq!(
        serde_json::to_string(&CtrlPressKind::SecondExit).unwrap(),
        "\"second_exit\""
    );
}

#[test]
fn record_observed_returning_prior_swaps_in_new_timestamp() {
    let _guard = test_state_lock().lock().unwrap_or_else(|e| e.into_inner());
    reset_for_test();
    // First call: no prior observation, must return 0.
    let prior_first = record_observed_returning_prior();
    assert_eq!(prior_first, 0, "first call must report 0 prior");
    let stamped_first = OBSERVED_UNIX_MS.load(Ordering::SeqCst);
    assert!(stamped_first > 0, "first call must stamp a real value");
    // Second call: must return the value stamped by the first.
    std::thread::sleep(std::time::Duration::from_millis(20));
    let prior_second = record_observed_returning_prior();
    assert_eq!(
        prior_second, stamped_first,
        "second call must report the value stamped by the first"
    );
    let stamped_second = OBSERVED_UNIX_MS.load(Ordering::SeqCst);
    assert!(stamped_second > stamped_first);
}

#[test]
fn record_press_kind_round_trips_through_observed_press_kind() {
    let _guard = test_state_lock().lock().unwrap_or_else(|e| e.into_inner());
    reset_for_test();
    assert_eq!(observed_press_kind(), None);
    record_press_kind(CtrlPressKind::FirstSoft);
    assert_eq!(observed_press_kind(), Some(CtrlPressKind::FirstSoft));
    // Last writer wins, matching the timestamp + event-kind semantics.
    record_press_kind(CtrlPressKind::SecondExit);
    assert_eq!(observed_press_kind(), Some(CtrlPressKind::SecondExit));
}

#[test]
fn record_elapsed_since_prior_ms_round_trips() {
    let _guard = test_state_lock().lock().unwrap_or_else(|e| e.into_inner());
    reset_for_test();
    assert_eq!(observed_elapsed_since_prior_ms(), None);
    record_elapsed_since_prior_ms(1234);
    assert_eq!(observed_elapsed_since_prior_ms(), Some(1234));
    // Zero collapses back to None — both "no prior press" and "the
    // clock didn't advance" map to "we don't know the gap", and the
    // field is documented as optional.
    record_elapsed_since_prior_ms(0);
    assert_eq!(observed_elapsed_since_prior_ms(), None);
}

#[test]
fn build_event_carries_press_kind_and_elapsed_since_prior() {
    let _guard = test_state_lock().lock().unwrap_or_else(|e| e.into_inner());
    reset_for_test();
    record_observed();
    record_press_kind(CtrlPressKind::SecondExit);
    record_elapsed_since_prior_ms(700);
    let event = build_event(InvocationKind::Direct, 130).expect("event built");
    assert_eq!(event.press_kind, Some(CtrlPressKind::SecondExit));
    assert_eq!(event.elapsed_since_prior_ms, Some(700));
}

#[test]
fn build_event_leaves_press_kind_none_when_guard_never_fired() {
    let _guard = test_state_lock().lock().unwrap_or_else(|e| e.into_inner());
    reset_for_test();
    record_observed();
    // No record_press_kind / record_elapsed_since_prior_ms call —
    // emulates non-Windows paths, opt-out via env var, or the
    // very first press in a fresh process.
    let event = build_event(InvocationKind::Direct, 130).expect("event built");
    assert_eq!(event.press_kind, None);
    assert_eq!(event.elapsed_since_prior_ms, None);
}

/// The env-var window override must accept reasonable values and
/// reject garbage / out-of-range values without blowing up — the
/// guard fires from a signal-handler-adjacent path and must never
/// panic on bad input.
#[test]
fn double_tap_window_ms_reads_env_var_with_bounds() {
    // Take the STATE_LOCK so the env-var mutation below doesn't
    // race tests that also touch the static observation state.
    let _guard = test_state_lock().lock().unwrap_or_else(|e| e.into_inner());
    // Default when unset / empty.
    std::env::remove_var(ENV_DOUBLE_TAP_WINDOW_MS);
    assert_eq!(double_tap_window_ms(), DOUBLE_TAP_WINDOW_MS_DEFAULT);
    std::env::set_var(ENV_DOUBLE_TAP_WINDOW_MS, "");
    assert_eq!(double_tap_window_ms(), DOUBLE_TAP_WINDOW_MS_DEFAULT);
    // Accepts reasonable values.
    std::env::set_var(ENV_DOUBLE_TAP_WINDOW_MS, "2500");
    assert_eq!(double_tap_window_ms(), 2500);
    // Rejects out-of-range and garbage; falls back to default.
    for bad in ["0", "10", "10001", "not-a-number", "  "] {
        std::env::set_var(ENV_DOUBLE_TAP_WINDOW_MS, bad);
        assert_eq!(
            double_tap_window_ms(),
            DOUBLE_TAP_WINDOW_MS_DEFAULT,
            "bad input {bad:?} must fall back to default"
        );
    }
    std::env::remove_var(ENV_DOUBLE_TAP_WINDOW_MS);
}

// The host-specific `double_tap_enabled` tests live in
// `platform/{windows,unix}/ctrl_c_track.rs`.

/// #520: the recorded signal sender round-trips, and clearing it or a
/// reset leaves none.
#[test]
fn signal_origin_round_trips_and_clears() {
    let _guard = test_state_lock().lock().unwrap_or_else(|e| e.into_inner());
    reset_for_test();
    assert_eq!(observed_signal_origin(), None);
    let origin = SignalOrigin {
        pid: 4242,
        uid: 1000,
    };
    record_signal_origin(Some(origin));
    assert_eq!(observed_signal_origin(), Some(origin));
    record_signal_origin(None);
    assert_eq!(observed_signal_origin(), None);
    record_signal_origin(Some(origin));
    reset_for_test();
    assert_eq!(observed_signal_origin(), None);
}

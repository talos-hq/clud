use super::*;

// Issue #548: the parking policy. Both conditions must hold to park, and
// each alone is a reason to stay fast.

#[test]
fn an_idle_daemon_with_no_sessions_parks() {
    assert_eq!(
        decide_cadence(CONSUMER_IDLE_GRACE_MS, 0, CONSUMER_IDLE_GRACE_MS),
        SampleCadence::Parked
    );
}

#[test]
fn a_live_session_keeps_the_fast_cadence_to_preserve_its_history() {
    // Not "because the daemon reconciles the tree on its behalf" -- it
    // does not, see decide_cadence's doc comment and #722. Parking here
    // would coarsen the EWMA 15x and let short-lived children live and die
    // entirely between two ticks, so a consumer attaching later would read
    // a history with holes in it.
    assert_eq!(
        decide_cadence(CONSUMER_IDLE_GRACE_MS * 100, 1, CONSUMER_IDLE_GRACE_MS),
        SampleCadence::Active
    );
}

#[test]
fn a_recent_consumer_keeps_the_fast_cadence_on_an_empty_machine() {
    // `clud top` against a machine with no sessions still expects numbers
    // that move.
    assert_eq!(
        decide_cadence(0, 0, CONSUMER_IDLE_GRACE_MS),
        SampleCadence::Active
    );
    // The boundary is exclusive: exactly at the grace window, park.
    assert_eq!(
        decide_cadence(CONSUMER_IDLE_GRACE_MS - 1, 0, CONSUMER_IDLE_GRACE_MS),
        SampleCadence::Active
    );
}

#[test]
fn parking_never_speeds_up_a_deliberately_slow_operator_setting() {
    // An operator who configured a 60 s cadence must not be sped up to the
    // 30 s parked interval -- parking may only ever reduce work.
    let slow = 60_000;
    assert_eq!(SampleCadence::Parked.interval_ms(slow), slow);
    assert_eq!(SampleCadence::Active.interval_ms(slow), slow);
    // And with the default, parking does slow things down as intended.
    assert_eq!(
        SampleCadence::Parked.interval_ms(DEFAULT_SAMPLE_INTERVAL_MS),
        PARKED_SAMPLE_INTERVAL_MS
    );
}

// Issue #548: the host environment scan is the expensive half, and
// parking did not previously reach it.

#[test]
fn a_parked_tick_never_scans_the_host_environment() {
    // Even when the interval has elapsed — which, before this, it always
    // had: the parked interval and the scan interval are both 30 s, so a
    // fully idle daemon paid a full-host PEB walk on every parked tick.
    assert!(!originator_scan_due(SampleCadence::Parked, true));
    assert!(!originator_scan_due(SampleCadence::Parked, false));
}

#[test]
fn an_active_tick_still_honours_the_scan_interval() {
    assert!(originator_scan_due(SampleCadence::Active, true));
    assert!(!originator_scan_due(SampleCadence::Active, false));
}

/// The regression this guards: the two intervals are equal, so a parked
/// tick and a due scan coincide on *every* parked tick rather than
/// occasionally. If someone later re-couples the scan to the interval
/// alone, an idle daemon silently goes back to one full-host PEB walk
/// every 30 s.
#[test]
fn the_parked_and_scan_intervals_coincide_which_is_why_the_gate_matters() {
    assert_eq!(
        PARKED_SAMPLE_INTERVAL_MS, ORIGINATOR_SCAN_INTERVAL_MS,
        "if these diverge, revisit the reasoning in originator_scan_due"
    );
    assert!(!originator_scan_due(SampleCadence::Parked, true));
}

#[test]
fn requesting_a_snapshot_records_the_consumer() {
    // This is what wakes a parked sampler; without it the handle looks
    // idle no matter how often `clud top` runs.
    let handle = ProcSamplerHandle::empty(DEFAULT_SAMPLE_INTERVAL_MS);
    handle.last_request_ms.store(0, Ordering::Relaxed);
    let _ = handle.snapshot(0);
    assert!(
        handle.last_request_ms.load(Ordering::Relaxed) > 0,
        "snapshot() must record the request"
    );
}

fn session_index(session: SessionRoot) -> SessionIndex {
    let mut index = SessionIndex::default();
    index.root_to_session.insert(session.worker_pid, 0);
    if let Some(root_pid) = session.root_pid {
        index.root_to_session.insert(root_pid, 0);
    }
    index.sessions.push(session);
    index
}

#[test]
fn ewma_applies_expected_decay() {
    let first = ewma(None, 10.0);
    assert!((first - 10.0).abs() < f32::EPSILON);
    let second = ewma(Some(first), 0.0);
    assert!((second - 7.0).abs() < 0.001);
    let third = ewma(Some(second), 10.0);
    assert!((third - 7.9).abs() < 0.001);
}

#[test]
fn originator_tag_wins_over_parent_chain_fallback() {
    let parents = HashMap::from([(300_u32, 200_u32), (200, 100)]);
    let tags = HashMap::from([(
        300_u32,
        OriginatorTag {
            originator: "CLUD:900".to_string(),
            originator_pid: 900,
            command: "tagged".to_string(),
            start_time: 1,
        },
    )]);
    let sessions = session_index(SessionRoot {
        id: "sess-a".to_string(),
        name: Some("build".to_string()),
        worker_pid: 100,
        root_pid: Some(200),
    });

    let assignment = resolve_assignment(300, &parents, &tags, &sessions).unwrap();
    assert_eq!(assignment.originator, "CLUD:900");
    assert_eq!(assignment.originator_pid, Some(900));
}

#[test]
fn parent_chain_falls_back_to_session_worker_originator() {
    let parents = HashMap::from([(300_u32, 200_u32), (200, 100)]);
    let tags = HashMap::new();
    let sessions = session_index(SessionRoot {
        id: "sess-a".to_string(),
        name: Some("build".to_string()),
        worker_pid: 100,
        root_pid: Some(200),
    });

    let assignment = resolve_assignment(300, &parents, &tags, &sessions).unwrap();
    assert_eq!(assignment.originator, "CLUD:100");
    assert_eq!(assignment.originator_pid, Some(100));
    assert_eq!(assignment.session_id.as_deref(), Some("sess-a"));
    assert_eq!(
        depth_from_originator(300, assignment.originator_pid, &parents),
        2
    );
}

#[test]
fn dead_rows_are_retained_and_marked_frozen() {
    let tmp = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let mut sampler = ProcSampler::new(
        tmp.path().to_path_buf(),
        DEFAULT_SAMPLE_INTERVAL_MS,
        Arc::new(crate::process_scan::EnvScanCache::new()),
    );
    let live = ProcRow {
        pid: 10,
        ppid: Some(1),
        originator: "CLUD:1".to_string(),
        originator_pid: Some(1),
        session_id: None,
        session_name: None,
        cpu_pct: 12.0,
        cpu_ewma_pct: 12.0,
        rss_bytes: 10,
        age_secs: 1,
        start_time: 0,
        command: "x".to_string(),
        depth: 1,
        tier: ProcTier::Hot,
        live: true,
        exited_at_ms: None,
    };
    sampler.last_live_rows.insert(10, live);
    sampler.ewma_by_pid.insert(10, 12.0);

    sampler.record_dead_rows(1_000, &HashMap::new());

    let dead = sampler.dead_rows.get(&10).unwrap();
    assert!(!dead.live);
    assert_eq!(dead.exited_at_ms, Some(1_000));
    assert_eq!(dead.cpu_pct, 0.0);
    assert_eq!(dead.tier, ProcTier::Frozen);
}

#[test]
fn proc_snapshot_filters_dead_rows_by_requested_window() {
    let handle = ProcSamplerHandle::empty(DEFAULT_SAMPLE_INTERVAL_MS);
    {
        let mut snapshot = handle.snapshot.lock().unwrap();
        snapshot.sampled_at_ms = unix_millis_now();
        snapshot.rows.push(ProcRow {
            pid: 10,
            ppid: None,
            originator: "CLUD:1".to_string(),
            originator_pid: Some(1),
            session_id: None,
            session_name: None,
            cpu_pct: 0.0,
            cpu_ewma_pct: 0.0,
            rss_bytes: 0,
            age_secs: 0,
            start_time: 0,
            command: "dead".to_string(),
            depth: 0,
            tier: ProcTier::Frozen,
            live: false,
            exited_at_ms: Some(unix_millis_now()),
        });
        let mut zombie = snapshot.rows[0].clone();
        zombie.pid = 11;
        zombie.ppid = Some(1);
        zombie.exited_at_ms = None;
        snapshot.rows.push(zombie);
    }

    let current = handle.snapshot(0);
    assert_eq!(current.rows.len(), 1);
    assert_eq!(current.rows[0].pid, 11);
    assert!(!current.rows[0].live);
    assert_eq!(handle.snapshot(5_000).rows.len(), 2);
}

#[test]
fn proc_snapshot_serde_roundtrips() {
    let mut snapshot = ProcTreeSnapshot::empty(DEFAULT_SAMPLE_INTERVAL_MS);
    snapshot.rows.push(ProcRow {
        pid: 42,
        ppid: Some(1),
        originator: "CLUD:1".to_string(),
        originator_pid: Some(1),
        session_id: Some("sess".to_string()),
        session_name: None,
        cpu_pct: 1.25,
        cpu_ewma_pct: 0.75,
        rss_bytes: 4096,
        age_secs: 9,
        start_time: 0,
        command: "worker".to_string(),
        depth: 1,
        tier: ProcTier::Warm,
        live: true,
        exited_at_ms: None,
    });
    snapshot.recompute_summary();

    let wire = serde_json::to_string(&snapshot).unwrap();
    let parsed: ProcTreeSnapshot = serde_json::from_str(&wire).unwrap();
    assert_eq!(parsed, snapshot);
}

#[test]
fn summary_counts_unique_originators() {
    let mut snapshot = ProcTreeSnapshot::empty(DEFAULT_SAMPLE_INTERVAL_MS);
    for pid in [1_u32, 2, 3] {
        snapshot.rows.push(ProcRow {
            pid,
            ppid: None,
            originator: if pid == 3 { "CLUD:9" } else { "CLUD:1" }.to_string(),
            originator_pid: Some(1),
            session_id: None,
            session_name: None,
            cpu_pct: 1.0,
            cpu_ewma_pct: 1.0,
            rss_bytes: 10,
            age_secs: 0,
            start_time: 0,
            command: "x".to_string(),
            depth: 0,
            tier: ProcTier::Cold,
            live: true,
            exited_at_ms: None,
        });
    }
    snapshot.recompute_summary();
    assert_eq!(snapshot.summary.process_count, 3);
    assert_eq!(snapshot.summary.originator_count, 2);
    assert_eq!(snapshot.summary.total_rss_bytes, 30);
}

use super::extern_repo::PurgeClass;
use super::*;
use std::ffi::OsString;
use std::fs;
use std::sync::Mutex;

#[path = "tests/parallel.rs"]
mod parallel;
#[path = "tests/policy.rs"]
mod policy;
#[path = "tests/registry_retry.rs"]
mod registry_retry;
use policy::{restore_env_var, spawn_test_worker};

// Serializes the tests in this module that share process-global state
// (env vars such as the GC tick and purge settings).
static TEST_LOCK: Mutex<()> = Mutex::new(());

fn spawn_test_worker_with_tick(
    db_path: &Path,
    tick_secs: &str,
) -> (
    mpsc::Sender<RegistryMsg>,
    std::sync::MutexGuard<'static, ()>,
) {
    let guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // Bind the worker to this test's file directly: a process-global
    // `CLUD_DATA_DB` set here can be swapped by any other test thread
    // between the set and the open ("Database already open").
    let registry = Registry::open_at(db_path).expect("open registry");
    let tx = spawn_registry_worker_with_live_cwds_and_cadence(
        registry,
        Arc::new(Vec::<PathBuf>::new),
        gc_tick_cadence_from_raw(Some(tick_secs)),
        None,
        None,
    )
    .expect("spawn registry worker");
    (tx, guard)
}

fn spawn_test_worker_with_live_cwds(
    db_path: &Path,
    live_cwds: Vec<PathBuf>,
) -> mpsc::Sender<RegistryMsg> {
    let registry = Registry::open_at(db_path).expect("open registry");
    spawn_registry_worker_with_live_cwds(registry, Arc::new(move || live_cwds.clone()))
        .expect("spawn registry worker")
}

struct ScopedEnv {
    key: &'static str,
    prior: Option<OsString>,
}

impl ScopedEnv {
    fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
        let prior = std::env::var_os(key);
        std::env::set_var(key, value);
        Self { key, prior }
    }
}

impl Drop for ScopedEnv {
    fn drop(&mut self) {
        restore_env_var(self.key, self.prior.take());
    }
}

/// How long a single worker round-trip may take before it is treated as slow.
/// The registry worker is single-threaded, so while it applies a bulk purge's
/// completions an unrelated `List` queues behind them.
const CALL_TIMEOUT: Duration = Duration::from_secs(5);

/// One request/response round-trip, returning `Err` on timeout rather than
/// panicking (issue #594).
///
/// The difference matters inside a polling loop: [`wait_for_row_count`] owns a
/// deadline of its own and is built to absorb a slow reply, but the previous
/// `recv_timeout(..).unwrap()` aborted the whole test on the first slow one, so
/// the outer deadline never got to do its job. That is how
/// `bulk_purge_keeps_serving_list_while_pool_grinds_through` failed on a loaded
/// Windows runner: 50 `remove_dir_all` calls kept the worker busy past five
/// seconds, and the panic carried no timing information at all.
fn try_call(
    tx: &mpsc::Sender<RegistryMsg>,
    op: GcOp,
    timeout: Duration,
) -> Result<GcReply, mpsc::RecvTimeoutError> {
    let (reply_tx, reply_rx) = mpsc::sync_channel::<GcReply>(1);
    tx.send(RegistryMsg::Op(GcRequestMsg { op, reply_tx }))
        .unwrap();
    reply_rx.recv_timeout(timeout)
}

/// One round-trip that must succeed. Use where a slow worker is itself the
/// bug; prefer [`try_call`] inside a loop that already has a deadline.
fn call(tx: &mpsc::Sender<RegistryMsg>, op: GcOp) -> GcReply {
    let started = Instant::now();
    match try_call(tx, op, CALL_TIMEOUT) {
        Ok(reply) => reply,
        Err(err) => panic!(
            "registry worker did not reply within {CALL_TIMEOUT:?} (waited {:?}): {err}",
            started.elapsed()
        ),
    }
}

/// Block (polling `GcOp::List`) until the worker reports
/// `target_count` rows of the given kind, or `timeout` elapses.
/// Used to bridge the asynchronous gap between a bulk
/// `PurgeStarted` reply and the matching completions reaching the
/// worker thread.
fn wait_for_row_count(
    tx: &mpsc::Sender<RegistryMsg>,
    kind: Option<&str>,
    target_count: usize,
    timeout: Duration,
) -> Vec<ListRow> {
    wait_for_row_count_with_budget(tx, kind, target_count, timeout, CALL_TIMEOUT)
}

/// [`wait_for_row_count`] with the per-round-trip budget exposed.
///
/// A round-trip that times out counts as "not yet" and is retried until the
/// caller's own deadline, which is the point of having one. On expiry the panic
/// names how long it waited and how many round-trips were slow, so the next
/// occurrence says whether the worker was merely busy or wedged — the question
/// #594 is trying to answer, and one the old panic destroyed the evidence for.
///
/// Only the tests for this helper pass anything but [`CALL_TIMEOUT`]: proving a
/// slow reply is absorbed needs a round-trip that actually exceeds its budget,
/// and stalling a worker for five seconds to arrange that would be five seconds
/// of dead test time.
fn wait_for_row_count_with_budget(
    tx: &mpsc::Sender<RegistryMsg>,
    kind: Option<&str>,
    target_count: usize,
    timeout: Duration,
    call_budget: Duration,
) -> Vec<ListRow> {
    let started = Instant::now();
    let deadline = started + timeout;
    let mut slow_calls = 0usize;
    let mut last_rows: Option<Vec<ListRow>> = None;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        // Never wait past the caller's deadline, and always leave the loop a
        // chance to observe expiry.
        let budget = call_budget.min(remaining.max(Duration::from_millis(1)));
        match try_call(
            tx,
            GcOp::List {
                kind: kind.map(String::from),
            },
            budget,
        ) {
            Ok(GcReply::ListOk { rows }) => {
                if rows.len() == target_count {
                    return rows;
                }
                last_rows = Some(rows);
            }
            Ok(other) => panic!("unexpected reply: {other:?}"),
            Err(_) => slow_calls += 1,
        }

        if Instant::now() >= deadline {
            return last_rows.unwrap_or_else(|| {
                panic!(
                    "worker never answered a List in {:?} ({slow_calls} round-trip(s) \
                     exceeded {call_budget:?}); expected {target_count} row(s)",
                    started.elapsed()
                )
            });
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// Apply exactly `expected` purge completions from `rx`, or fail with the
/// shortfall.
///
/// Prefer this to [`drain_purge_completions`] whenever the dispatch count is
/// known. Issue #560: a quiet interval is not evidence that a purge finished —
/// two directory removals dispatched to a two-worker pool can land more than
/// any fixed gap apart under AV-scanner or `TempDir` contention on Windows,
/// and the drain would return after the first one with the test none the
/// wiser. Waiting for a count the *producer* reported turns that from a
/// timing guess into an assertion.
///
/// `timeout` remains a hard bound so a genuinely lost completion fails the
/// test rather than hanging it.
fn expect_purge_completions(
    registry: &Registry,
    rx: &mpsc::Receiver<RegistryMsg>,
    expected: usize,
    timeout: Duration,
) {
    let deadline = Instant::now() + timeout;
    let mut applied = 0usize;
    let mut other_messages = 0usize;
    while applied < expected {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            panic!(
                "timed out after {timeout:?} waiting for purge completions: \
                 applied {applied} of {expected} dispatched \
                 ({other_messages} non-completion message(s) seen). \
                 A missing completion means a purge worker never reported back."
            );
        }
        match rx.recv_timeout(remaining) {
            Ok(RegistryMsg::PurgeCompletion(c)) => {
                apply_purge_completion(registry, &mut SpareReasons::new(), c);
                applied += 1;
            }
            Ok(_) => other_messages += 1,
            Err(err) => panic!(
                "purge completion channel closed after {applied} of {expected}: {err} \
                 ({other_messages} non-completion message(s) seen)"
            ),
        }
    }
}

/// Drain `RegistryMsg::PurgeCompletion(..)` items from `rx` until
/// either no completion arrives within `quiet_for` or `timeout`
/// elapses, applying each one against `registry`. Used by tests
/// that drive the periodic-tick helpers directly — outside the
/// worker loop the test plays the role of the worker.
///
/// Only appropriate where the expected count is zero or genuinely unknown;
/// see [`expect_purge_completions`].
fn drain_purge_completions(
    registry: &Registry,
    rx: &mpsc::Receiver<RegistryMsg>,
    quiet_for: Duration,
    timeout: Duration,
) -> usize {
    let deadline = Instant::now() + timeout;
    let mut drained = 0usize;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return drained;
        }
        let wait = quiet_for.min(remaining);
        match rx.recv_timeout(wait) {
            Ok(RegistryMsg::PurgeCompletion(c)) => {
                apply_purge_completion(registry, &mut SpareReasons::new(), c);
                drained += 1;
            }
            Ok(RegistryMsg::Op(_)) => {
                // Tests don't drive ops through this channel; ignore.
            }
            Ok(RegistryMsg::WatchRescan(_)) => {
                // Watch notifications are irrelevant to periodic-purge tests.
            }
            Ok(
                RegistryMsg::ExternVerdicts(_)
                | RegistryMsg::RepoWorktreeVerdicts(_)
                | RegistryMsg::RepoWorktreeReclaimed { .. }
                | RegistryMsg::RepoHints(_),
            ) => {
                // Issue #946: the off-worker probe publishes here. These
                // tests drive the tick directly and assert on the registry,
                // not on cached verdicts, so the snapshot is ignored.
            }
            Err(_) => return drained,
        }
    }
}

/// Block until the off-worker extern probe publishes its snapshot, then
/// apply it exactly as the worker loop would. Issue #946: the probe runs on
/// its own thread, so a test that wants a verdict must wait for it rather
/// than assume the tick computed one inline.
fn apply_extern_verdicts(rx: &mpsc::Receiver<RegistryMsg>, spare_reasons: &mut SpareReasons) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "extern probe never published verdicts"
        );
        match rx.recv_timeout(remaining) {
            Ok(RegistryMsg::ExternVerdicts(v)) => {
                spare_reasons.replace_extern(v, now_unix());
                return;
            }
            Ok(_) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => {
                panic!("extern probe did not publish verdicts within the deadline")
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                panic!("extern probe channel closed before publishing")
            }
        }
    }
}

/// Issue #594: a worker that never answers must produce a panic that says so,
/// naming the elapsed time and the number of slow round-trips.
///
/// The receiver is held but never replied to, which is the wedged case. Before
/// this change the failure surfaced as a bare `RecvTimeoutError` unwrap inside
/// `call`, with no indication of which wait had expired or for how long — the
/// reason characterising the real occurrence needed a log dig.
#[test]
#[should_panic(expected = "worker never answered a List")]
fn a_wedged_worker_produces_a_diagnosable_timeout() {
    let (tx, _held_receiver) = mpsc::channel::<RegistryMsg>();
    // Short deadline: the point is the message, not the waiting.
    let _ = wait_for_row_count(&tx, None, 0, Duration::from_millis(50));
}

/// The complementary case, and the actual regression guard: a worker that is
/// merely *slow* must not abort the test. The first round-trip outlives its
/// per-call budget, the helper retries within its own deadline, and the answer
/// still arrives.
///
/// The budget is passed explicitly and is deliberately shorter than the stall.
/// With the production 5 s budget a 120 ms stall never expires, so the test
/// would pass against the old panicking code too and guard nothing — which is
/// the exact failure mode this change is about. Verified by re-introducing the
/// panic locally: this test fails, the wedged-worker one still passes.
#[test]
fn a_slow_reply_is_absorbed_by_the_callers_deadline() {
    let (tx, rx) = mpsc::channel::<RegistryMsg>();
    let worker = thread::spawn(move || {
        // Stall past the per-call budget, the way a real worker does while
        // grinding through a bulk purge. The caller abandons this first reply,
        // exactly as in production.
        let first = rx.recv().expect("a request");
        thread::sleep(Duration::from_millis(120));
        if let RegistryMsg::Op(req) = first {
            let _ = req.reply_tx.send(GcReply::ListOk { rows: Vec::new() });
        }
        // Answer every retry promptly.
        while let Ok(msg) = rx.recv() {
            if let RegistryMsg::Op(req) = msg {
                let _ = req.reply_tx.send(GcReply::ListOk { rows: Vec::new() });
            }
        }
    });

    let rows = wait_for_row_count_with_budget(
        &tx,
        None,
        0,
        Duration::from_secs(5),
        Duration::from_millis(30),
    );
    assert!(rows.is_empty());
    drop(tx);
    let _ = worker.join();
}

#[test]
fn gc_tick_cadence_config_handles_default_disable_and_positive() {
    assert_eq!(
        gc_tick_cadence_from_raw(None),
        Some(Duration::from_secs(DEFAULT_GC_TICK_SECS))
    );
    assert_eq!(gc_tick_cadence_from_raw(Some("0")), None);
    assert_eq!(
        gc_tick_cadence_from_raw(Some("1")),
        Some(Duration::from_secs(1))
    );
}

#[test]
fn gc_disk_watchdog_config_parses_defaults_and_overrides() {
    let defaults = gc_disk_watchdog_config_from_raw(None, None, None, None);
    assert_eq!(defaults.warn_free_bytes, 10 * BYTES_PER_GB);
    assert_eq!(defaults.auto_purge_free_bytes, 5 * BYTES_PER_GB);
    assert_eq!(defaults.min_age, Duration::from_secs(24 * 60 * 60));
    assert!(defaults.auto_purge_enabled);

    let overrides =
        gc_disk_watchdog_config_from_raw(Some("1.5"), Some("2"), Some("7"), Some("off"));
    assert_eq!(overrides.warn_free_bytes, BYTES_PER_GB + BYTES_PER_GB / 2);
    assert_eq!(overrides.auto_purge_free_bytes, 2 * BYTES_PER_GB);
    assert_eq!(overrides.min_age, Duration::from_secs(7 * 60 * 60));
    assert!(!overrides.auto_purge_enabled);
}

#[test]
fn gc_disk_watchdog_config_falls_back_on_invalid_values() {
    let config =
        gc_disk_watchdog_config_from_raw(Some("-1"), Some("nan"), Some("bad"), Some("maybe"));
    assert_eq!(config.warn_free_bytes, 10 * BYTES_PER_GB);
    assert_eq!(config.auto_purge_free_bytes, 5 * BYTES_PER_GB);
    assert_eq!(config.min_age, Duration::from_secs(24 * 60 * 60));
    assert!(config.auto_purge_enabled);
}

#[test]
fn disk_watchdog_decision_warns_and_purges_only_below_thresholds() {
    let config = GcDiskWatchdogConfig {
        warn_free_bytes: 10 * BYTES_PER_GB,
        auto_purge_free_bytes: 5 * BYTES_PER_GB,
        min_age: Duration::from_secs(24 * 60 * 60),
        auto_purge_enabled: true,
    };

    assert_eq!(
        disk_watchdog_decision(&config, 10 * BYTES_PER_GB),
        DiskWatchdogDecision {
            warn: false,
            auto_purge: false
        }
    );
    assert_eq!(
        disk_watchdog_decision(&config, 9 * BYTES_PER_GB),
        DiskWatchdogDecision {
            warn: true,
            auto_purge: false
        }
    );
    assert_eq!(
        disk_watchdog_decision(&config, 4 * BYTES_PER_GB),
        DiskWatchdogDecision {
            warn: true,
            auto_purge: true
        }
    );

    let disabled = GcDiskWatchdogConfig {
        auto_purge_enabled: false,
        ..config
    };
    assert_eq!(
        disk_watchdog_decision(&disabled, 4 * BYTES_PER_GB),
        DiskWatchdogDecision {
            warn: true,
            auto_purge: false
        }
    );
}

#[test]
fn round_trip_insert_then_list() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.redb");
    let (tx, _g) = spawn_test_worker(&db_path);

    let resp = call(
        &tx,
        GcOp::Insert {
            kind: "worktree".to_string(),
            path: "/tmp/test-a".to_string(),
            repo_root: Some("/tmp/repo".to_string()),
            branch: Some("main".to_string()),
            agent_id: Some("agent-abc".to_string()),
            created_unix: Some(100),
        },
    );
    assert!(matches!(resp, GcReply::InsertOk { inserted: true }));

    let resp = call(&tx, GcOp::List { kind: None });
    match resp {
        GcReply::ListOk { rows } => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].path, "/tmp/test-a");
            assert_eq!(rows[0].agent_id.as_deref(), Some("agent-abc"));
        }
        other => panic!("unexpected reply: {other:?}"),
    }
}

#[test]
fn purge_with_no_duration_removes_all_non_live() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("purge-all.redb");
    let (tx, _g) = spawn_test_worker(&db_path);

    for path in ["/tmp/c1", "/tmp/c2"] {
        call(
            &tx,
            GcOp::Insert {
                kind: "cache".to_string(),
                path: path.to_string(),
                repo_root: None,
                branch: None,
                agent_id: None,
                created_unix: Some(100),
            },
        );
    }

    let resp = call(
        &tx,
        GcOp::Purge {
            duration: None,
            kind: None,
            dry_run: false,
        },
    );
    match resp {
        GcReply::PurgeStarted {
            dispatched,
            skipped,
        } => {
            assert_eq!(dispatched, 2);
            assert_eq!(skipped, 0);
        }
        other => panic!("unexpected reply: {other:?}"),
    }

    // Issue #268: bulk purge dispatches to the pool and returns
    // immediately. Wait for the pool to finish + the worker to
    // apply the completions before asserting the registry is
    // empty.
    let rows = wait_for_row_count(&tx, None, 0, Duration::from_secs(5));
    assert!(rows.is_empty(), "expected registry to drain, got {rows:?}");
}

#[test]
fn purge_dry_run_does_not_modify_db() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("purge-dry.redb");
    let (tx, _g) = spawn_test_worker(&db_path);

    call(
        &tx,
        GcOp::Insert {
            kind: "cache".to_string(),
            path: "/tmp/dry".to_string(),
            repo_root: None,
            branch: None,
            agent_id: None,
            created_unix: Some(100),
        },
    );
    let resp = call(
        &tx,
        GcOp::Purge {
            duration: None,
            kind: None,
            dry_run: true,
        },
    );
    match resp {
        GcReply::PurgeOk { removed, .. } => assert_eq!(removed, 1),
        other => panic!("unexpected reply: {other:?}"),
    }
    let resp = call(&tx, GcOp::List { kind: None });
    match resp {
        GcReply::ListOk { rows } => assert_eq!(rows.len(), 1),
        other => panic!("unexpected reply: {other:?}"),
    }
}

#[test]
fn purge_skips_entry_equal_to_live_session_cwd() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("live-cwd-direct.redb");
    let path_a = dir.path().join("A");
    let path_b = dir.path().join("B");
    std::fs::create_dir_all(&path_a).unwrap();
    std::fs::create_dir_all(&path_b).unwrap();
    let tx = spawn_test_worker_with_live_cwds(&db_path, vec![path_a.clone()]);

    for path in [&path_a, &path_b] {
        call(
            &tx,
            GcOp::Insert {
                kind: "cache".to_string(),
                path: path.to_string_lossy().to_string(),
                repo_root: None,
                branch: None,
                agent_id: None,
                created_unix: Some(100),
            },
        );
    }

    let resp = call(
        &tx,
        GcOp::Purge {
            duration: None,
            kind: None,
            dry_run: false,
        },
    );
    match resp {
        GcReply::PurgeStarted {
            dispatched,
            skipped,
        } => {
            assert_eq!(dispatched, 1);
            assert_eq!(skipped, 1);
        }
        other => panic!("unexpected reply: {other:?}"),
    }

    // Wait for the async delete of path_b to land in redb.
    let rows = wait_for_row_count(&tx, None, 1, Duration::from_secs(5));
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].path, path_a.to_string_lossy().to_string());
    assert!(path_a.exists(), "live cwd entry should remain on disk");
    assert!(!path_b.exists(), "non-live entry should be deleted");
}

#[test]
fn purge_skips_entry_that_is_ancestor_of_live_session_cwd() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("live-cwd-ancestor.redb");
    let path_a = dir.path().join("A");
    let live_subdir = path_a.join("sub");
    std::fs::create_dir_all(&live_subdir).unwrap();
    let tx = spawn_test_worker_with_live_cwds(&db_path, vec![live_subdir]);

    call(
        &tx,
        GcOp::Insert {
            kind: "cache".to_string(),
            path: path_a.to_string_lossy().to_string(),
            repo_root: None,
            branch: None,
            agent_id: None,
            created_unix: Some(100),
        },
    );

    let resp = call(
        &tx,
        GcOp::Purge {
            duration: None,
            kind: None,
            dry_run: false,
        },
    );
    match resp {
        GcReply::PurgeStarted {
            dispatched,
            skipped,
        } => {
            assert_eq!(dispatched, 0);
            assert_eq!(skipped, 1);
        }
        other => panic!("unexpected reply: {other:?}"),
    }

    assert!(
        path_a.exists(),
        "ancestor of live cwd should remain on disk"
    );
    let rows = match call(&tx, GcOp::List { kind: None }) {
        GcReply::ListOk { rows } => rows,
        other => panic!("unexpected reply: {other:?}"),
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].path, path_a.to_string_lossy().to_string());
}

#[test]
fn periodic_tick_spares_unverified_source_when_free_space_low() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("periodic-purge.redb");
    let registry = Registry::open_at(&db_path).unwrap();
    let old_path = dir.path().join("old-worktree");
    let old_sibling = dir.path().join("clud-pr-old");
    std::fs::create_dir_all(&old_path).unwrap();
    std::fs::create_dir_all(&old_sibling).unwrap();

    registry
        .insert_if_new(&InsertInput {
            kind: "worktree".to_string(),
            path: old_path.to_string_lossy().to_string(),
            repo_root: Some(dir.path().to_string_lossy().to_string()),
            branch: Some("stale".to_string()),
            agent_id: Some("agent-old".to_string()),
            now_unix: now_unix().saturating_sub(25 * 60 * 60),
        })
        .unwrap();
    registry
        .insert_if_new(&InsertInput {
            kind: SIBLING_CLONE_KIND.to_string(),
            path: old_sibling.to_string_lossy().to_string(),
            repo_root: Some(dir.path().to_string_lossy().to_string()),
            branch: Some("old".to_string()),
            agent_id: None,
            now_unix: now_unix().saturating_sub(25 * 60 * 60),
        })
        .unwrap();

    let config = GcDiskWatchdogConfig {
        warn_free_bytes: 10 * BYTES_PER_GB,
        auto_purge_free_bytes: 5 * BYTES_PER_GB,
        min_age: Duration::from_secs(24 * 60 * 60),
        auto_purge_enabled: true,
    };
    let live_cwds_provider: LiveCwdsProvider = Arc::new(Vec::<PathBuf>::new);
    let pool_tx = spawn_purge_pool(2);
    let (completion_tx, completion_rx) = mpsc::channel::<RegistryMsg>();
    let dispatched = run_periodic_purge_tick_with_free_space(
        &registry,
        &pool_tx,
        &completion_tx,
        &live_cwds_provider,
        &config,
        &|_| Ok(4 * BYTES_PER_GB),
        PeriodicPurgeContext {
            session_state_dir: None,
            activity: None,
            spare_reasons: &mut SpareReasons::new(),
            repo_worktrees: RepoWorktreeGcConfig {
                mode: ReclaimMode::Off,
                delete_remote: false,
                wt_root: None,
            },
        },
    );
    // Neither source has a cached Git verdict. Disk pressure cannot turn
    // unknown source into a verified deletion candidate.
    assert_eq!(dispatched, 0, "unverified source must not be dispatched");

    // Outside the worker loop the test plays the role of the
    // registry-writer thread: apply the pool's completion callbacks to redb
    // directly. Issues #383/#560: this used to stop after a quiet gap
    // (250ms, then 1500ms), which is a guess about scheduler, AV-scanner and
    // TempDir latency rather than a fact about the purge. Wait for the count
    // the tick actually reported instead.
    expect_purge_completions(
        &registry,
        &completion_rx,
        dispatched,
        Duration::from_secs(30),
    );

    assert_eq!(registry.list(Some(WORKTREE_KIND)).unwrap().len(), 1);
    assert_eq!(registry.list(Some(SIBLING_CLONE_KIND)).unwrap().len(), 1);
    assert!(
        old_path.exists(),
        "unverified worktree must survive disk pressure"
    );
    assert!(
        old_sibling.exists(),
        "unverified nested contents must survive"
    );
}

#[test]
fn periodic_tick_keeps_old_worktree_entry_when_free_space_is_healthy() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("periodic-healthy.redb");
    let registry = Registry::open_at(&db_path).unwrap();
    let old_path = dir.path().join("old-worktree");
    std::fs::create_dir_all(&old_path).unwrap();

    registry
        .insert_if_new(&InsertInput {
            kind: "worktree".to_string(),
            path: old_path.to_string_lossy().to_string(),
            repo_root: Some(dir.path().to_string_lossy().to_string()),
            branch: Some("stale".to_string()),
            agent_id: Some("agent-old".to_string()),
            now_unix: now_unix().saturating_sub(25 * 60 * 60),
        })
        .unwrap();

    let config = GcDiskWatchdogConfig {
        warn_free_bytes: 10 * BYTES_PER_GB,
        auto_purge_free_bytes: 5 * BYTES_PER_GB,
        min_age: Duration::from_secs(24 * 60 * 60),
        auto_purge_enabled: true,
    };
    let live_cwds_provider: LiveCwdsProvider = Arc::new(Vec::<PathBuf>::new);
    let pool_tx = spawn_purge_pool(1);
    let (completion_tx, completion_rx) = mpsc::channel::<RegistryMsg>();
    run_periodic_purge_tick_with_free_space(
        &registry,
        &pool_tx,
        &completion_tx,
        &live_cwds_provider,
        &config,
        &|_| Ok(20 * BYTES_PER_GB),
        PeriodicPurgeContext {
            session_state_dir: None,
            activity: None,
            spare_reasons: &mut SpareReasons::new(),
            repo_worktrees: RepoWorktreeGcConfig {
                mode: ReclaimMode::Off,
                delete_remote: false,
                wt_root: None,
            },
        },
    );
    // Healthy disk → no dispatches expected, so no completions
    // should land.
    let drained = drain_purge_completions(
        &registry,
        &completion_rx,
        Duration::from_millis(150),
        Duration::from_millis(500),
    );
    assert_eq!(drained, 0);

    assert_eq!(registry.list(Some(WORKTREE_KIND)).unwrap().len(), 1);
    assert!(old_path.exists());
}

#[test]
fn trash_reaper_deletes_successful_entry_and_row() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("trash-reap.redb");
    let registry = Registry::open_at(&db_path).unwrap();
    let trash_dir = dir.path().join("trash-item");
    std::fs::create_dir_all(&trash_dir).unwrap();
    registry
        .insert_if_new(&InsertInput {
            kind: "trash".to_string(),
            path: trash_dir.to_string_lossy().to_string(),
            repo_root: None,
            branch: None,
            agent_id: Some("C:/repo/target/debug/foo.dll".to_string()),
            now_unix: 100,
        })
        .unwrap();

    let (removed, failed) = reap_trash_entries(&registry).unwrap();

    assert_eq!((removed, failed), (1, 0));
    assert!(!trash_dir.exists());
    assert!(registry.list(Some("trash")).unwrap().is_empty());
}

#[test]
fn rm_tool_trash_is_kept_for_its_window_then_reaped() {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::open_at(&dir.path().join("rm-trash.redb")).unwrap();
    let entry = dir.path().join("20260101T000000Z-abcdef-build");
    std::fs::create_dir_all(&entry).unwrap();
    std::fs::write(entry.join(crate::rm_tool::TRASH_MANIFEST), b"{}").unwrap();
    registry
        .insert_if_new(&InsertInput {
            kind: "trash".to_string(),
            path: entry.to_string_lossy().to_string(),
            repo_root: None,
            branch: None,
            agent_id: Some("/repo/build".to_string()),
            now_unix: 100,
        })
        .unwrap();
    let now = std::time::SystemTime::now();

    // Inside the keep window the entry is recoverable, so the tick spares it.
    assert_eq!(
        filesystem::reap_trash_entries_at(&registry, now).unwrap(),
        (0, 0)
    );
    assert!(entry.exists());

    let later = now + crate::rm_tool::TRASH_KEEP + std::time::Duration::from_secs(60);
    assert_eq!(
        filesystem::reap_trash_entries_at(&registry, later).unwrap(),
        (1, 0)
    );
    assert!(!entry.exists());
    assert!(registry.list(Some("trash")).unwrap().is_empty());
}

#[test]
fn unregistered_rm_tool_trash_is_swept_only_once_expired() {
    let dir = tempfile::tempdir().unwrap();
    let entry = dir.path().join("20260101T000000Z-abcdef-notes.txt");
    std::fs::create_dir_all(&entry).unwrap();
    std::fs::write(entry.join(crate::rm_tool::TRASH_MANIFEST), b"{}").unwrap();
    let quarantine = dir.path().join("20260101T000000Z-123456");
    std::fs::create_dir_all(&quarantine).unwrap();
    let now = std::time::SystemTime::now();
    assert_eq!(reap_unregistered_rm_trash(dir.path(), now), 0);
    let later = now + crate::rm_tool::TRASH_KEEP + std::time::Duration::from_secs(60);
    assert_eq!(reap_unregistered_rm_trash(dir.path(), later), 1);
    assert!(!entry.exists());
    assert!(
        quarantine.exists(),
        "`clud trash` quarantine is the registry's job"
    );
}

#[test]
fn trash_reaper_keeps_row_when_delete_fails() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("trash-reap-fail.redb");
    let registry = Registry::open_at(&db_path).unwrap();
    let not_a_dir = dir.path().join("still-locked.dll");
    std::fs::write(&not_a_dir, b"locked").unwrap();
    registry
        .insert_if_new(&InsertInput {
            kind: "trash".to_string(),
            path: not_a_dir.to_string_lossy().to_string(),
            repo_root: None,
            branch: None,
            agent_id: Some("C:/repo/target/debug/still-locked.dll".to_string()),
            now_unix: 100,
        })
        .unwrap();

    let (removed, failed) = reap_trash_entries(&registry).unwrap();

    assert_eq!((removed, failed), (0, 1));
    assert!(not_a_dir.exists());
    assert_eq!(registry.list(Some("trash")).unwrap().len(), 1);
}

#[test]
fn periodic_tick_removes_stale_extern_repo_entry() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("extern-purge.redb");
    let repo = dir.path().join("extern");
    fs::create_dir_all(&repo).unwrap();
    // Real, fully pushed source: the positive reclaim case must remain
    // possible after unknown container directories become pinned (#1790).
    let git = |cwd: &Path, args: &[&str]| crate::worktrees::run_git(cwd, args).unwrap();
    git(dir.path(), &["init", "--bare", "upstream.git"]);
    git(&repo, &["init", "--initial-branch=main"]);
    git(&repo, &["config", "user.email", "test@example.com"]);
    git(&repo, &["config", "user.name", "test"]);
    git(&repo, &["config", "commit.gpgsign", "false"]);
    git(&repo, &["config", "core.hooksPath", ""]);
    fs::write(repo.join("source.txt"), "published source").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "published"]);
    git(&repo, &["remote", "add", "origin", "../upstream.git"]);
    git(&repo, &["push", "-u", "origin", "main"]);

    let _age = ScopedEnv::set(ENV_GC_EXTERN_REPO_MAX_AGE_SECS, "0");

    let registry = Registry::open_at(&db_path).expect("open registry");
    registry
        .insert_if_new(&InsertInput {
            kind: EXTERN_REPO_KIND.to_string(),
            path: repo.to_string_lossy().to_string(),
            repo_root: Some(dir.path().to_string_lossy().to_string()),
            branch: None,
            agent_id: None,
            now_unix: now_unix(),
        })
        .expect("insert extern repo");

    let live_cwds_provider: LiveCwdsProvider = Arc::new(Vec::<PathBuf>::new);
    let pool_tx = spawn_purge_pool(1);
    let (completion_tx, completion_rx) = mpsc::channel::<RegistryMsg>();
    let mut spare_reasons = SpareReasons::new();

    // Issue #946: the probe now runs off-worker, so the first tick has no
    // verdict for this row and MUST spare it. Purging on the mtime gate
    // alone would delete a checkout nothing had inspected — exactly the
    // data loss the extern-repo guard exists to prevent.
    let first = run_periodic_purge_tick(
        &registry,
        &pool_tx,
        &completion_tx,
        &live_cwds_provider,
        None,
        None,
        &mut spare_reasons,
    );
    assert_eq!(
        first, 0,
        "an extern repo with no verdict yet must be spared, not purged"
    );
    assert!(repo.exists(), "the first tick must not have deleted it");

    // Apply the verdict the probe thread published, then tick again.
    apply_extern_verdicts(&completion_rx, &mut spare_reasons);

    let dispatched = run_periodic_purge_tick(
        &registry,
        &pool_tx,
        &completion_tx,
        &live_cwds_provider,
        None,
        None,
        &mut spare_reasons,
    );
    // Same #383/#560 hazard as the periodic-purge test: this one discarded
    // the drain count entirely, so a completion that never arrived showed up
    // only as the "dir should be deleted" assertion failing — with no hint
    // that the purge simply hadn't finished yet.
    assert_eq!(dispatched, 1, "the stale extern repo should be dispatched");
    expect_purge_completions(
        &registry,
        &completion_rx,
        dispatched,
        Duration::from_secs(30),
    );

    let rows = registry.list(Some(EXTERN_REPO_KIND)).expect("list");
    assert!(rows.is_empty(), "stale extern-repo row should be deleted");
    assert!(!repo.exists(), "stale extern-repo dir should be deleted");
}

#[test]
fn periodic_tick_keeps_fresh_extern_repo_entry() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("extern-keep.redb");
    let repo = dir.path().join("extern");
    fs::create_dir_all(&repo).unwrap();

    // 1h stale-after, but the dir was just created (mtime ~ now) → keep.
    let _age = ScopedEnv::set(ENV_GC_EXTERN_REPO_MAX_AGE_SECS, "3600");

    let registry = Registry::open_at(&db_path).expect("open registry");
    registry
        .insert_if_new(&InsertInput {
            kind: EXTERN_REPO_KIND.to_string(),
            path: repo.to_string_lossy().to_string(),
            repo_root: Some(dir.path().to_string_lossy().to_string()),
            branch: None,
            agent_id: None,
            now_unix: now_unix(),
        })
        .expect("insert extern repo");

    let live_cwds_provider: LiveCwdsProvider = Arc::new(Vec::<PathBuf>::new);
    let pool_tx = spawn_purge_pool(1);
    let (completion_tx, completion_rx) = mpsc::channel::<RegistryMsg>();
    run_periodic_purge_tick(
        &registry,
        &pool_tx,
        &completion_tx,
        &live_cwds_provider,
        None,
        None,
        &mut SpareReasons::new(),
    );
    let drained = drain_purge_completions(
        &registry,
        &completion_rx,
        Duration::from_millis(150),
        Duration::from_millis(500),
    );
    assert_eq!(drained, 0, "fresh extern-repo must not be dispatched");

    let rows = registry.list(Some(EXTERN_REPO_KIND)).expect("list");
    assert_eq!(rows.len(), 1, "fresh extern-repo row should survive");
    assert!(repo.exists(), "fresh extern-repo dir should survive");
}

/// Issue #896, and the load-bearing test for its design: `gc list` must
/// explain a retained row *without* re-running the git probe, because it is
/// a hot client op on the registry worker thread (see #946).
///
/// The proof is the second assertion: the very same registry row reads as a
/// plain `reclaimable` row when handed an empty cache. If `List` probed git
/// itself it would reach the same `pinned` verdict both times, and that
/// assertion would fail.
#[test]
fn list_surfaces_the_ticks_reason_from_cache_and_never_probes_itself() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("list-state.redb");
    let repo = dir.path().join("extern-pinned");
    // A `.git` that is not a usable repository: the anchor gate passes, then
    // every probe query fails, which is `ProbeFailed` → spared. That is a
    // real pinned state and needs no git fixture to reproduce.
    fs::create_dir_all(repo.join(".git")).unwrap();

    // Idle immediately, so the tick evaluates rather than skipping on mtime.
    let _age = ScopedEnv::set(ENV_GC_EXTERN_REPO_MAX_AGE_SECS, "0");

    let registry = Registry::open_at(&db_path).expect("open registry");
    registry
        .insert_if_new(&InsertInput {
            kind: EXTERN_REPO_KIND.to_string(),
            path: repo.to_string_lossy().to_string(),
            repo_root: Some(dir.path().to_string_lossy().to_string()),
            branch: None,
            agent_id: None,
            now_unix: now_unix(),
        })
        .expect("insert extern repo");

    let live_cwds_provider: LiveCwdsProvider = Arc::new(Vec::<PathBuf>::new);
    let pool_tx = spawn_purge_pool(1);
    let (completion_tx, completion_rx) = mpsc::channel::<RegistryMsg>();
    let mut spare_reasons = SpareReasons::new();

    // Drive one tick to kick off the off-worker probe, then apply the
    // snapshot it publishes — the same sequence the worker loop performs.
    run_periodic_purge_tick(
        &registry,
        &pool_tx,
        &completion_tx,
        &live_cwds_provider,
        None,
        None,
        &mut spare_reasons,
    );
    apply_extern_verdicts(&completion_rx, &mut spare_reasons);
    assert!(
        !spare_reasons.is_empty(),
        "the probe must have published a verdict for gc list to reuse"
    );

    let rows = list_rows(&registry, &pool_tx, &completion_tx, &mut spare_reasons);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].state, "pinned");
    assert!(!rows[0].reclaimable);
    assert_eq!(
        rows[0].reason.as_deref(),
        Some("pinned: git state unreadable")
    );
    assert!(
        rows[0].evaluated_unix.is_some(),
        "a cached verdict should carry when it was computed"
    );

    // Without a cached verdict, source verification is pending. The absence
    // of an evaluated timestamp proves `List` did not probe git itself.
    let uncached = list_rows(
        &registry,
        &pool_tx,
        &completion_tx,
        &mut SpareReasons::new(),
    );
    assert_eq!(uncached[0].state, "pinned");
    assert!(!uncached[0].reclaimable);
    assert_eq!(
        uncached[0].reason.as_deref(),
        Some("pinned: source verification pending")
    );
    assert_eq!(uncached[0].evaluated_unix, None);
}

/// Issue #946's acceptance criterion, asserted structurally rather than by
/// timing: nothing on the registry worker thread computes an extern-repo
/// verdict. A worker-side purge over an extern-repo row must leave the cache
/// **untouched** — if any code path still probed inline, it would have a
/// verdict to record and this would fail. It must also spare the row, since
/// purging without a verdict is the data-loss case.
#[test]
fn worker_side_ops_never_compute_an_extern_verdict() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("no-inline-probe.redb");
    let repo = dir.path().join("extern");
    fs::create_dir_all(&repo).unwrap();

    // Idle immediately: under the old inline design this row would have been
    // evaluated (and, being a non-git dir, purged on the mtime fallback).
    let _age = ScopedEnv::set(ENV_GC_EXTERN_REPO_MAX_AGE_SECS, "0");

    let registry = Registry::open_at(&db_path).expect("open registry");
    registry
        .insert_if_new(&InsertInput {
            kind: EXTERN_REPO_KIND.to_string(),
            path: repo.to_string_lossy().to_string(),
            repo_root: Some(dir.path().to_string_lossy().to_string()),
            branch: None,
            agent_id: None,
            now_unix: now_unix(),
        })
        .expect("insert extern repo");

    let pool_tx = spawn_purge_pool(1);
    let (completion_tx, _completion_rx) = mpsc::channel::<RegistryMsg>();
    let mut spare_reasons = SpareReasons::new();

    let reply = process_op(
        &registry,
        &pool_tx,
        &completion_tx,
        GcOp::Purge {
            duration: None,
            kind: Some(EXTERN_REPO_KIND.to_string()),
            dry_run: true,
        },
        Vec::new(),
        &mut spare_reasons,
    );
    match reply {
        GcReply::PurgeOk { removed, skipped } => {
            assert_eq!(removed, 0, "no verdict yet → must spare, never purge");
            assert_eq!(skipped, 1);
        }
        other => panic!("expected PurgeOk, got {other:?}"),
    }
    assert!(
        spare_reasons.is_empty(),
        "the worker must not have computed a verdict — that is the probe's job"
    );

    // And `List` likewise leaves it empty.
    let rows = list_rows(&registry, &pool_tx, &completion_tx, &mut spare_reasons);
    assert_eq!(rows.len(), 1);
    assert!(
        spare_reasons.is_empty(),
        "gc list must not compute verdicts either"
    );
}

/// Issue #946's data-loss guard. Decoupling the verdict from the delete put
/// up to a tick between them, so a verdict saying "clean and pushed" can be
/// stale by the time it authorizes `remove_dir_all` — a developer who came
/// back and edited in that window would lose the work. `reverify_before_delete`
/// re-checks on the purge-pool thread, immediately before deleting.
#[test]
fn a_stale_reclaimable_verdict_does_not_authorize_a_delete() {
    // ENV_GC_EXTERN_REPO_MAX_AGE_SECS is process-global and other tests set it
    // (to 0); without the lock a parallel test makes the pool skip the veto.
    let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("extern");
    fs::create_dir_all(&repo).unwrap();

    let entry = TrackedEntry {
        id: 1,
        kind: EXTERN_REPO_KIND.to_string(),
        path: repo.to_string_lossy().to_string(),
        repo_root: None,
        branch: None,
        agent_id: None,
        created_unix: 0,
    };

    // Idle window of a day: the directory was just created, so it is NOT
    // idle now — exactly the state a developer returning to the checkout
    // produces after a verdict was taken while it was quiet.
    let _age = ScopedEnv::set(ENV_GC_EXTERN_REPO_MAX_AGE_SECS, "86400");
    assert_eq!(
        reverify_before_delete(&entry),
        Some("spared: recently active"),
        "a freshly-touched checkout must be spared at delete time"
    );
    assert!(repo.exists());

    // #1790: a non-checkout remains unverified even with a zero idle window;
    // its nested contents must never be deleted on mtime alone.
    let _age0 = ScopedEnv::set(ENV_GC_EXTERN_REPO_MAX_AGE_SECS, "0");
    assert_eq!(
        reverify_before_delete(&entry),
        Some("pinned: not a verified git checkout")
    );
}

/// End-to-end guard for the same hazard: the *pool* must apply the re-check,
/// not merely have a function that could. A cached `Reclaimable` verdict is
/// dispatched for a directory that is no longer idle; the entry must survive
/// and come back marked spared.
#[test]
fn the_purge_pool_refuses_a_delete_the_recheck_vetoes() {
    // ENV_GC_EXTERN_REPO_MAX_AGE_SECS is process-global and other tests set it
    // (to 0); without the lock a parallel test makes the pool skip the veto.
    let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("extern-live-again");
    fs::create_dir_all(&repo).unwrap();
    let path = repo.to_string_lossy().to_string();

    // Freshly created → not idle under a one-day window.
    let _age = ScopedEnv::set(ENV_GC_EXTERN_REPO_MAX_AGE_SECS, "86400");

    let entry = TrackedEntry {
        id: 7,
        kind: EXTERN_REPO_KIND.to_string(),
        path: path.clone(),
        repo_root: None,
        branch: None,
        agent_id: None,
        created_unix: 0,
    };

    // The cache still says it is reclaimable — a verdict taken while the
    // checkout was quiet, now stale.
    let mut spare_reasons = SpareReasons::new();
    spare_reasons.replace_extern(
        vec![(
            path.clone(),
            PurgeDecision {
                purge: true,
                reason: "reclaimable: clean and pushed",
                class: PurgeClass::Reclaimable,
            },
        )],
        now_unix(),
    );

    let pool_tx = spawn_purge_pool(1);
    let (completion_tx, completion_rx) = mpsc::channel::<RegistryMsg>();
    let reply = dispatch_purge_entries(
        &pool_tx,
        &completion_tx,
        vec![entry],
        Vec::new(),
        &mut spare_reasons,
    );
    assert!(
        matches!(reply, GcReply::PurgeStarted { dispatched: 1, .. }),
        "the stale verdict should get it dispatched: {reply:?}"
    );

    // The pool re-checks and vetoes.
    let msg = completion_rx
        .recv_timeout(Duration::from_secs(30))
        .expect("pool should report a completion");
    match msg {
        RegistryMsg::PurgeCompletion(c) => {
            assert_eq!(
                c.spared,
                Some("spared: recently active"),
                "the pool must veto the delete, not perform it"
            );
        }
        _ => panic!("expected PurgeCompletion from the pool"),
    }
    assert!(repo.exists(), "the checkout must still be on disk");
}

/// Other kinds have no probe verdict and are unaffected by the re-check.
#[test]
fn tracked_worktree_reverification_checks_source_references() {
    let tmp = tempfile::tempdir().unwrap();
    let entry = TrackedEntry {
        id: 1,
        kind: WORKTREE_KIND.to_string(),
        path: tmp.path().to_string_lossy().into_owned(),
        repo_root: None,
        branch: None,
        agent_id: None,
        created_unix: 0,
    };
    assert_eq!(
        reverify_before_delete_with(&entry, &|_| Some(
            "pinned: container bind references source tree"
        )),
        Some("pinned: container bind references source tree")
    );
    assert_eq!(
        reverify_before_delete_with(&entry, &|_| None),
        Some("pinned: not a verified git checkout")
    );
}

#[test]
fn worktree_preview_requires_a_current_source_verdict() {
    let tmp = tempfile::tempdir().unwrap();
    let entry = TrackedEntry {
        id: 1,
        kind: WORKTREE_KIND.to_string(),
        path: tmp.path().to_string_lossy().into_owned(),
        repo_root: None,
        branch: None,
        agent_id: None,
        created_unix: 0,
    };
    let mut cache = SpareReasons::new();
    let (purgeable, skipped) = partition_purgeable(vec![entry.clone()], vec![], &mut cache);
    assert!(purgeable.is_empty(), "unprobed source is not removable");
    assert_eq!(skipped, 1);
    cache.record(
        entry.path.clone(),
        PurgeDecision {
            purge: false,
            reason: "pinned: uncommitted or unpushed work",
            class: extern_repo::PurgeClass::Pinned,
        },
        now_unix(),
    );
    let (purgeable, skipped) = partition_purgeable(vec![entry], vec![], &mut cache);
    assert!(purgeable.is_empty());
    assert_eq!(skipped, 1);
}

/// A second probe must not stack on top of one already running: it would
/// duplicate every `git` spawn for no benefit.
#[test]
fn a_second_probe_does_not_stack_on_an_in_flight_one() {
    let mut cache = SpareReasons::new();
    assert!(cache.begin_probe(), "first probe should claim the slot");
    assert!(
        !cache.begin_probe(),
        "second must be refused while in flight"
    );
    cache.probe_finished();
    assert!(cache.begin_probe(), "slot frees once the probe completes");
}

/// A registry row whose directory is gone reads as `dangling` (red) — and
/// that needs no cached verdict, since `List` can stat the path itself.
#[test]
fn list_marks_a_row_whose_path_is_gone_as_dangling() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("dangling.redb");

    let registry = Registry::open_at(&db_path).expect("open registry");
    registry
        .insert_if_new(&InsertInput {
            kind: WORKTREE_KIND.to_string(),
            path: dir
                .path()
                .join("never-existed")
                .to_string_lossy()
                .to_string(),
            repo_root: None,
            branch: None,
            agent_id: None,
            now_unix: now_unix(),
        })
        .expect("insert worktree");

    let pool_tx = spawn_purge_pool(1);
    let (completion_tx, _completion_rx) = mpsc::channel::<RegistryMsg>();
    let rows = list_rows(
        &registry,
        &pool_tx,
        &completion_tx,
        &mut SpareReasons::new(),
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].state, "dangling");
    assert!(!rows[0].reclaimable);
    assert_eq!(rows[0].reason.as_deref(), Some("dangling: path missing"));
}

fn list_rows(
    registry: &Registry,
    pool_tx: &mpsc::Sender<PurgeJob>,
    completion_tx: &mpsc::Sender<RegistryMsg>,
    spare_reasons: &mut SpareReasons,
) -> Vec<ListRow> {
    match process_op(
        registry,
        pool_tx,
        completion_tx,
        GcOp::List { kind: None },
        Vec::new(),
        spare_reasons,
    ) {
        GcReply::ListOk { rows } => rows,
        other => panic!("expected ListOk, got {other:?}"),
    }
}

#[test]
fn list_filter_by_kind() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("filter.redb");
    let (tx, _g) = spawn_test_worker(&db_path);

    call(
        &tx,
        GcOp::Insert {
            kind: "worktree".to_string(),
            path: "/tmp/wt".to_string(),
            repo_root: None,
            branch: None,
            agent_id: None,
            created_unix: Some(100),
        },
    );
    call(
        &tx,
        GcOp::Insert {
            kind: "cache".to_string(),
            path: "/tmp/ca".to_string(),
            repo_root: None,
            branch: None,
            agent_id: None,
            created_unix: Some(100),
        },
    );
    let resp = call(
        &tx,
        GcOp::List {
            kind: Some("worktree".to_string()),
        },
    );
    match resp {
        GcReply::ListOk { rows } => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].kind, "worktree");
        }
        other => panic!("unexpected reply: {other:?}"),
    }
}

#[path = "tests/delete_and_pool.rs"]
mod delete_and_pool;
#[path = "tests/repo_worktree_reclaim.rs"]
mod repo_worktree_reclaim_tick;

/// Issue #1591: `gc list` surfaces the cached repo-worktree snapshot with
/// state, reason, reclaimable and evaluated_unix; a registry-tracked path
/// keeps its own row; and a live session cwd pins a row the (up to one tick
/// old) snapshot called reclaimable.
#[test]
fn repo_worktree_rows_surface_in_gc_list_shape() {
    use super::repo_worktree::{RepoWorktreeState, RepoWorktreeVerdict};
    let tmp = tempfile::tempdir().unwrap();
    let landed = tmp.path().join("landed");
    let busy = tmp.path().join("busy");
    let tracked = tmp.path().join("tracked");
    for p in [&landed, &busy, &tracked] {
        fs::create_dir_all(p).unwrap();
    }
    let row = |p: &Path, reason: &str| RepoWorktreeRow {
        path: p.to_string_lossy().to_string(),
        repo_root: "/repo".to_string(),
        branch: Some("feat".to_string()),
        tip: None,
        mtime_unix: 5,
        verdict: RepoWorktreeVerdict {
            state: RepoWorktreeState::Reclaimable,
            reason: reason.to_string(),
        },
        reservation: false,
    };
    let mut cache = SpareReasons::new();
    assert!(cache.begin_repo_probe());
    assert!(!cache.begin_repo_probe(), "one probe at a time");
    cache.replace_repo_worktrees(
        vec![
            row(&landed, "merged via PR #1"),
            row(&busy, "merged via PR #2"),
            row(&tracked, "merged via PR #3"),
        ],
        1234,
    );
    assert!(cache.begin_repo_probe(), "a snapshot frees the slot");

    let tracked_paths: HashSet<String> = [tracked.to_string_lossy().to_string()].into();
    let live = canonicalize_live_cwds(vec![busy.clone()]);
    let rows = repo_worktree_list_rows(
        &cache,
        &tracked_paths,
        &live,
        &crate::gc::keep_list::KeepList::Paths(Default::default()),
    );
    assert_eq!(rows.len(), 2, "tracked path must not be listed twice");

    let landed_row = &rows[0];
    assert_eq!(landed_row.kind, REPO_WORKTREE_KIND);
    assert_eq!(landed_row.id, 0, "no redb row backs it");
    assert_eq!(landed_row.state, "reclaimable");
    assert!(landed_row.reclaimable);
    assert_eq!(landed_row.reason.as_deref(), Some("merged via PR #1"));
    assert_eq!(landed_row.evaluated_unix, Some(1234));

    let busy_row = &rows[1];
    assert_eq!(busy_row.state, "pinned");
    assert!(!busy_row.reclaimable);
    assert_eq!(busy_row.reason.as_deref(), Some("process inside"));
}

/// #1672: a registered trash row whose directory is already gone (evicted by
/// the size cap, or removed by hand) is done, not a failure retried forever.
#[test]
fn trash_reaper_drops_row_whose_path_is_gone() {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::open_at(&dir.path().join("trash-gone.redb")).unwrap();
    let gone = dir.path().join("20260101T000000Z-abcdef");
    registry
        .insert_if_new(&InsertInput {
            kind: "trash".to_string(),
            path: gone.to_string_lossy().to_string(),
            repo_root: None,
            branch: None,
            agent_id: None,
            now_unix: 100,
        })
        .unwrap();

    let (removed, failed) = reap_trash_entries(&registry).unwrap();

    assert_eq!((removed, failed), (1, 0));
    assert!(registry.list(Some("trash")).unwrap().is_empty());
}

/// #1672: a trash entry holding a read-only tree (sealed build output) was
/// "reaped" on every tick and never removed. The reaper now makes it
/// writable first, as safe-rm's own purge does (#1573).
#[test]
fn trash_reaper_removes_read_only_trees() {
    if !crate::platform::host::UNIX {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::open_at(&dir.path().join("trash-ro.redb")).unwrap();
    let entry = dir.path().join("20260101T000000Z-123456");
    let sealed = entry.join("target").join("debug");
    std::fs::create_dir_all(&sealed).unwrap();
    std::fs::write(sealed.join("app"), b"bin").unwrap();
    crate::platform::gc_test_fs::set_mode(&sealed, 0o555).unwrap();
    registry
        .insert_if_new(&InsertInput {
            kind: "trash".to_string(),
            path: entry.to_string_lossy().to_string(),
            repo_root: None,
            branch: None,
            agent_id: None,
            now_unix: 100,
        })
        .unwrap();

    let (removed, failed) = reap_trash_entries(&registry).unwrap();
    if entry.exists() {
        crate::platform::gc_test_fs::set_mode(&sealed, 0o755).unwrap();
    }

    assert_eq!((removed, failed), (1, 0));
    assert!(!entry.exists());
}

#[test]
fn dangling_source_rows_are_forgotten_and_present_ones_kept() {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::open_at(&dir.path().join("dangling.redb")).unwrap();
    let present = dir.path().join("repo-wt-live");
    std::fs::create_dir_all(&present).unwrap();
    let missing = dir.path().join("repo-wt-gone");
    for (kind, path) in [
        (WORKTREE_KIND, &missing),
        (WORKTREE_KIND, &present),
        (EXTERN_REPO_KIND, &missing),
        ("trash", &missing),
    ] {
        registry
            .insert_if_new(&InsertInput {
                kind: kind.to_string(),
                path: path.to_string_lossy().to_string(),
                repo_root: None,
                branch: None,
                agent_id: None,
                now_unix: 100,
            })
            .unwrap();
    }

    assert_eq!(
        filesystem::forget_dangling_source_rows(&registry).unwrap(),
        2
    );

    let worktrees = registry.list(Some(WORKTREE_KIND)).unwrap();
    assert_eq!(worktrees.len(), 1);
    assert_eq!(Path::new(&worktrees[0].path), present.as_path());
    assert!(registry.list(Some(EXTERN_REPO_KIND)).unwrap().is_empty());
    // Trash rows keep their own reaper.
    assert_eq!(registry.list(Some("trash")).unwrap().len(), 1);
    assert!(present.is_dir());
}

/// Issue #2103: a repo only the shim saw joins this scan's repo list, is
/// handed to the worker, and lands in `repo_visits` without a launch count.
#[test]
fn shim_hints_join_the_scan_and_register_the_repo() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("seen-by-shim");
    fs::create_dir_all(repo.join(".git")).unwrap();
    let repo = fs::canonicalize(repo).unwrap();
    let repo_key = repo.to_string_lossy().into_owned();
    let source = shim_hints::HintSource::under(&dir.path().join("state"));
    fs::create_dir_all(source.log.parent().unwrap()).unwrap();
    fs::write(
        &source.log,
        format!("{}\n", serde_json::json!({"cwd": repo, "argv": ["status"]})),
    )
    .unwrap();
    let (tx, rx) = mpsc::channel();
    let roots = with_shim_hints(vec!["/already/known".to_string()], Some(&source), &tx);
    assert_eq!(roots, vec!["/already/known".to_string(), repo_key.clone()]);
    let Ok(RegistryMsg::RepoHints(repos)) = rx.try_recv() else {
        panic!("the hints go to the worker");
    };
    let registry = Registry::open_at(&dir.path().join("hints.redb")).unwrap();
    record_repo_hints(&registry, &repos, 100);
    record_repo_hints(&registry, &repos, 200);
    let rows = registry.list_repo_visits().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].repo_root, repo_key);
    assert_eq!((rows[0].run_count, rows[0].last_visited_unix), (0, 100));
    assert!(
        with_shim_hints(Vec::new(), Some(&source), &tx).is_empty(),
        "a consumed line is not read twice"
    );
    assert!(rx.try_recv().is_err());
}

use super::*;
use std::collections::HashSet;
use std::sync::Mutex as StdMutex;

/// Serialize env-var manipulation across the few tests that touch
/// process-global state. Test threads otherwise stomp each other.
static ENV_LOCK: StdMutex<()> = StdMutex::new(());

/// Take [`ENV_LOCK`] even after another test panicked while holding it.
/// The lock only serializes; it guards no data a panic could leave
/// half-written, so one failure must not fail every later test (#2181).
fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Build a unique DB path inside a TempDir that's intentionally
/// leaked for the lifetime of the test process. We need the file
/// alive across reopens (e.g. `register_then_drop_round_trips`),
/// and the test process exits shortly anyway.
fn fresh_db_path(tag: &str) -> PathBuf {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join(format!("sessions-{tag}.redb"));
    std::mem::forget(dir);
    path
}

fn open_with_alive_set(path: &Path, alive: Vec<u32>) -> SessionRegistry {
    let probe = Box::new(MockLivenessProbe::with_alive(alive));
    SessionRegistry::open_at_with_probe(path, probe).expect("open registry")
}

/// Raw insert that bypasses `register_self` (the public path sets
/// the `registered` flag, which we *don't* want for most tests).
fn raw_insert(reg: &SessionRegistry, pid: u32) {
    raw_insert_all(reg, &[pid]);
}

/// [`raw_insert`] for many rows in one transaction: each commit syncs to disk.
fn raw_insert_all(reg: &SessionRegistry, pids: &[u32]) {
    let row = SessionRow {
        started_unix: 0,
        backend: None,
        launch_mode: None,
        cwd: None,
    };
    let bytes = serde_json::to_vec(&row).unwrap();
    let wtxn = reg.db.begin_write().unwrap();
    {
        let mut table = wtxn.open_table(SESSIONS).unwrap();
        for pid in pids {
            table.insert(*pid, bytes.as_slice()).unwrap();
        }
    }
    wtxn.commit().unwrap();
}

#[test]
fn gc_removes_dead_pids() {
    // u32::MAX is virtually guaranteed to be a dead PID. Insert it,
    // then call gc_dead_sessions and assert it's gone.
    let path = fresh_db_path("gc-dead");
    let reg = open_with_alive_set(&path, vec![]);
    raw_insert(&reg, u32::MAX);
    raw_insert(&reg, u32::MAX - 1);
    assert_eq!(reg.count_live().unwrap(), 2);
    let removed = reg.gc_dead_sessions().unwrap();
    assert_eq!(removed, 2);
    assert_eq!(reg.count_live().unwrap(), 0);
}

#[test]
fn gc_keeps_live_pids() {
    let path = fresh_db_path("gc-live");
    let reg = open_with_alive_set(&path, vec![1234, 5678]);
    raw_insert(&reg, 1234);
    raw_insert(&reg, 5678);
    raw_insert(&reg, 9999); // not in alive set => dead
    let removed = reg.gc_dead_sessions().unwrap();
    assert_eq!(removed, 1);
    assert_eq!(reg.count_live().unwrap(), 2);
}

#[test]
fn count_under_cap_returns_allow() {
    let path = fresh_db_path("under-cap");
    let reg = open_with_alive_set(&path, vec![]);
    let cfg = CapConfig::defaults();
    assert_eq!(reg.check_cap(&cfg).unwrap(), CapDecision::Allow);
}

#[test]
fn count_at_warn_returns_warn() {
    // Populate DB with N=warn rows of distinct, "alive" fake PIDs so
    // GC wouldn't reap them. We don't run GC here — check_cap itself
    // doesn't either.
    let path = fresh_db_path("at-warn");
    let cfg = CapConfig { max: 10, warn: 5 };
    let alive: Vec<u32> = (1000..1000 + cfg.warn as u32).collect();
    let reg = open_with_alive_set(&path, alive.clone());
    for pid in &alive {
        raw_insert(&reg, *pid);
    }
    assert_eq!(reg.count_live().unwrap(), cfg.warn);
    assert_eq!(reg.check_cap(&cfg).unwrap(), CapDecision::Warn(cfg.warn));
}

#[test]
fn count_at_cap_returns_refuse() {
    let path = fresh_db_path("at-cap");
    let cfg = CapConfig { max: 4, warn: 2 };
    let alive: Vec<u32> = (2000..2000 + cfg.max as u32).collect();
    let reg = open_with_alive_set(&path, alive.clone());
    for pid in &alive {
        raw_insert(&reg, *pid);
    }
    assert_eq!(reg.count_live().unwrap(), cfg.max);
    assert_eq!(reg.check_cap(&cfg).unwrap(), CapDecision::Refuse(cfg.max));
}

/// **Issue #73 regression test**: verifies the `CLUD_MAX_INSTANCES=0`
/// "cap disabled" hatch actually disables the cap. A future commit
/// that drops the `cfg.max == CAP_DISABLED` short-circuit fails this
/// test instead of silently breaking the env-var override that ops
/// folks may rely on to recover from a stuck registry.
#[test]
fn fork_bomb_regression_max_instances_zero_disables_cap() {
    let path = fresh_db_path("max-zero-disables");
    // 1000 fake-alive PIDs.
    let alive: Vec<u32> = (10_000..11_000).collect();
    let reg = open_with_alive_set(&path, alive.clone());
    raw_insert_all(&reg, &alive);
    let cfg = CapConfig { max: 0, warn: 0 };
    assert_eq!(reg.count_live().unwrap(), 1000);
    assert_eq!(reg.check_cap(&cfg).unwrap(), CapDecision::Allow);
}

/// **Issue #73 fork-bomb regression test** — the explicit one the
/// user asked for. With `CLUD_MAX_INSTANCES=1` and a single live
/// sibling already in the DB, `check_cap` MUST refuse. A future
/// commit that accidentally removes the cap check, inverts the
/// comparison, or special-cases small caps will fail this test
/// instead of silently letting `clud` fork-bomb the workstation.
#[test]
fn fork_bomb_regression_max_instances_one_caps_at_one() {
    let path = fresh_db_path("max-one-caps");
    let reg = open_with_alive_set(&path, vec![424242]);
    raw_insert(&reg, 424242);
    let cfg = CapConfig { max: 1, warn: 0 };
    assert_eq!(reg.count_live().unwrap(), 1);
    assert_eq!(reg.check_cap(&cfg).unwrap(), CapDecision::Refuse(1));
}

#[test]
fn register_then_drop_round_trips() {
    let path = fresh_db_path("register-drop");
    {
        let reg = open_with_alive_set(&path, vec![std::process::id()]);
        let info = SessionInfo {
            pid: std::process::id(),
            started_unix: 1234,
            backend: Some("claude".into()),
            launch_mode: Some("subprocess".into()),
            cwd: Some("/tmp/x".into()),
        };
        reg.register_self(info).unwrap();
        assert_eq!(reg.count_live().unwrap(), 1);
    }
    // Reopen and check the row was deleted on drop.
    let reg2 = open_with_alive_set(&path, vec![]);
    assert_eq!(reg2.count_live().unwrap(), 0);
}

#[test]
fn drop_without_register_does_not_delete_other_rows() {
    // If `register_self` was never called, Drop should not touch the
    // DB. Otherwise an early-aborted clud (e.g. cap-exceeded refuse)
    // would clobber a sibling row that *happens* to share its PID
    // namespace via PID reuse.
    let path = fresh_db_path("drop-no-register");
    let reg = open_with_alive_set(&path, vec![]);
    raw_insert(&reg, std::process::id()); // pretend a sibling has our PID
    drop(reg);
    let reg2 = open_with_alive_set(&path, vec![]);
    assert_eq!(reg2.count_live().unwrap(), 1);
}

#[test]
fn cap_config_from_env_defaults() {
    let _g = env_lock();
    // SAFETY: serialized via ENV_LOCK.
    unsafe {
        std::env::remove_var(ENV_MAX_INSTANCES);
        std::env::remove_var(ENV_WARN_INSTANCES);
    }
    let cfg = SessionRegistry::cap_config_from_env();
    assert_eq!(
        cfg,
        CapConfig {
            max: DEFAULT_MAX_INSTANCES,
            warn: DEFAULT_MAX_INSTANCES / 2,
        }
    );
}

#[test]
fn cap_config_from_env_custom() {
    let _g = env_lock();
    unsafe {
        std::env::set_var(ENV_MAX_INSTANCES, "10");
        std::env::set_var(ENV_WARN_INSTANCES, "3");
    }
    let cfg = SessionRegistry::cap_config_from_env();
    unsafe {
        std::env::remove_var(ENV_MAX_INSTANCES);
        std::env::remove_var(ENV_WARN_INSTANCES);
    }
    assert_eq!(cfg, CapConfig { max: 10, warn: 3 });
}

#[test]
fn cap_config_from_env_max_only_redrives_warn() {
    let _g = env_lock();
    unsafe {
        std::env::set_var(ENV_MAX_INSTANCES, "8");
        std::env::remove_var(ENV_WARN_INSTANCES);
    }
    let cfg = SessionRegistry::cap_config_from_env();
    unsafe {
        std::env::remove_var(ENV_MAX_INSTANCES);
    }
    assert_eq!(cfg, CapConfig { max: 8, warn: 4 });
}

#[test]
fn cap_config_from_env_clamps_warn_to_max() {
    let _g = env_lock();
    unsafe {
        std::env::set_var(ENV_MAX_INSTANCES, "5");
        std::env::set_var(ENV_WARN_INSTANCES, "999");
    }
    let cfg = SessionRegistry::cap_config_from_env();
    unsafe {
        std::env::remove_var(ENV_MAX_INSTANCES);
        std::env::remove_var(ENV_WARN_INSTANCES);
    }
    assert_eq!(cfg.max, 5);
    assert_eq!(cfg.warn, 5);
}

#[test]
fn gc_handles_concurrent_writes() {
    // Two registries on the same DB; register both, drop one, GC,
    // count → 1.
    //
    // NOTE on redb concurrency: redb takes an exclusive lock per
    // process via flock/LockFileEx. Opening the *same* file twice
    // from the same process succeeds on Windows and macOS/Linux
    // because the lock is held by the file descriptor, not by the
    // process — but the test's intent is to verify that two
    // independent SessionRegistry instances over the same file
    // coordinate correctly via redb's own write serialization.
    let path = fresh_db_path("concurrent");
    let pid_a: u32 = 700_001;
    let pid_b: u32 = 700_002;
    let mut reg_a = SessionRegistry::open_at_with_probe(
        &path,
        Box::new(MockLivenessProbe::with_alive([pid_a, pid_b])),
    )
    .unwrap();

    // Override own_pid so two registries in one test process can
    // each "register themselves" without colliding on the primary
    // key (and so each one's Drop removes its *own* row).
    reg_a.set_own_pid_for_test(pid_a);
    reg_a
        .register_self(SessionInfo {
            pid: pid_a,
            started_unix: 0,
            backend: None,
            launch_mode: None,
            cwd: None,
        })
        .unwrap();
    // Insert the sibling row directly — opening a second redb handle
    // on the same file in the same process is not supported (file
    // lock conflict), but the cap-check semantics we want to test
    // are: row count, GC keeps live rows, drop reduces count.
    raw_insert(&reg_a, pid_b);
    assert_eq!(reg_a.count_live().unwrap(), 2);

    // Drop pid_b's row directly. From reg_a's perspective only one
    // row remains.
    {
        let wtxn = reg_a.db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(SESSIONS).unwrap();
            t.remove(pid_b).unwrap();
        }
        wtxn.commit().unwrap();
    }
    // GC with both PIDs marked alive: nothing to remove.
    let removed = reg_a.gc_dead_sessions().unwrap();
    assert_eq!(removed, 0);
    assert_eq!(reg_a.count_live().unwrap(), 1);
}

#[test]
fn schema_bootstrap_is_idempotent() {
    let path = fresh_db_path("schema-idempotent");
    // Open twice in a row — second open must not error.
    let reg1 = open_with_alive_set(&path, vec![]);
    drop(reg1);
    let reg2 = open_with_alive_set(&path, vec![]);
    // schema_version row was inserted exactly once and equals 1.
    let rtxn = reg2.db.begin_read().unwrap();
    let meta = rtxn.open_table(META).unwrap();
    let v = meta.get("schema_version").unwrap().unwrap().value();
    assert_eq!(v, 1);
}

#[test]
fn decide_cap_branches() {
    // Pure-function coverage: keep the branch table here so a
    // refactor that reshapes `decide_cap` has to update *one* test
    // and not three.
    let cfg = CapConfig { max: 10, warn: 5 };
    assert_eq!(decide_cap(0, &cfg), CapDecision::Allow);
    assert_eq!(decide_cap(4, &cfg), CapDecision::Allow);
    assert_eq!(decide_cap(5, &cfg), CapDecision::Warn(5));
    assert_eq!(decide_cap(9, &cfg), CapDecision::Warn(9));
    assert_eq!(decide_cap(10, &cfg), CapDecision::Refuse(10));
    assert_eq!(decide_cap(99, &cfg), CapDecision::Refuse(99));

    // max == 0 disables the cap entirely.
    let disabled = CapConfig { max: 0, warn: 0 };
    assert_eq!(decide_cap(99, &disabled), CapDecision::Allow);

    // warn == 0 with max > 0 means "no warn band, just the cap".
    let no_warn = CapConfig { max: 5, warn: 0 };
    assert_eq!(decide_cap(4, &no_warn), CapDecision::Allow);
    assert_eq!(decide_cap(5, &no_warn), CapDecision::Refuse(5));
}

#[test]
fn mock_liveness_probe_set_arithmetic() {
    let probe = MockLivenessProbe::with_alive([1, 2, 3]);
    assert!(probe.is_alive(1));
    assert!(probe.is_alive(2));
    assert!(!probe.is_alive(99));
    probe.mark_dead(2);
    assert!(!probe.is_alive(2));
    probe.mark_alive(99);
    assert!(probe.is_alive(99));
}

#[test]
fn os_liveness_probe_treats_pid_zero_as_dead() {
    // PID 0 is reserved on every OS we ship to (Idle on Windows,
    // process-group sentinel on POSIX). Counting it as a "clud
    // sibling" would be a bug.
    let probe = OsLivenessProbe;
    assert!(!probe.is_alive(0));
}

#[test]
fn os_liveness_probe_recognizes_self() {
    // The current test process must show up as alive — this is the
    // closest thing to an integration smoke test we can run without
    // launching a child. If this ever fails, the cap will refuse to
    // launch a fresh `clud` even on an empty DB (because GC would
    // wrongly reap our own row).
    let probe = OsLivenessProbe;
    assert!(probe.is_alive(std::process::id()));
}

#[test]
fn session_info_for_self_uses_current_pid_and_cwd() {
    let info = SessionInfo::for_self(Some("claude".into()), Some("subprocess".into()));
    assert_eq!(info.pid, std::process::id());
    assert!(info.started_unix > 0);
    assert!(info.cwd.is_some());
}

#[test]
fn distinct_db_paths_do_not_collide() {
    // Each test gets its own DB path; this asserts the helper itself
    // returns distinct paths so future tests can rely on it.
    let a = fresh_db_path("a");
    let b = fresh_db_path("b");
    assert_ne!(a, b);
    let mut seen = HashSet::new();
    seen.insert(a);
    seen.insert(b);
    assert_eq!(seen.len(), 2);
}

/// **Issue #138 regression test**: `unregister` deletes the row
/// synchronously and clears the `registered` flag so a subsequent
/// `Drop` doesn't try to re-delete the row (and possibly clobber a
/// sibling that inherited our PID via PID reuse).
#[test]
fn unregister_deletes_row_and_clears_flag() {
    let path = fresh_db_path("unregister");
    let reg = open_with_alive_set(&path, vec![std::process::id()]);
    reg.register_self(SessionInfo {
        pid: std::process::id(),
        started_unix: 1234,
        backend: None,
        launch_mode: None,
        cwd: None,
    })
    .unwrap();
    assert_eq!(reg.count_live().unwrap(), 1);
    assert!(reg.registered.load(std::sync::atomic::Ordering::SeqCst));
    reg.unregister().unwrap();
    assert_eq!(reg.count_live().unwrap(), 0);
    assert!(!reg.registered.load(std::sync::atomic::Ordering::SeqCst));
    // Subsequent Drop must not panic and must not touch the table.
    drop(reg);
    let reg2 = open_with_alive_set(&path, vec![]);
    assert_eq!(reg2.count_live().unwrap(), 0);
}

/// **Issue #138 regression test**: two `acquire_lock_at` calls on the
/// same lock path serialize — the second one only returns after the
/// first guard drops. We model "wait for the other thread" with a
/// barrier + a generous timeout to keep the test deterministic on
/// slow CI without making the happy path slow.
///
/// Why this is the right shape: redb's exclusive lock fails fast on
/// contention; `fs4`'s `lock_exclusive` blocks. The lock-file pattern
/// from issue #138 converts a "fail on contention" surface into a
/// "queue on contention" surface — that's what this test pins.
#[test]
fn acquire_lock_serializes_callers() {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    let dir = tempfile::tempdir().expect("tempdir");
    let lock_path = dir.path().join("acquire-serializes.lock");

    // Holder thread: grabs the lock, then sleeps for a known interval.
    let holder_path = lock_path.clone();
    let holder_started = Arc::new(AtomicU64::new(0));
    let holder_released = Arc::new(AtomicU64::new(0));
    let holder_started_clone = Arc::clone(&holder_started);
    let holder_released_clone = Arc::clone(&holder_released);
    let holder = thread::spawn(move || {
        let _guard = acquire_lock_at(&holder_path).expect("holder lock");
        holder_started_clone.store(now_ms(), Ordering::SeqCst);
        thread::sleep(Duration::from_millis(200));
        holder_released_clone.store(now_ms(), Ordering::SeqCst);
    });

    // Wait until the holder confirms it owns the lock before we try
    // to acquire from this thread. Otherwise we might *win* the race
    // and the test asserts nothing.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while holder_started.load(Ordering::SeqCst) == 0 {
        if std::time::Instant::now() > deadline {
            panic!("holder never acquired the lock");
        }
        thread::sleep(Duration::from_millis(5));
    }

    // Now race: this acquire MUST block until the holder releases.
    let waiter_acquired = now_ms();
    let _guard = acquire_lock_at(&lock_path).expect("waiter lock");
    let waiter_unblocked = now_ms();
    holder.join().expect("holder join");

    // The waiter's unblock time should be at or after the holder's
    // release time. Generous epsilon (50ms) for clock skew between
    // the two thread observations on a busy CI runner.
    let released = holder_released.load(Ordering::SeqCst);
    assert!(
        waiter_unblocked + 50 >= released,
        "waiter unblocked at {waiter_unblocked} but holder released at {released}",
    );
    // Sanity: the waiter was at least delayed beyond when it tried.
    assert!(
        waiter_unblocked >= waiter_acquired,
        "waiter clock skew detected: tried at {waiter_acquired}, unblocked at {waiter_unblocked}",
    );
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// **Issue #138 regression test**: `run_startup_under_lock` opens the
/// redb file, performs gc / cap-check / register inside the lock, and
/// **closes the file before returning**. After it returns, a subsequent
/// caller can immediately open the redb file — proving the lock is
/// scoped to the helper, not the whole `clud` lifetime.
#[test]
fn run_startup_under_lock_releases_redb_after_return() {
    let _g = env_lock();
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("startup-releases.redb");
    let lock = dir.path().join("startup-releases.lock");
    // SAFETY: serialized via ENV_LOCK.
    unsafe {
        std::env::set_var(ENV_SESSION_DB, &db);
        std::env::set_var(ENV_SESSION_LOCK, &lock);
        std::env::set_var(ENV_MAX_INSTANCES, "0"); // disable cap to keep test simple
    }

    let info = SessionInfo {
        pid: std::process::id(),
        started_unix: 1,
        backend: None,
        launch_mode: None,
        cwd: None,
    };
    let cfg = SessionRegistry::cap_config_from_env();
    let outcome = run_startup_under_lock(&cfg, info).expect("startup");
    assert_eq!(outcome.decision, CapDecision::Allow);
    assert!(outcome.registered);

    // If `run_startup_under_lock` left redb open, this would fail
    // with `DatabaseAlreadyOpen`. The whole point of issue #138 is
    // that this succeeds without contention.
    let reopen = SessionRegistry::open_default().expect("reopen");
    assert_eq!(reopen.count_live().unwrap(), 1);
    drop(reopen);

    // Shutdown removes the row, again under the lock.
    run_shutdown_under_lock().expect("shutdown");
    let after = SessionRegistry::open_default().expect("reopen after shutdown");
    assert_eq!(after.count_live().unwrap(), 0);

    unsafe {
        std::env::remove_var(ENV_SESSION_DB);
        std::env::remove_var(ENV_SESSION_LOCK);
        std::env::remove_var(ENV_MAX_INSTANCES);
    }
}

/// **Issue #1660**: exit must not wait on a held registry lock. Another
/// holder keeps the lock; the bounded shutdown gives up after its budget and
/// reports it, instead of blocking the process in `session_guard_drop`.
#[test]
fn bounded_shutdown_returns_when_lock_is_held() {
    let _g = env_lock();
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("bounded.redb");
    let lock = dir.path().join("bounded.lock");
    // SAFETY: serialized via ENV_LOCK.
    unsafe {
        std::env::set_var(ENV_SESSION_DB, &db);
        std::env::set_var(ENV_SESSION_LOCK, &lock);
    }
    let held = acquire_lock_at(&lock).expect("hold lock");
    let budget = std::time::Duration::from_millis(200);
    let started = std::time::Instant::now();
    let result = run_shutdown_under_lock_bounded(budget);
    let elapsed = started.elapsed();
    unsafe {
        std::env::remove_var(ENV_SESSION_DB);
        std::env::remove_var(ENV_SESSION_LOCK);
    }
    assert!(result.is_err(), "held lock must surface as a timeout");
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "bounded shutdown blocked for {elapsed:?} behind a held lock"
    );
    // Releasing lets the detached worker finish against the tempdir paths it
    // captured, not whatever the environment says now.
    drop(held);
}

/// **Issue #1660**: uncontended, the bounded shutdown still removes the row.
#[test]
fn bounded_shutdown_removes_row_when_uncontended() {
    let _g = env_lock();
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("bounded-ok.redb");
    let lock = dir.path().join("bounded-ok.lock");
    // SAFETY: serialized via ENV_LOCK.
    unsafe {
        std::env::set_var(ENV_SESSION_DB, &db);
        std::env::set_var(ENV_SESSION_LOCK, &lock);
        std::env::set_var(ENV_MAX_INSTANCES, "0");
    }
    let info = SessionInfo {
        pid: std::process::id(),
        started_unix: 1,
        backend: None,
        launch_mode: None,
        cwd: None,
    };
    let cfg = SessionRegistry::cap_config_from_env();
    assert!(
        run_startup_under_lock(&cfg, info)
            .expect("startup")
            .registered
    );
    // #2158: this test is about the row being removed, not the 2 s budget
    // (`bounded_shutdown_returns_when_lock_is_held` owns that); a loaded
    // fsync can exceed 2 s on an uncontended lock.
    run_shutdown_under_lock_bounded(std::time::Duration::from_secs(30)).expect("bounded shutdown");
    let after = SessionRegistry::open_default().expect("reopen");
    assert_eq!(after.count_live().unwrap(), 0);
    drop(after);
    unsafe {
        std::env::remove_var(ENV_SESSION_DB);
        std::env::remove_var(ENV_SESSION_LOCK);
        std::env::remove_var(ENV_MAX_INSTANCES);
    }
}

/// **Issue #138 regression test**: `default_lock_path` derives from
/// the DB path's parent dir when `CLUD_SESSION_LOCK` is unset.
/// `CLUD_SESSION_LOCK` wins when both are set.
#[test]
fn default_lock_path_derives_from_db_parent_or_env() {
    let _g = env_lock();
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("derives.redb");
    // SAFETY: serialized via ENV_LOCK.
    unsafe {
        std::env::set_var(ENV_SESSION_DB, &db);
        std::env::remove_var(ENV_SESSION_LOCK);
    }
    let derived = default_lock_path().expect("derived");
    assert_eq!(derived, dir.path().join("sessions.lock"));

    let explicit = dir.path().join("custom.lock");
    unsafe {
        std::env::set_var(ENV_SESSION_LOCK, &explicit);
    }
    let resolved = default_lock_path().expect("explicit");
    assert_eq!(resolved, explicit);
    unsafe {
        std::env::remove_var(ENV_SESSION_DB);
        std::env::remove_var(ENV_SESSION_LOCK);
    }
}

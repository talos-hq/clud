use super::*;

// Issues #509/#510: background maintenance-sweep prioritization.
#[test]
fn maintenance_action_prioritizes_low_disk() {
    assert_eq!(maintenance_action(true, true), MaintenanceAction::RunUrgent);
    assert_eq!(
        maintenance_action(true, false),
        MaintenanceAction::RunUrgent
    );
}

#[test]
fn maintenance_action_defers_when_busy_and_disk_ok() {
    assert_eq!(maintenance_action(false, true), MaintenanceAction::Defer);
}

#[test]
fn maintenance_action_runs_normal_when_idle_and_disk_ok() {
    assert_eq!(
        maintenance_action(false, false),
        MaintenanceAction::RunNormal
    );
}

#[test]
fn sweep_cpu_ceiling_defaults_and_overrides() {
    assert_eq!(sweep_cpu_ceiling_pct(None), DEFAULT_GC_SWEEP_MAX_CPU_PCT);
    assert_eq!(sweep_cpu_ceiling_pct(Some("  75 ")), 75.0);
    assert_eq!(
        sweep_cpu_ceiling_pct(Some("nan")),
        DEFAULT_GC_SWEEP_MAX_CPU_PCT
    );
    assert_eq!(
        sweep_cpu_ceiling_pct(Some("0")),
        DEFAULT_GC_SWEEP_MAX_CPU_PCT
    );
    assert_eq!(
        sweep_cpu_ceiling_pct(Some("-5")),
        DEFAULT_GC_SWEEP_MAX_CPU_PCT
    );
}

pub(super) fn spawn_test_worker(
    db_path: &Path,
) -> (
    mpsc::Sender<RegistryMsg>,
    std::sync::MutexGuard<'static, ()>,
) {
    spawn_test_worker_with_tick(db_path, "0")
}

pub(super) fn restore_env_var(key: &str, prior: Option<std::ffi::OsString>) {
    match prior {
        Some(value) => std::env::set_var(key, value),
        None => std::env::remove_var(key),
    }
}

#[test]
fn watch_registration_acks_before_initial_scan_and_discovers_matching_child() {
    let temp = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let worktrees = temp.path().join(".claude").join("worktrees");
    fs::create_dir_all(worktrees.join("agent-zz")).unwrap();
    let db = temp.path().join("registry.redb");
    let (tx, _guard) = spawn_test_worker(&db);
    let (reply_tx, reply_rx) = mpsc::sync_channel(1);
    tx.send(RegistryMsg::Op(GcRequestMsg {
        op: GcOp::Watch {
            kind: WORKTREE_KIND.to_string(),
            watch_dir: worktrees.to_string_lossy().to_string(),
            repo_root: Some(temp.path().to_string_lossy().to_string()),
        },
        reply_tx,
    }))
    .unwrap();
    assert!(matches!(
        reply_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
        GcReply::WatchOk
    ));
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let (list_tx, list_rx) = mpsc::sync_channel(1);
        tx.send(RegistryMsg::Op(GcRequestMsg {
            op: GcOp::List {
                kind: Some(WORKTREE_KIND.to_string()),
            },
            reply_tx: list_tx,
        }))
        .unwrap();
        // A slow individual reply is not a failure: the contract this test
        // asserts is "the initial scan lands within the deadline", so a
        // timeout on one poll has to fall through to the deadline check and
        // try again. Unwrapping it instead turned a loaded runner into a hard
        // `Err(Timeout)` panic while seconds of budget remained.
        if matches!(
            list_rx.recv_timeout(Duration::from_secs(1)),
            Ok(GcReply::ListOk { rows }) if rows.len() == 1 && rows[0].path.ends_with("agent-zz")
        ) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "watch initial scan did not complete"
        );
        thread::sleep(Duration::from_millis(25));
    }
}

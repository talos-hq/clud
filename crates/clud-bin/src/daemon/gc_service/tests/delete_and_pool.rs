use super::*;

/// Issue #183: per-row Delete must target exactly the requested id
/// regardless of how many siblings share its kind. Earlier iterations
/// of the dashboard worked around the missing IPC primitive by
/// issuing `Purge { kind: Some(k) }` and refusing when k had >1 row,
/// which broke the per-row button in the common multi-row case.
#[test]
fn delete_by_id_removes_only_the_targeted_row() {
    let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let db_path = dir.path().join("delete-by-id.redb");
    let (tx, _g) = spawn_test_worker(&db_path);

    // Three rows of the same kind — the bug case the workaround
    // refused to handle.
    let paths = [
        dir.path().join("e1").to_string_lossy().to_string(),
        dir.path().join("e2").to_string_lossy().to_string(),
        dir.path().join("e3").to_string_lossy().to_string(),
    ];
    for p in &paths {
        std::fs::create_dir_all(p).unwrap();
        call(
            &tx,
            GcOp::Insert {
                kind: "cache".to_string(),
                path: p.clone(),
                repo_root: None,
                branch: None,
                agent_id: None,
                created_unix: Some(100),
            },
        );
    }

    // Snapshot the rows so we can pick the middle id by stable mapping.
    let list = match call(&tx, GcOp::List { kind: None }) {
        GcReply::ListOk { rows } => rows,
        other => panic!("unexpected reply: {other:?}"),
    };
    assert_eq!(list.len(), 3);
    let middle = list
        .iter()
        .find(|r| r.path == paths[1])
        .expect("middle row");

    let resp = call(&tx, GcOp::DeleteById { id: middle.id });
    match resp {
        GcReply::PurgeOk { removed, skipped } => {
            assert_eq!(removed, 1);
            assert_eq!(skipped, 0);
        }
        other => panic!("unexpected reply: {other:?}"),
    }

    // The two siblings must survive.
    let after = match call(&tx, GcOp::List { kind: None }) {
        GcReply::ListOk { rows } => rows,
        other => panic!("unexpected reply: {other:?}"),
    };
    let remaining: Vec<&str> = after.iter().map(|r| r.path.as_str()).collect();
    assert_eq!(after.len(), 2);
    assert!(remaining.contains(&paths[0].as_str()));
    assert!(remaining.contains(&paths[2].as_str()));
    assert!(!remaining.contains(&paths[1].as_str()));

    // The on-disk path for the targeted row should be gone too.
    assert!(!std::path::Path::new(&paths[1]).exists());
    // Siblings should still be on disk.
    assert!(std::path::Path::new(&paths[0]).exists());
    assert!(std::path::Path::new(&paths[2]).exists());
}

/// Issue #268: the env-driven purge-pool concurrency knob.
#[test]
fn purge_concurrency_from_raw_picks_user_value_and_falls_back() {
    let default = default_purge_concurrency();
    assert!((1..=DEFAULT_GC_PURGE_CONCURRENCY_CAP).contains(&default));
    assert_eq!(purge_concurrency_from_raw(None), default);
    assert_eq!(purge_concurrency_from_raw(Some("4")), 4);
    // Empty / zero / non-numeric all fall back to the default.
    assert_eq!(purge_concurrency_from_raw(Some(" ")), default);
    assert_eq!(purge_concurrency_from_raw(Some("0")), default);
    assert_eq!(purge_concurrency_from_raw(Some("bad")), default);
}

/// Issue #268: dispatch returns `PurgeStarted` with the count of
/// jobs enqueued plus the count filtered out by the live/kind
/// gates, not the count actually removed.
#[test]
fn dispatch_purge_entries_returns_purge_started_with_counts() {
    let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let path_keep = dir.path().join("keep");
    let path_a = dir.path().join("a");
    let path_b = dir.path().join("b");
    for p in [&path_keep, &path_a, &path_b] {
        std::fs::create_dir_all(p).unwrap();
    }
    let candidates = vec![
        TrackedEntry {
            id: 1,
            kind: "cache".to_string(),
            path: path_a.to_string_lossy().to_string(),
            repo_root: None,
            branch: None,
            agent_id: None,
            created_unix: 100,
        },
        TrackedEntry {
            id: 2,
            kind: "cache".to_string(),
            path: path_b.to_string_lossy().to_string(),
            repo_root: None,
            branch: None,
            agent_id: None,
            created_unix: 100,
        },
        // path_keep is "live" via the live-cwd filter below.
        TrackedEntry {
            id: 3,
            kind: "cache".to_string(),
            path: path_keep.to_string_lossy().to_string(),
            repo_root: None,
            branch: None,
            agent_id: None,
            created_unix: 100,
        },
    ];
    let pool_tx = spawn_purge_pool(2);
    let (completion_tx, _completion_rx) = mpsc::channel::<RegistryMsg>();
    let reply = dispatch_purge_entries(
        &pool_tx,
        &completion_tx,
        candidates,
        vec![path_keep.clone()],
        &mut SpareReasons::new(),
    );
    match reply {
        GcReply::PurgeStarted {
            dispatched,
            skipped,
        } => {
            assert_eq!(dispatched, 2);
            assert_eq!(skipped, 1);
        }
        other => panic!("expected PurgeStarted, got {other:?}"),
    }
}

use super::*;

/// Issue #268: a successful completion drops the row; a failed one
/// leaves it in place so the next purge tries again.
#[test]
fn apply_purge_completion_drops_row_on_success_keeps_on_failure() {
    let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let db_path = dir.path().join("apply-completion.redb");
    let registry = Registry::open_at(&db_path).unwrap();
    let ok_path = dir.path().join("ok");
    let fail_path = dir.path().join("fail");
    std::fs::create_dir_all(&ok_path).unwrap();
    std::fs::create_dir_all(&fail_path).unwrap();
    registry
        .insert_if_new(&InsertInput {
            kind: "cache".to_string(),
            path: ok_path.to_string_lossy().to_string(),
            repo_root: None,
            branch: None,
            agent_id: None,
            now_unix: 100,
        })
        .unwrap();
    registry
        .insert_if_new(&InsertInput {
            kind: "cache".to_string(),
            path: fail_path.to_string_lossy().to_string(),
            repo_root: None,
            branch: None,
            agent_id: None,
            now_unix: 100,
        })
        .unwrap();
    let rows = registry.list(None).unwrap();
    let ok_path_str = ok_path.to_string_lossy();
    let fail_path_str = fail_path.to_string_lossy();
    let ok_id = rows.iter().find(|r| r.path == ok_path_str).unwrap().id;
    let fail_id = rows.iter().find(|r| r.path == fail_path_str).unwrap().id;

    apply_purge_completion(
        &registry,
        &mut SpareReasons::new(),
        PurgeCompletion {
            id: ok_id,
            path: ok_path.to_string_lossy().to_string(),
            kind: "cache".to_string(),
            result: Ok(()),
            spared: None,
        },
    );
    apply_purge_completion(
        &registry,
        &mut SpareReasons::new(),
        PurgeCompletion {
            id: fail_id,
            path: fail_path.to_string_lossy().to_string(),
            kind: "cache".to_string(),
            result: Err("locked".to_string()),
            spared: None,
        },
    );
    let remaining: Vec<_> = registry
        .list(None)
        .unwrap()
        .into_iter()
        .map(|r| r.path)
        .collect();
    assert_eq!(remaining, vec![fail_path.to_string_lossy().to_string()]);
}

/// Issue #268: while the pool is grinding through a purge the
/// registry worker MUST keep serving cheap client ops. Earlier
/// inline-rm-rf design blocked the worker for the whole purge and
/// blew past the 30s `WORKER_REPLY_TIMEOUT`.
#[test]
fn bulk_purge_keeps_serving_list_while_pool_grinds_through() {
    let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let db_path = dir.path().join("parallel-serve.redb");
    let (tx, _g) = spawn_test_worker(&db_path);

    // Insert 50 entries pointing at real (but small) tempdirs so
    // the pool actually has to call remove_dir_all.
    let mut paths = Vec::new();
    for i in 0..50 {
        let p = dir.path().join(format!("e-{i}"));
        std::fs::create_dir_all(&p).unwrap();
        paths.push(p.to_string_lossy().to_string());
        call(
            &tx,
            GcOp::Insert {
                kind: "cache".to_string(),
                path: p.to_string_lossy().to_string(),
                repo_root: None,
                branch: None,
                agent_id: None,
                created_unix: Some(100),
            },
        );
    }

    // Bulk purge: should return PurgeStarted quickly.
    let started = Instant::now();
    let reply = call(
        &tx,
        GcOp::Purge {
            duration: None,
            kind: None,
            dry_run: false,
        },
    );
    let purge_latency = started.elapsed();
    match reply {
        GcReply::PurgeStarted {
            dispatched,
            skipped,
        } => {
            assert_eq!(dispatched, 50);
            assert_eq!(skipped, 0);
        }
        other => panic!("expected PurgeStarted, got {other:?}"),
    }
    // The dispatch path is O(candidates) redb reads + N channel
    // sends — should complete well under a second even on slow
    // CI runners.
    assert!(
        purge_latency < Duration::from_secs(2),
        "Purge IPC should return immediately, took {purge_latency:?}"
    );

    // Issue a `List` while completions are still landing. The
    // worker must serve it without blocking.
    let list_started = Instant::now();
    let _ = call(&tx, GcOp::List { kind: None });
    let list_latency = list_started.elapsed();
    assert!(
        list_latency < Duration::from_secs(2),
        "List during parallel purge should not block, took {list_latency:?}"
    );

    // Eventually all rows drain.
    let rows = wait_for_row_count(&tx, None, 0, Duration::from_secs(10));
    assert!(rows.is_empty(), "expected drain, got {rows:?}");
    for p in &paths {
        assert!(!std::path::Path::new(p).exists(), "leftover {p}");
    }
}

/// Deleting a non-existent id is idempotent (`removed=0, skipped=0`).
/// Lets the dashboard refresh-then-click race resolve without a 500.
#[test]
fn delete_by_id_with_missing_id_is_idempotent() {
    let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let db_path = dir.path().join("delete-missing.redb");
    let (tx, _g) = spawn_test_worker(&db_path);

    let resp = call(&tx, GcOp::DeleteById { id: 9_999_999 });
    match resp {
        GcReply::PurgeOk { removed, skipped } => {
            assert_eq!(removed, 0);
            assert_eq!(skipped, 0);
        }
        other => panic!("unexpected reply: {other:?}"),
    }
}

//! Issue #2095: a daemon whose registry open fails keeps retrying instead of
//! running without GC for its whole lifetime.

use super::*;

fn deferred_config() -> RegistryWorkerConfig {
    RegistryWorkerConfig {
        tick_cadence: None,
        live_cwds_provider: Arc::new(Vec::<PathBuf>::new),
        session_state_dir: None,
        activity: None,
    }
}

#[test]
fn locked_registry_is_retried_until_the_holder_lets_go() {
    let dir = kernal_api::platform::fs::TemporaryDirectory::new().expect("tempdir");
    let db_path = dir.path().join("data.redb");
    // Stands in for the superseded daemon that still holds the redb lock.
    let holder = Registry::open_at(&db_path).expect("open holder");

    let open_path = db_path.clone();
    let tx = spawn_deferred_registry_worker(
        move || Registry::open_at(&open_path),
        deferred_config(),
        Duration::from_millis(50),
    )
    .expect("spawn deferred worker");

    match call(&tx, GcOp::List { kind: None }) {
        GcReply::Error { message } => assert!(
            message.contains("retrying"),
            "locked registry should say it is retrying, got: {message}"
        ),
        other => panic!("expected an error while the registry is locked, got {other:?}"),
    }

    drop(holder);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match call(&tx, GcOp::List { kind: None }) {
            GcReply::ListOk { .. } => break,
            other => assert!(
                Instant::now() < deadline,
                "registry never opened after the holder dropped: {other:?}"
            ),
        }
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn deferred_wait_ends_when_every_sender_is_gone() {
    let (tx, rx) = mpsc::channel::<RegistryMsg>();
    drop(tx);
    let opened = wait_for_registry(
        &rx,
        || Err(crate::gc::GcError::NoDefaultPath),
        Duration::from_millis(10),
    );
    assert!(opened.is_none());
}

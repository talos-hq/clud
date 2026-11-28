use super::*;

pub(crate) fn entry(id: &str, activity: &str, route: Route, inferred: bool) -> SessionEntry {
    SessionEntry {
        session_id: id.to_string(),
        transcript_path: PathBuf::from(format!("/nowhere/{id}.jsonl")),
        route,
        route_inferred: inferred,
        model: None,
        title: None,
        last_activity: Some(activity.to_string()),
        compact_checkpoints: 0,
        lineage: None,
    }
}

#[test]
fn sessions_sort_newest_first() {
    let mut index = CwdIndex::default();
    index.upsert(entry("a", "2026-01-01T00:00:00Z", Route::Claude, false));
    index.upsert(entry("b", "2026-03-01T00:00:00Z", Route::Claude, false));
    index.upsert(entry("c", "2026-02-01T00:00:00Z", Route::Claude, false));
    let order: Vec<_> = index
        .newest_first()
        .iter()
        .map(|s| s.session_id.as_str())
        .collect();
    assert_eq!(order, ["b", "c", "a"]);
}

/// The launch-recorded route is authoritative; a later legacy import that
/// guesses from a model name must not overwrite it, but the reverse must.
#[test]
fn authoritative_routes_are_never_overwritten_by_inferred_ones() {
    let mut index = CwdIndex::default();
    let codex = Route::ViaClaude("Codex".into());
    index.upsert(entry("s", "2026-01-01T00:00:00Z", codex.clone(), false));
    index.upsert(entry("s", "2026-01-02T00:00:00Z", Route::Claude, true));
    assert_eq!(index.find("s").unwrap().route, codex);
    assert_eq!(
        index.find("s").unwrap().last_activity.as_deref(),
        Some("2026-01-02T00:00:00Z")
    );

    let mut legacy = CwdIndex::default();
    legacy.upsert(entry("s", "2026-01-01T00:00:00Z", Route::Claude, true));
    legacy.upsert(entry("s", "2026-01-01T00:00:00Z", codex.clone(), false));
    let found = legacy.find("s").unwrap();
    assert_eq!(found.route, codex);
    assert!(!found.route_inferred);
}

#[test]
fn update_round_trips_under_the_lock_and_isolates_cwds() {
    let state = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    update(state.path(), "/work/a", |index| {
        index.upsert(entry("s1", "2026-01-01T00:00:00Z", Route::Claude, false));
    })
    .unwrap();
    update(state.path(), "/work/b", |index| {
        index.upsert(entry("s2", "2026-01-01T00:00:00Z", Route::Claude, false));
    })
    .unwrap();
    let a = read(state.path(), "/work/a");
    assert_eq!(a.sessions.len(), 1);
    assert_eq!(a.sessions[0].session_id, "s1");
    assert_eq!(read(state.path(), "/work/b").sessions[0].session_id, "s2");
}

#[test]
fn a_corrupt_index_reads_as_empty_and_is_rebuilt() {
    let state = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let path = index_path(state.path(), "/work/a");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, b"{not json").unwrap();
    assert!(read(state.path(), "/work/a").sessions.is_empty());
    update(state.path(), "/work/a", |index| {
        index.upsert(entry("s1", "2026-01-01T00:00:00Z", Route::Claude, false));
    })
    .unwrap();
    assert_eq!(read(state.path(), "/work/a").sessions.len(), 1);
}

#[test]
fn concurrent_updates_do_not_lose_writes() {
    let state = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let dir = state.path().to_path_buf();
    let handles: Vec<_> = (0..8)
        .map(|i| {
            let dir = dir.clone();
            std::thread::spawn(move || {
                update(&dir, "/work/a", |index| {
                    index.upsert(entry(
                        &format!("s{i}"),
                        "2026-01-01T00:00:00Z",
                        Route::Claude,
                        false,
                    ));
                })
                .unwrap();
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap();
    }
    assert_eq!(read(&dir, "/work/a").sessions.len(), 8);
}

#[test]
fn route_labels_name_the_provider() {
    assert_eq!(Route::Claude.label(), "Claude");
    assert_eq!(
        Route::ViaClaude("DeepSeek".into()).label(),
        "DeepSeek via Claude"
    );
}

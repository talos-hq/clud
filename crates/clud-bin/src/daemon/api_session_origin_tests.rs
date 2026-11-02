//! `SessionOrigin` provenance and the idle-is-never-reaped GC invariant.
//! See `docs/architecture/session-origin.md`.

use std::fs;
use std::path::Path;

use tempfile::TempDir;

use super::api_sessions::{
    ApiSessionBackend, ApiSessionState, ApiSessionStore, CreateApiSession,
    ResolvedApiSessionSettings,
};
use super::paths::api_session_path;
use crate::session_origin::SessionOrigin;

fn request(cwd: &Path, origin: SessionOrigin) -> CreateApiSession {
    CreateApiSession {
        backend: ApiSessionBackend::Claude,
        cwd: cwd.to_path_buf(),
        name: None,
        origin,
        resolved_settings: ResolvedApiSessionSettings {
            model: None,
            safe: true,
            model_provider: None,
            harness: None,
            routing_mode: None,
        },
    }
}

fn force_state(store: &ApiSessionStore, id: &str, state: ApiSessionState) {
    store
        .mutate(id, |record| {
            record.state = state;
            Ok(())
        })
        .unwrap();
}

#[test]
fn every_origin_is_stored_on_the_record_and_read_back() {
    let temp = TempDir::new().unwrap();
    let store = ApiSessionStore::new(temp.path());
    for origin in SessionOrigin::ALL {
        let created = store.create(request(temp.path(), origin)).unwrap();
        assert_eq!(created.origin, origin);
        let stored = store.get(&created.id).unwrap();
        assert_eq!(stored.origin, origin, "stored record for {origin:?}");
        let raw: serde_json::Value =
            serde_json::from_slice(&fs::read(api_session_path(temp.path(), &created.id)).unwrap())
                .unwrap();
        assert_eq!(raw["origin"], origin.as_str());
    }
}

#[test]
fn legacy_record_without_origin_loads_as_unknown_and_lists() {
    let temp = TempDir::new().unwrap();
    let store = ApiSessionStore::new(temp.path());
    let created = store
        .create(request(temp.path(), SessionOrigin::Cli))
        .unwrap();
    let path = api_session_path(temp.path(), &created.id);
    let mut raw: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    raw.as_object_mut().unwrap().remove("origin");
    fs::write(&path, serde_json::to_vec(&raw).unwrap()).unwrap();

    assert_eq!(
        store.get(&created.id).unwrap().origin,
        SessionOrigin::Unknown
    );
    let listed = store.list().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].origin, SessionOrigin::Unknown);
}

/// Table-driven over every state: GC reaps only finished sessions.
#[test]
fn gc_reaps_only_terminated_and_failed_states() {
    for state in ApiSessionState::ALL {
        let expected = matches!(state, ApiSessionState::Terminated | ApiSessionState::Failed);
        assert_eq!(state.is_reapable(), expected, "{state:?}");
    }
}

#[test]
fn idle_cli_session_survives_gc_and_terminated_one_is_reaped() {
    let temp = TempDir::new().unwrap();
    let store = ApiSessionStore::new(temp.path());
    let idle = store
        .create(request(temp.path(), SessionOrigin::Cli))
        .unwrap();
    force_state(&store, &idle.id, ApiSessionState::Idle);
    let dead = store
        .create(request(temp.path(), SessionOrigin::Cli))
        .unwrap();
    store.terminate(&dead.id).unwrap();

    let reaped = store.gc().unwrap();

    assert_eq!(reaped, vec![dead.id.clone()]);
    let survivor = store.get(&idle.id).unwrap();
    assert_eq!(survivor.state, ApiSessionState::Idle);
    assert_eq!(survivor.origin, SessionOrigin::Cli);
    assert!(store.get(&dead.id).is_err());
    assert!(!api_session_path(temp.path(), &dead.id).exists());
}

#[test]
fn origin_filter_selects_only_matching_sessions() {
    let temp = TempDir::new().unwrap();
    let store = ApiSessionStore::new(temp.path());
    for origin in SessionOrigin::ALL {
        store.create(request(temp.path(), origin)).unwrap();
    }
    for origin in SessionOrigin::ALL {
        let rows = super::commands::api_list_rows(&store.list().unwrap(), Some(origin));
        assert_eq!(rows.len(), 1, "{origin:?}");
        assert_eq!(rows[0]["origin"], origin.as_str());
    }
    let all = super::commands::api_list_rows(&store.list().unwrap(), None);
    assert_eq!(all.len(), SessionOrigin::ALL.len());
    assert!(all.iter().all(|row| row.get("origin").is_some()));
}

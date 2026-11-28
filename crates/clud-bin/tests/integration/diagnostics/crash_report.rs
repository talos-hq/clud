//! Integration test for the clud crash-report writer.
//!
//! Installs the panic hook with a temp `~/.clud/state` redirected via
//! `CLUD_DAEMON_STATE_DIR`, forces a panic via `catch_unwind`, and asserts
//! the JSON record landed with the expected role and a non-empty backtrace.
//!
//! `std::panic::set_hook` is process-global, so we serialize across in-file
//! tests with a Mutex. Other integration test files get their own test
//! binary (separate process), so their hook installs don't collide with
//! this file's.

use std::fs;
use std::sync::Mutex;

use kernal_api::platform::fs::TemporaryDirectory;

static SERIALIZE: Mutex<()> = Mutex::new(());

#[test]
fn panic_writes_a_crash_report_with_role_and_backtrace() {
    let _g = SERIALIZE.lock().unwrap();

    let temp = TemporaryDirectory::new().expect("tempdir");
    let state_dir = temp.path().join("state");
    // Redirect ~/.clud/state to our tempdir via the public env var.
    // SAFETY: `set_var` is safe on Edition 2021; this test is single-threaded
    // because we hold the SERIALIZE mutex.
    std::env::set_var("CLUD_DAEMON_STATE_DIR", &state_dir);

    clud::crash_report::install("test");

    // Note: `install()`'s OnceLock-guarded first-call block may have
    // already fired in a previous test (tests in this file share a
    // process). The crashes_dir for THIS tempdir is created lazily by
    // the panic hook's write path, so we don't assert existence
    // pre-panic.
    let crashes_dir = state_dir.join("crashes");

    let panic_outcome = std::panic::catch_unwind(|| {
        panic!("intentional test panic for crash_report integration");
    });
    assert!(panic_outcome.is_err(), "the panic should propagate as Err");

    assert!(
        crashes_dir.exists(),
        "panic hook should have created crashes_dir lazily at {}",
        crashes_dir.display()
    );

    let entries: Vec<_> = fs::read_dir(&crashes_dir)
        .expect("read crashes_dir")
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("json"))
        .collect();
    assert_eq!(
        entries.len(),
        1,
        "exactly one report should have been written"
    );

    let raw = fs::read_to_string(entries[0].path()).expect("read report");
    let json: serde_json::Value = serde_json::from_str(&raw).expect("report is valid JSON");

    assert_eq!(json["role"], "test");
    assert_eq!(json["pid"], serde_json::Value::from(std::process::id()));
    assert!(
        json["panic_message"]
            .as_str()
            .unwrap_or("")
            .contains("intentional test panic"),
        "panic_message should include the panic string, got {:?}",
        json["panic_message"]
    );
    assert!(
        json["panic_location"]
            .as_str()
            .unwrap_or("")
            .contains("crash_report.rs"),
        "panic_location should point at this test file, got {:?}",
        json["panic_location"]
    );
    assert!(
        !json["backtrace"].as_str().unwrap_or("").is_empty(),
        "backtrace should be non-empty"
    );
    assert!(
        !json["version"].as_str().unwrap_or("").is_empty(),
        "version should be present"
    );
}

#[test]
fn install_is_idempotent_and_retag_updates_role() {
    let _g = SERIALIZE.lock().unwrap();

    let temp = TemporaryDirectory::new().expect("tempdir");
    let state_dir = temp.path().join("state");
    std::env::set_var("CLUD_DAEMON_STATE_DIR", &state_dir);

    // First install — may already have happened in the prior test; harmless.
    clud::crash_report::install("foreground");
    // Retag — second install MUST NOT chain a second hook, otherwise a
    // panic would write two reports.
    clud::crash_report::install("daemon");

    let panic_outcome = std::panic::catch_unwind(|| {
        panic!("retag-check panic");
    });
    assert!(panic_outcome.is_err());

    let crashes_dir = state_dir.join("crashes");
    let entries: Vec<_> = fs::read_dir(&crashes_dir)
        .expect("read crashes_dir")
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("json"))
        .collect();
    assert_eq!(
        entries.len(),
        1,
        "double install should still write exactly one report per panic"
    );

    let raw = fs::read_to_string(entries[0].path()).expect("read report");
    let json: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON");
    assert_eq!(
        json["role"], "daemon",
        "role should reflect the most-recent install() call"
    );
}

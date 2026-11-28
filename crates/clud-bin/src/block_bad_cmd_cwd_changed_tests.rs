//! Tests for the `CwdChanged` backstop handler (#967 Phase 5, #966 D12).

use super::*;
use kernal_api::platform::fs::TemporaryDirectory;

/// Serializes the tests that mutate process-global env (`HOME`,
/// `CLAUDE_PROJECT_DIR`).
///
/// Crate-shared, not file-private: `gc::session_tmp` rewrites `HOME` too, and
/// a lock per file serializes each file against itself while both share one
/// process environment. See `crate::test_env`.
fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    crate::test_env::home_lock()
}

struct EnvGuard {
    old: Vec<(&'static str, Option<std::ffi::OsString>)>,
}

impl EnvGuard {
    fn set(vars: &[(&'static str, PathBuf)]) -> Self {
        // `home_dir()` on Windows resolves USERPROFILE, not HOME; mirror
        // HOME so the handler's log lands in the isolated home on every OS.
        let mut names: Vec<&'static str> = vars.iter().map(|(name, _)| *name).collect();
        if vars.iter().any(|(name, _)| *name == "HOME") {
            names.push("USERPROFILE");
        }
        let old = names
            .iter()
            .map(|name| (*name, std::env::var_os(name)))
            .collect();
        for (name, value) in vars {
            std::env::set_var(name, value);
        }
        if let Some((_, home)) = vars.iter().find(|(name, _)| *name == "HOME") {
            std::env::set_var("USERPROFILE", home);
        }
        Self { old }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (name, value) in &self.old {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
}

/// A tempdir holding a bare fixture repo (git marker + `src/` subdir) plus
/// an isolated `home/` for the hook log and the frontend-settings scan.
fn fixture() -> (kernal_api::platform::fs::TemporaryDirectory, PathBuf) {
    let dir = TemporaryDirectory::new().expect("tempdir");
    let repo = dir.path().join("repo");
    for sub in [".clud", ".git", "src"] {
        std::fs::create_dir_all(repo.join(sub)).unwrap();
    }
    (dir, repo)
}

fn write_hooks_json(repo: &Path, command: &str) {
    let text = format!(r#"{{"hooks":{{"CwdChanged":[{{"command":{command:?}}}]}}}}"#);
    std::fs::write(repo.join(".clud").join("hooks.json"), text).unwrap();
}

fn move_payload(new_cwd: &Path) -> String {
    format!(
        r#"{{"hook_event_name":"CwdChanged","session_id":"s1","old_cwd":"/start","cwd":{:?},"new_cwd":{:?},"transcript_path":"/t.jsonl"}}"#,
        new_cwd.to_string_lossy(),
        new_cwd.to_string_lossy(),
    )
}

fn hook_log(home: &Path) -> String {
    std::fs::read_to_string(home.join(".clud/tools/hooks/block-bad-cmd.log")).unwrap_or_default()
}

#[test]
fn cwd_from_payload_prefers_new_cwd_then_cwd_then_process_cwd() {
    let value: Value = serde_json::from_str(r#"{"new_cwd":"/a","cwd":"/b"}"#).unwrap();
    assert_eq!(
        cwd_from_payload(&value, Path::new("/proc")),
        PathBuf::from("/a")
    );

    let value: Value = serde_json::from_str(r#"{"cwd":"/b"}"#).unwrap();
    assert_eq!(
        cwd_from_payload(&value, Path::new("/proc")),
        PathBuf::from("/b")
    );

    let value: Value = serde_json::from_str("{}").unwrap();
    assert_eq!(
        cwd_from_payload(&value, Path::new("/proc")),
        PathBuf::from("/proc")
    );

    let value: Value = serde_json::from_str(r#"{"new_cwd":"","cwd":"  "}"#).unwrap();
    assert_eq!(
        cwd_from_payload(&value, Path::new("/proc")),
        PathBuf::from("/proc")
    );
}

#[test]
fn a_garbage_or_empty_payload_still_exits_zero() {
    // The backstop is hygiene: no payload shape may turn it into a wall.
    let _lock = env_lock();
    let (dir, _repo) = fixture();
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let nowhere = dir.path().join("nowhere");
    std::fs::create_dir_all(&nowhere).unwrap();
    let _guard = EnvGuard::set(&[("HOME", home.clone()), ("CLAUDE_PROJECT_DIR", nowhere)]);

    assert_eq!(handle_cwd_changed("not json", Some(&home)), 0);
    assert_eq!(handle_cwd_changed("", Some(&home)), 0);
    assert_eq!(handle_cwd_changed(r#"{"new_cwd": 7}"#, Some(&home)), 0);
    assert_eq!(handle_cwd_changed(r#"{"new_cwd": 7}"#, None), 0);
}

#[test]
fn drift_out_of_the_registered_roots_warns() {
    // A migrated repo resolves "auto" to relaxed; a chdir that escapes the
    // registered roots is exactly the drift the PreToolUse scanner cannot
    // see, so the backstop flags it — while still exiting 0.
    let _lock = env_lock();
    let (dir, repo) = fixture();
    write_hooks_json(&repo, "touch marker");
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let outside = dir.path().join("elsewhere");
    std::fs::create_dir_all(&outside).unwrap();
    let _guard = EnvGuard::set(&[("HOME", home.clone()), ("CLAUDE_PROJECT_DIR", repo.clone())]);

    let code = handle_cwd_changed(&move_payload(&outside), Some(&repo));

    assert_eq!(code, 0);
    let log = hook_log(&home);
    assert!(
        log.contains("cwd_changed_drift_check policy=Relaxed"),
        "log: {log}"
    );
    assert!(
        log.contains("cwd_changed_drift_warning_emitted"),
        "log: {log}"
    );
}

#[test]
fn a_strict_repo_also_warns_on_drift_outside_the_registered_roots() {
    // A cwd-sensitive raw hook in the frontend settings keeps the repo
    // strict; the backstop still flags a chdir the scanner could not see.
    let _lock = env_lock();
    let (dir, repo) = fixture();
    std::fs::create_dir_all(repo.join(".claude")).unwrap();
    std::fs::write(
        repo.join(".claude").join("settings.json"),
        r#"{"hooks":{"PreToolUse":[{"matcher":"*","hooks":[{"type":"command","command":"uv run python ci/hooks/check.py"}]}]}}"#,
    )
    .unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let outside = dir.path().join("elsewhere");
    std::fs::create_dir_all(&outside).unwrap();
    let _guard = EnvGuard::set(&[("HOME", home.clone()), ("CLAUDE_PROJECT_DIR", repo.clone())]);

    let code = handle_cwd_changed(&move_payload(&outside), Some(&repo));

    assert_eq!(code, 0);
    let log = hook_log(&home);
    assert!(
        log.contains("cwd_changed_drift_check policy=Strict"),
        "log: {log}"
    );
    assert!(
        log.contains("cwd_changed_drift_warning_emitted"),
        "log: {log}"
    );
}

#[test]
fn a_move_within_the_registered_roots_does_not_warn() {
    // Relaxed allows `cd` freely inside the registered trees; a move to a
    // subdirectory is the normal case and must stay silent.
    let _lock = env_lock();
    let (dir, repo) = fixture();
    write_hooks_json(&repo, "touch marker");
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let _guard = EnvGuard::set(&[("HOME", home.clone()), ("CLAUDE_PROJECT_DIR", repo.clone())]);

    let subdir = repo.join("src");
    let code = handle_cwd_changed(&move_payload(&subdir), Some(&repo));

    assert_eq!(code, 0);
    let log = hook_log(&home);
    assert!(
        !log.contains("cwd_changed_drift_warning_emitted"),
        "log: {log}"
    );
}

#[test]
fn block_cd_false_silences_the_backstop() {
    // bash.block_cd=false opts out of pinning entirely; the backstop must
    // not second-guess that choice.
    let _lock = env_lock();
    let (dir, repo) = fixture();
    std::fs::create_dir_all(repo.join(".claude")).unwrap();
    std::fs::write(
        repo.join(".claude").join("settings.json"),
        r#"{"bash":{"block_cd":false}}"#,
    )
    .unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let outside = dir.path().join("elsewhere");
    std::fs::create_dir_all(&outside).unwrap();
    let _guard = EnvGuard::set(&[("HOME", home.clone()), ("CLAUDE_PROJECT_DIR", repo.clone())]);

    let code = handle_cwd_changed(&move_payload(&outside), Some(&repo));

    assert_eq!(code, 0);
    let log = hook_log(&home);
    assert!(
        !log.contains("cwd_changed_drift_warning_emitted"),
        "log: {log}"
    );
}

#[test]
fn a_declared_cwd_changed_hook_runs_rooted_at_the_repo() {
    // The payload carries no command text — the session *moved* — so Tier B
    // keys on the new cwd, and the hook runs rooted at the repo it belongs
    // to, exactly the rooting contract every declared hook gets.
    let _lock = env_lock();
    let (dir, repo) = fixture();
    write_hooks_json(&repo, "touch marker");
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let _guard = EnvGuard::set(&[("HOME", home.clone()), ("CLAUDE_PROJECT_DIR", repo.clone())]);

    let subdir = repo.join("src");
    let code = handle_cwd_changed(&move_payload(&subdir), Some(&repo));

    assert_eq!(code, 0);
    assert!(repo.join("marker").exists(), "hook ran rooted at the repo");
}

#[test]
fn a_refusing_cwd_changed_hook_is_downgraded_to_a_warning() {
    // The cwd has already changed and the harness gives this event no
    // decision control, so an exit 2 cannot be honored — surfacing it as a
    // wall would be a denial clud cannot enforce.
    let _lock = env_lock();
    let (dir, repo) = fixture();
    write_hooks_json(&repo, "exit 2");
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let _guard = EnvGuard::set(&[("HOME", home.clone()), ("CLAUDE_PROJECT_DIR", repo.clone())]);

    let subdir = repo.join("src");
    let code = handle_cwd_changed(&move_payload(&subdir), Some(&repo));

    assert_eq!(
        code, 0,
        "a CwdChanged refusal cannot be enforced, so it must not block"
    );
    let log = hook_log(&home);
    assert!(
        log.contains("cwd_changed_denial_not_enforceable"),
        "log: {log}"
    );
}

#[test]
fn session_parent_root_prefers_claude_project_dir() {
    let _lock = env_lock();
    let (_dir, repo) = fixture();
    let _guard = EnvGuard::set(&[("CLAUDE_PROJECT_DIR", repo.clone())]);

    assert_eq!(session_parent_root().as_deref(), Some(repo.as_path()));
}

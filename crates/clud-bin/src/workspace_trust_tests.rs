use super::*;
use kernal_api::platform::fs::TemporaryDirectory;
use std::fs;

/// Write a `~/.claude.json`-shaped state file.
fn write_state(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
}

/// Write a state file whose `projects` map holds one entry for `project`.
///
/// Built through `serde_json` rather than `format!` on purpose: a Windows path
/// is full of backslashes, and pasting one straight into a JSON string literal
/// produces invalid escapes (`C:\Users` carries `\U`). That parses as an
/// error, `read_workspace_trust` answers `Unknown`, and a test asserting
/// `Untrusted` then passes for entirely the wrong reason. Escaping the key
/// properly is also what makes these the real Windows path-matching tests they
/// claim to be.
fn write_project_state(path: &Path, project: &Path, accepted: bool) {
    let mut projects = serde_json::Map::new();
    projects.insert(
        project.to_string_lossy().into_owned(),
        serde_json::json!({ TRUST_KEY: accepted }),
    );
    let document = serde_json::json!({ "projects": serde_json::Value::Object(projects) });
    fs::write(path, serde_json::to_string(&document).unwrap()).unwrap();
}

/// Give a temp dir a `.claude/settings.local.json` so it looks like a repo
/// whose settings the trust decision would actually suppress.
fn with_local_settings(cwd: &Path) -> PathBuf {
    let dir = cwd.join(".claude");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("settings.local.json");
    fs::write(&path, r#"{"permissions":{"allow":["Bash(ls:*)"]}}"#).unwrap();
    path
}

#[test]
fn accepted_trust_dialog_reads_as_trusted() {
    let tmp = TemporaryDirectory::new().unwrap();
    let cwd = tmp.path().join("repo");
    fs::create_dir_all(&cwd).unwrap();
    let state = tmp.path().join(".claude.json");
    write_project_state(&state, &cwd, true);

    assert_eq!(read_workspace_trust(&state, &cwd), WorkspaceTrust::Trusted);
}

#[test]
fn project_present_but_not_accepted_reads_as_untrusted() {
    let tmp = TemporaryDirectory::new().unwrap();
    let cwd = tmp.path().join("repo");
    fs::create_dir_all(&cwd).unwrap();
    let state = tmp.path().join(".claude.json");
    write_project_state(&state, &cwd, false);

    assert_eq!(
        read_workspace_trust(&state, &cwd),
        WorkspaceTrust::Untrusted
    );
}

#[test]
fn project_absent_from_state_reads_as_untrusted() {
    // This is the reported case: grind ran in a directory the harness had
    // never been opened in interactively, so there was no entry at all.
    let tmp = TemporaryDirectory::new().unwrap();
    let cwd = tmp.path().join("repo");
    fs::create_dir_all(&cwd).unwrap();
    let state = tmp.path().join(".claude.json");
    write_state(
        &state,
        r#"{"projects":{"/somewhere/else":{"hasTrustDialogAccepted":true}}}"#,
    );

    assert_eq!(
        read_workspace_trust(&state, &cwd),
        WorkspaceTrust::Untrusted
    );
}

#[test]
fn state_file_without_projects_map_reads_as_untrusted() {
    let tmp = TemporaryDirectory::new().unwrap();
    let state = tmp.path().join(".claude.json");
    write_state(&state, r#"{"numStartups":3}"#);

    assert_eq!(
        read_workspace_trust(&state, tmp.path()),
        WorkspaceTrust::Untrusted
    );
}

#[test]
fn missing_or_corrupt_state_file_reads_as_unknown() {
    // Never warn on a guess: a fresh machine, a relocated config root, and a
    // half-written file all have to stay silent rather than tell a user
    // their trusted workspace is untrusted.
    let tmp = TemporaryDirectory::new().unwrap();
    let missing = tmp.path().join("nope.json");
    assert_eq!(
        read_workspace_trust(&missing, tmp.path()),
        WorkspaceTrust::Unknown
    );

    let corrupt = tmp.path().join(".claude.json");
    write_state(&corrupt, "{not json");
    assert_eq!(
        read_workspace_trust(&corrupt, tmp.path()),
        WorkspaceTrust::Unknown
    );
}

#[test]
fn non_canonical_cwd_still_matches_its_project_entry() {
    // The harness stores the path it resolved; clud may be handed a
    // `.`-laden or symlinked spelling of the same directory. A string
    // compare would miss and warn a user whose workspace is trusted.
    let tmp = TemporaryDirectory::new().unwrap();
    let cwd = tmp.path().join("repo");
    fs::create_dir_all(cwd.join("sub")).unwrap();
    let canonical = fs::canonicalize(&cwd).unwrap();
    let state = tmp.path().join(".claude.json");
    write_project_state(&state, &canonical, true);

    let noisy = cwd.join("sub").join("..");
    assert_eq!(
        read_workspace_trust(&state, &noisy),
        WorkspaceTrust::Trusted
    );
}

#[test]
fn settings_probe_finds_local_then_tracked_then_nothing() {
    let tmp = TemporaryDirectory::new().unwrap();
    // No .claude dir at all: nothing for the trust decision to suppress.
    assert_eq!(project_settings_file(tmp.path()), None);

    // Tracked settings.json only.
    let dir = tmp.path().join(".claude");
    fs::create_dir_all(&dir).unwrap();
    let tracked = dir.join("settings.json");
    fs::write(&tracked, "{}").unwrap();
    assert_eq!(project_settings_file(tmp.path()), Some(tracked));

    // settings.local.json wins — it is the file the harness names in its
    // own banner.
    let local = with_local_settings(tmp.path());
    assert_eq!(project_settings_file(tmp.path()), Some(local));
}

#[test]
fn notice_names_the_file_and_the_fix() {
    let tmp = TemporaryDirectory::new().unwrap();
    let settings = with_local_settings(tmp.path());

    let notice = untrusted_workspace_notice(&settings, 200);
    assert!(notice.contains("settings.local.json"), "{notice}");
    assert!(notice.contains("accept the trust prompt"), "{notice}");
    // A grind run has to say how long it will be wrong for.
    assert!(
        notice.contains("all 200 iterations of this run"),
        "{notice}"
    );
}

#[test]
fn notice_never_tells_the_user_to_hand_edit_trust() {
    // Trust is a security boundary. The notice points at the interactive
    // prompt; it must not coach anyone into flipping the flag by hand, and
    // clud must never write it for them.
    let tmp = TemporaryDirectory::new().unwrap();
    let settings = with_local_settings(tmp.path());
    let notice = untrusted_workspace_notice(&settings, 5);

    assert!(!notice.contains(TRUST_KEY), "{notice}");
    assert!(!notice.contains(CLAUDE_JSON), "{notice}");
}

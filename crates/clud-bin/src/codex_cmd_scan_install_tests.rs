use super::*;
use kernal_api::platform::fs::TemporaryDirectory;
use serde_json::json;
use std::path::{Path, PathBuf};

fn layout(root: &Path) -> (PathBuf, PathBuf) {
    (root.join(".clud"), root.join(".codex"))
}

fn run(clud_dir: &Path, codex_dir: &Path) -> (InstallOutcome, String) {
    let mut buf: Vec<u8> = Vec::new();
    let outcome = run_at(clud_dir, codex_dir, &mut buf, true).expect("run_at");
    (outcome, String::from_utf8(buf).unwrap())
}

fn read_json(path: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn scan_group() -> Value {
    json!({"matcher": "*", "hooks": [{"type": "command", "command": "clud-cmd-scan"}]})
}

fn trusted_hash(codex_dir: &Path, key: &str) -> Option<String> {
    let text = std::fs::read_to_string(codex_dir.join("config.toml")).ok()?;
    let doc = text.parse::<DocumentMut>().ok()?;
    doc.get("hooks")?
        .get("state")?
        .get(key)?
        .get("trusted_hash")?
        .as_str()
        .map(str::to_string)
}

fn state_key(codex_dir: &Path, group: usize) -> String {
    format!(
        "{}:pre_tool_use:{group}:0",
        codex_dir.join("hooks.json").to_string_lossy()
    )
}

#[test]
fn missing_hooks_file_is_created_with_the_scan_entry_and_trust() {
    let tmp = TemporaryDirectory::new().unwrap();
    let (clud_dir, codex_dir) = layout(tmp.path());
    let (outcome, out) = run(&clud_dir, &codex_dir);
    assert!(outcome.hooks_written && outcome.trust_written, "{out}");
    let doc = read_json(&codex_dir.join("hooks.json"));
    assert_eq!(doc, json!({"hooks": {"PreToolUse": [scan_group()]}}));
    assert_eq!(
        trusted_hash(&codex_dir, &state_key(&codex_dir, 0)),
        Some(codex_hook_trust_hash("*", SCAN_COMMAND, 600))
    );
}

#[test]
fn user_hooks_and_unknown_fields_are_preserved_and_entry_appended() {
    let tmp = TemporaryDirectory::new().unwrap();
    let (clud_dir, codex_dir) = layout(tmp.path());
    std::fs::create_dir_all(&codex_dir).unwrap();
    let user = json!({
        "custom": {"keep": true},
        "hooks": {
            "PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "mine"}]}],
            "Stop": [{"hooks": [{"type": "command", "command": "bye"}]}]
        }
    });
    std::fs::write(codex_dir.join("hooks.json"), user.to_string()).unwrap();
    std::fs::write(codex_dir.join("config.toml"), "model = \"x\"\n").unwrap();
    let (outcome, _) = run(&clud_dir, &codex_dir);
    assert!(outcome.hooks_written);
    let doc = read_json(&codex_dir.join("hooks.json"));
    let mut expected = user.clone();
    expected["hooks"]["PreToolUse"]
        .as_array_mut()
        .unwrap()
        .push(scan_group());
    assert_eq!(doc, expected);
    let config = std::fs::read_to_string(codex_dir.join("config.toml")).unwrap();
    assert!(config.starts_with("model = \"x\"\n"), "{config}");
    assert!(trusted_hash(&codex_dir, &state_key(&codex_dir, 1)).is_some());
}

#[test]
fn already_installed_means_no_change_and_no_rewrite() {
    let tmp = TemporaryDirectory::new().unwrap();
    let (clud_dir, codex_dir) = layout(tmp.path());
    run(&clud_dir, &codex_dir);
    let hooks = codex_dir.join("hooks.json");
    let config = codex_dir.join("config.toml");
    let before = (
        std::fs::read_to_string(&hooks).unwrap(),
        std::fs::metadata(&hooks).unwrap().modified().unwrap(),
        std::fs::read_to_string(&config).unwrap(),
        std::fs::metadata(&config).unwrap().modified().unwrap(),
    );
    std::thread::sleep(std::time::Duration::from_millis(20));
    let (outcome, out) = run(&clud_dir, &codex_dir);
    assert_eq!(outcome, InstallOutcome::default());
    assert!(out.is_empty(), "{out}");
    let after = (
        std::fs::read_to_string(&hooks).unwrap(),
        std::fs::metadata(&hooks).unwrap().modified().unwrap(),
        std::fs::read_to_string(&config).unwrap(),
        std::fs::metadata(&config).unwrap().modified().unwrap(),
    );
    assert_eq!(before, after);
}

#[test]
fn existing_bash_scoped_scan_entry_is_not_duplicated() {
    let tmp = TemporaryDirectory::new().unwrap();
    let (clud_dir, codex_dir) = layout(tmp.path());
    std::fs::create_dir_all(&codex_dir).unwrap();
    let body = r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"clud-cmd-scan"}]}]}}"#;
    std::fs::write(codex_dir.join("hooks.json"), body).unwrap();
    let (outcome, _) = run(&clud_dir, &codex_dir);
    assert!(!outcome.hooks_written);
    assert_eq!(
        std::fs::read_to_string(codex_dir.join("hooks.json")).unwrap(),
        body
    );
    assert_eq!(
        trusted_hash(&codex_dir, &state_key(&codex_dir, 0)),
        Some(codex_hook_trust_hash("Bash", SCAN_COMMAND, 600))
    );
}

#[test]
fn malformed_json_is_left_untouched_and_logged() {
    let tmp = TemporaryDirectory::new().unwrap();
    let (clud_dir, codex_dir) = layout(tmp.path());
    std::fs::create_dir_all(&codex_dir).unwrap();
    let body = "{ this is not json";
    std::fs::write(codex_dir.join("hooks.json"), body).unwrap();
    let (outcome, out) = run(&clud_dir, &codex_dir);
    assert_eq!(outcome, InstallOutcome::default());
    assert!(out.contains("malformed JSON"), "{out}");
    assert_eq!(
        std::fs::read_to_string(codex_dir.join("hooks.json")).unwrap(),
        body
    );
    assert!(!codex_dir.join("config.toml").exists());
}

#[test]
fn malformed_toml_is_left_untouched_and_logged() {
    let tmp = TemporaryDirectory::new().unwrap();
    let (clud_dir, codex_dir) = layout(tmp.path());
    std::fs::create_dir_all(&codex_dir).unwrap();
    std::fs::write(codex_dir.join("config.toml"), "[[[ nope").unwrap();
    let (outcome, out) = run(&clud_dir, &codex_dir);
    assert!(outcome.hooks_written && !outcome.trust_written);
    assert!(out.contains("malformed TOML"), "{out}");
    assert_eq!(
        std::fs::read_to_string(codex_dir.join("config.toml")).unwrap(),
        "[[[ nope"
    );
}

#[test]
fn existing_trust_entry_is_never_overwritten() {
    let tmp = TemporaryDirectory::new().unwrap();
    let (clud_dir, codex_dir) = layout(tmp.path());
    std::fs::create_dir_all(&codex_dir).unwrap();
    let key = state_key(&codex_dir, 0);
    let config = format!(
        "[hooks.state.\"{}\"]\ntrusted_hash = \"user\"\n",
        key.replace('\\', "\\\\")
    );
    std::fs::write(codex_dir.join("config.toml"), &config).unwrap();
    let (outcome, _) = run(&clud_dir, &codex_dir);
    assert!(outcome.hooks_written && !outcome.trust_written);
    assert_eq!(trusted_hash(&codex_dir, &key).as_deref(), Some("user"));
}

#[test]
fn wrong_shape_is_left_untouched() {
    let tmp = TemporaryDirectory::new().unwrap();
    let (clud_dir, codex_dir) = layout(tmp.path());
    std::fs::create_dir_all(&codex_dir).unwrap();
    let body = r#"{"hooks":{"PreToolUse":{"not":"an array"}}}"#;
    std::fs::write(codex_dir.join("hooks.json"), body).unwrap();
    let (outcome, out) = run(&clud_dir, &codex_dir);
    assert_eq!(outcome, InstallOutcome::default());
    assert!(out.contains("unexpected shape"), "{out}");
    assert_eq!(
        std::fs::read_to_string(codex_dir.join("hooks.json")).unwrap(),
        body
    );
}

#[test]
fn settings_opt_out_skips_the_install() {
    let tmp = TemporaryDirectory::new().unwrap();
    let (clud_dir, codex_dir) = layout(tmp.path());
    std::fs::create_dir_all(&clud_dir).unwrap();
    let settings = json!({"hooks": {OPT_OUT_KEY: false}});
    std::fs::write(clud_dir.join(SETTINGS_FILE_NAME), settings.to_string()).unwrap();
    let (outcome, _) = run(&clud_dir, &codex_dir);
    assert!(outcome.opted_out);
    assert!(!codex_dir.join("hooks.json").exists());
}

#[test]
fn trust_hash_matches_the_session_flag_hash_clud_already_ships() {
    // Pins the canonical form command::builder has used for the `-c` hook.
    let hash = codex_hook_trust_hash("Bash", SCAN_COMMAND, 600);
    assert!(
        hash.starts_with("sha256:") && hash.len() == 7 + 64,
        "{hash}"
    );
    assert_ne!(hash, codex_hook_trust_hash("*", SCAN_COMMAND, 600));
}

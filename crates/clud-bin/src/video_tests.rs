use super::*;
use crate::provider_auth::SecretStoreError;
use std::cell::RefCell;

#[derive(Default)]
struct FakeStore {
    value: RefCell<Option<String>>,
}

impl SecretStore for FakeStore {
    fn get(&self) -> Result<Option<String>, SecretStoreError> {
        Ok(self.value.borrow().clone())
    }
    fn set(&self, secret: &str) -> Result<(), SecretStoreError> {
        *self.value.borrow_mut() = Some(secret.to_string());
        Ok(())
    }
    fn delete(&self) -> Result<(), SecretStoreError> {
        *self.value.borrow_mut() = None;
        Ok(())
    }
}

fn no_prompt() -> Result<String, ()> {
    panic!("must not prompt")
}

#[test]
fn video_path_reports_the_checkout_after_installing() {
    let home = Path::new("/h");
    let mut installs = 0;
    let path = checkout_for_path(home, &[], &mut |_| {
        installs += 1;
        Ok(PathBuf::from("/wrapper"))
    })
    .unwrap();
    assert_eq!(path, checkout_dir(home));
    assert_eq!(installs, 1);
}

#[test]
fn video_path_fails_actionably_without_media_tools_or_install() {
    let home = Path::new("/h");
    let error =
        checkout_for_path(home, &["ffmpeg"], &mut |_| panic!("must not install")).unwrap_err();
    assert!(error.contains("ffmpeg"), "{error}");
    let error =
        checkout_for_path(home, &[], &mut |_| Err("uv was not found on PATH".into())).unwrap_err();
    assert!(error.contains("uv was not found"), "{error}");
}

#[test]
fn video_paths_live_under_clud_extern_never_harness_skills() {
    let home = Path::new("/h");
    assert_eq!(
        checkout_dir(home),
        Path::new("/h/.clud/extern/video-use").join(VIDEO_USE_SHA)
    );
    assert_eq!(
        plugin_dir(home),
        Path::new("/h/.clud/extern/video-use-plugin")
    );
}

#[test]
fn video_pinned_sha_is_a_full_commit() {
    assert_eq!(VIDEO_USE_SHA.len(), 40);
    assert!(VIDEO_USE_SHA.chars().all(|c| c.is_ascii_hexdigit()));
}

#[test]
fn video_vault_identifiers_are_frozen() {
    assert_eq!(ELEVENLABS_VAULT_SERVICE, "clud.elevenlabs");
    assert_eq!(ELEVENLABS_VAULT_ACCOUNT, "api-key-v1");
}

#[test]
fn video_plugin_manifest_is_json_with_a_name() {
    let value: serde_json::Value = serde_json::from_str(&plugin_manifest()).unwrap();
    assert_eq!(value["name"], PLUGIN_NAME);
}

#[test]
fn video_plugin_wrapper_layout_and_no_harness_skill_writes() {
    let home = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let checkout = checkout_dir(home.path());
    std::fs::create_dir_all(checkout.join("helpers")).unwrap();
    std::fs::write(checkout.join("SKILL.md"), "---\nname: video-use\n---\n").unwrap();
    std::fs::write(checkout.join("helpers/render.py"), "").unwrap();

    let wrapper = write_plugin_wrapper(home.path(), &checkout).unwrap();
    assert_eq!(wrapper, plugin_dir(home.path()));
    assert!(wrapper.join(".claude-plugin/plugin.json").is_file());
    let skill = wrapper.join("skills").join(SKILL_NAME); // skill-source-lint: allow (plugin wrapper)
    assert!(skill.join("SKILL.md").is_file());
    assert!(
        skill.join("helpers/render.py").is_file(),
        "helpers stay siblings"
    );

    // Idempotent refresh.
    write_plugin_wrapper(home.path(), &checkout).unwrap();
    assert!(skill.join("SKILL.md").is_file());

    assert!(!home.path().join(".claude").exists());
    assert!(!home.path().join(".codex").exists());
}

#[test]
fn video_seed_prompt_starts_the_skill_and_waits_for_approval() {
    let prompt = seed_prompt(Path::new("/x/ck"));
    assert!(prompt.starts_with("/video-use "));
    assert!(prompt.contains("wait for my OK"));
    assert!(prompt.contains("/x/ck"));
}

#[test]
fn video_missing_ffmpeg_is_one_actionable_message() {
    let missing = missing_media_tools(&|tool| tool != "ffmpeg");
    assert_eq!(missing, vec!["ffmpeg"]);
    let message = missing_media_tools_message(&missing, "macos");
    assert!(message.contains("ffmpeg"));
    assert!(message.contains("brew install ffmpeg"));
    assert!(!message.contains('\n'));
    assert!(missing_media_tools(&|_| true).is_empty());
    assert!(missing_media_tools_message(&["ffprobe"], "linux").contains("ffprobe"));
}

#[test]
fn video_ambient_key_wins_over_vault() {
    let store = FakeStore::default();
    store.set("vault-key").unwrap();
    let got = resolve_key(Some("env-key".into()), &store, false, &mut no_prompt).unwrap();
    assert_eq!(got, ("env-key".to_string(), KeySource::Ambient));
}

#[test]
fn video_vault_key_used_without_prompt() {
    let store = FakeStore::default();
    store.set("vault-key").unwrap();
    let got = resolve_key(None, &store, true, &mut no_prompt).unwrap();
    assert_eq!(got, ("vault-key".to_string(), KeySource::Vault));
}

#[test]
fn video_missing_key_prompts_once_and_stores_it() {
    let store = FakeStore::default();
    let mut calls = 0;
    let got = resolve_key(Some("  ".into()), &store, true, &mut || {
        calls += 1;
        Ok("typed-key".to_string())
    })
    .unwrap();
    assert_eq!(calls, 1);
    assert_eq!(got.1, KeySource::Prompt);
    assert_eq!(store.get().unwrap().as_deref(), Some("typed-key"));
}

#[test]
fn video_missing_key_non_tty_fails_actionably() {
    let store = FakeStore::default();
    let error = resolve_key(None, &store, false, &mut no_prompt).unwrap_err();
    assert!(error.contains(ELEVENLABS_ENV));
    assert!(store.get().unwrap().is_none());
}

#[test]
fn video_malformed_prompted_key_is_rejected() {
    let store = FakeStore::default();
    let error = resolve_key(None, &store, true, &mut || Ok("a b".into())).unwrap_err();
    assert!(error.contains("whitespace"));
    assert!(store.get().unwrap().is_none());
}

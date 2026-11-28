use super::*;
use kernal_api::platform::fs::TemporaryDirectory;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

/// Convenience: build the `~/.clud` and `~/.codex/hooks.json` layout
/// inside `root` and return both paths.
fn layout(root: &Path) -> (PathBuf, PathBuf) {
    let clud_dir = root.join(".clud");
    let codex_dir = root.join(".codex");
    std::fs::create_dir_all(&codex_dir).unwrap();
    (clud_dir, codex_dir.join("hooks.json"))
}

/// Run `run_at` capturing stderr-equivalent output into a `Vec<u8>`.
fn run(clud_dir: &Path, hooks_path: &Path) -> (NormalizeOutcome, String) {
    let mut buf: Vec<u8> = Vec::new();
    let outcome = run_at(clud_dir, hooks_path, &mut buf, true).expect("run_at");
    (outcome, String::from_utf8(buf).unwrap())
}

#[test]
fn missing_clud_dir_is_created() {
    let tmp = TemporaryDirectory::new().unwrap();
    let (clud_dir, hooks_path) = layout(tmp.path());
    assert!(!clud_dir.exists());
    let (_, _) = run(&clud_dir, &hooks_path);
    assert!(clud_dir.is_dir(), "clud dir must be created");
}

#[test]
fn missing_settings_json_is_created() {
    let tmp = TemporaryDirectory::new().unwrap();
    let (clud_dir, hooks_path) = layout(tmp.path());
    let (_, _) = run(&clud_dir, &hooks_path);
    let settings = clud_dir.join(SETTINGS_FILE_NAME);
    assert!(settings.is_file(), "settings.json must be created");
    let body = std::fs::read_to_string(&settings).unwrap();
    let parsed: Value = serde_json::from_str(&body).unwrap();
    assert!(parsed.is_object(), "settings.json must hold a JSON object");
}

#[test]
fn existing_settings_json_is_left_alone() {
    let tmp = TemporaryDirectory::new().unwrap();
    let (clud_dir, hooks_path) = layout(tmp.path());
    std::fs::create_dir_all(&clud_dir).unwrap();
    let settings = clud_dir.join(SETTINGS_FILE_NAME);
    let user_body = r#"{"skills_version":"v9","agent_backend":"codex"}"#;
    std::fs::write(&settings, user_body).unwrap();

    let (_, _) = run(&clud_dir, &hooks_path);
    let after = std::fs::read_to_string(&settings).unwrap();
    assert_eq!(after, user_body, "must not touch existing settings.json");
}

#[test]
fn missing_hooks_json_is_noop_and_silent() {
    let tmp = TemporaryDirectory::new().unwrap();
    let (clud_dir, hooks_path) = layout(tmp.path());
    assert!(!hooks_path.exists());
    let (outcome, stderr) = run(&clud_dir, &hooks_path);
    assert_eq!(outcome.changed(), 0);
    assert!(
        !stderr.contains("updated Codex hook timeout"),
        "must stay quiet when no hook file"
    );
}

#[test]
fn timeout_five_is_rewritten_to_thirty() {
    let tmp = TemporaryDirectory::new().unwrap();
    let (clud_dir, hooks_path) = layout(tmp.path());
    let body = json!({
        "hooks": {
            "PreToolUse": [
                {
                    "matcher": "Bash",
                    "hooks": [
                        {"type": "command", "command": "do.sh", "timeout": 5},
                    ],
                },
            ],
        },
    });
    std::fs::write(&hooks_path, serde_json::to_string_pretty(&body).unwrap()).unwrap();

    let (outcome, stderr) = run(&clud_dir, &hooks_path);
    assert_eq!(outcome.changed(), 1);
    assert!(
        stderr.contains("updated Codex hook timeout: 5s -> 30s"),
        "green status line missing: {stderr}"
    );
    assert!(stderr.contains("\x1b[32m"), "must use green ANSI: {stderr}");

    let rewritten: Value = serde_json::from_str(&std::fs::read_to_string(&hooks_path).unwrap())
        .expect("rewritten hooks.json must still parse");
    let t = &rewritten["hooks"]["PreToolUse"][0]["hooks"][0]["timeout"];
    assert_eq!(t.as_u64(), Some(30), "got: {t}");
}

#[test]
fn timeout_above_five_is_left_unchanged() {
    let tmp = TemporaryDirectory::new().unwrap();
    let (clud_dir, hooks_path) = layout(tmp.path());
    let body = json!({
        "hooks": {
            "PreToolUse": [
                {
                    "matcher": "Bash",
                    "hooks": [
                        {"type": "command", "command": "a", "timeout": 10},
                        {"type": "command", "command": "b", "timeout": 30},
                        {"type": "command", "command": "c", "timeout": 60},
                        {"type": "command", "command": "d", "timeout": 6},
                    ],
                },
            ],
        },
    });
    std::fs::write(&hooks_path, serde_json::to_string_pretty(&body).unwrap()).unwrap();

    let (outcome, stderr) = run(&clud_dir, &hooks_path);
    assert_eq!(outcome.changed(), 0, "must not touch timeout > 5");
    assert!(
        !stderr.contains("updated Codex hook timeout"),
        "must stay quiet when nothing changes"
    );
}

#[test]
fn missing_timeout_is_left_unchanged() {
    let tmp = TemporaryDirectory::new().unwrap();
    let (clud_dir, hooks_path) = layout(tmp.path());
    let body = json!({
        "hooks": {
            "PreToolUse": [
                {
                    "matcher": "Bash",
                    "hooks": [
                        {"type": "command", "command": "a"},
                    ],
                },
            ],
        },
    });
    let raw = serde_json::to_string_pretty(&body).unwrap();
    std::fs::write(&hooks_path, &raw).unwrap();

    let (outcome, stderr) = run(&clud_dir, &hooks_path);
    assert_eq!(outcome.changed(), 0);
    assert!(!stderr.contains("updated Codex hook timeout"));
    // File untouched.
    assert_eq!(std::fs::read_to_string(&hooks_path).unwrap(), raw);
}

#[test]
fn float_timeout_five_point_zero_is_left_unchanged() {
    // Codex stores timeouts as integers, but a hand-edited 5.0 must NOT
    // be misclassified as a 5 — we only target the exact integer 5.
    let tmp = TemporaryDirectory::new().unwrap();
    let (clud_dir, hooks_path) = layout(tmp.path());
    let raw = r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"a","timeout":5.0}]}]}}"#;
    std::fs::write(&hooks_path, raw).unwrap();

    let (outcome, _) = run(&clud_dir, &hooks_path);
    assert_eq!(outcome.changed(), 0, "5.0 must not be touched");
    let on_disk: Value =
        serde_json::from_str(&std::fs::read_to_string(&hooks_path).unwrap()).unwrap();
    let t = &on_disk["hooks"]["PreToolUse"][0]["hooks"][0]["timeout"];
    assert!(t.is_f64(), "timeout must remain a float, got {t}");
}

#[test]
fn malformed_hooks_json_does_not_block_launch() {
    let tmp = TemporaryDirectory::new().unwrap();
    let (clud_dir, hooks_path) = layout(tmp.path());
    let bad = "{ not real json";
    std::fs::write(&hooks_path, bad).unwrap();

    let (outcome, stderr) = run(&clud_dir, &hooks_path);
    assert_eq!(outcome.changed(), 0);
    assert!(
        stderr.contains("malformed JSON"),
        "verbose warning missing: {stderr}"
    );
    // File left untouched.
    assert_eq!(std::fs::read_to_string(&hooks_path).unwrap(), bad);
}

#[test]
fn multiple_timeouts_are_all_normalized() {
    let tmp = TemporaryDirectory::new().unwrap();
    let (clud_dir, hooks_path) = layout(tmp.path());
    let body = json!({
        "hooks": {
            "PreToolUse": [
                {
                    "matcher": "Bash",
                    "hooks": [
                        {"type": "command", "command": "a", "timeout": 5},
                        {"type": "command", "command": "b", "timeout": 5},
                        {"type": "command", "command": "c", "timeout": 30},
                    ],
                },
                {
                    "matcher": "Write",
                    "hooks": [
                        {"type": "command", "command": "d", "timeout": 5},
                    ],
                },
            ],
        },
    });
    std::fs::write(&hooks_path, serde_json::to_string_pretty(&body).unwrap()).unwrap();

    let (outcome, stderr) = run(&clud_dir, &hooks_path);
    assert_eq!(outcome.changed(), 3);
    assert!(stderr.contains("3 hooks"), "count missing: {stderr}");
    let v: Value = serde_json::from_str(&std::fs::read_to_string(&hooks_path).unwrap()).unwrap();
    let group0 = &v["hooks"]["PreToolUse"][0]["hooks"];
    assert_eq!(group0[0]["timeout"].as_u64(), Some(30));
    assert_eq!(group0[1]["timeout"].as_u64(), Some(30));
    assert_eq!(group0[2]["timeout"].as_u64(), Some(30));
    let group1 = &v["hooks"]["PreToolUse"][1]["hooks"];
    assert_eq!(group1[0]["timeout"].as_u64(), Some(30));
}

#[test]
fn rerun_after_change_is_idempotent_silent() {
    let tmp = TemporaryDirectory::new().unwrap();
    let (clud_dir, hooks_path) = layout(tmp.path());
    let body = json!({
        "hooks": {
            "PreToolUse": [
                {
                    "matcher": "Bash",
                    "hooks": [{"type": "command", "command": "a", "timeout": 5}],
                },
            ],
        },
    });
    std::fs::write(&hooks_path, serde_json::to_string_pretty(&body).unwrap()).unwrap();

    let (first, first_out) = run(&clud_dir, &hooks_path);
    assert_eq!(first.changed(), 1);
    assert!(first_out.contains("updated Codex hook timeout"));

    let (second, second_out) = run(&clud_dir, &hooks_path);
    assert_eq!(second.changed(), 0);
    assert!(
        !second_out.contains("updated Codex hook timeout"),
        "second run must stay quiet: {second_out}"
    );
}

#[test]
fn repair_after_regression_back_to_five() {
    // The pass is not a one-time migration: if a Codex update writes a
    // 5 back in, the next eligible launch should upgrade it again.
    let tmp = TemporaryDirectory::new().unwrap();
    let (clud_dir, hooks_path) = layout(tmp.path());
    let body = json!({
        "hooks": {
            "PreToolUse": [{"matcher":"Bash","hooks":[{"type":"command","command":"a","timeout":5}]}],
        },
    });
    std::fs::write(&hooks_path, serde_json::to_string_pretty(&body).unwrap()).unwrap();

    let (first, _) = run(&clud_dir, &hooks_path);
    assert_eq!(first.changed(), 1);

    // Pretend the next Codex upgrade wrote 5 back.
    let regressed = json!({
        "hooks": {
            "PreToolUse": [{"matcher":"Bash","hooks":[{"type":"command","command":"a","timeout":5}]}],
        },
    });
    std::fs::write(
        &hooks_path,
        serde_json::to_string_pretty(&regressed).unwrap(),
    )
    .unwrap();

    let (again, again_out) = run(&clud_dir, &hooks_path);
    assert_eq!(again.changed(), 1, "must re-repair regressed timeout");
    assert!(again_out.contains("updated Codex hook timeout"));
}

#[test]
fn concurrent_normalization_serializes_via_lock() {
    // Two threads racing to normalize the same file must serialize on
    // the lock; the file must end up valid (no torn write), with exactly
    // one rewrite "winning" the green message and the other observing
    // the already-normalized state.
    let tmp = TemporaryDirectory::new().unwrap();
    let (clud_dir, hooks_path) = layout(tmp.path());
    let body = json!({
        "hooks": {
            "PreToolUse": [{"matcher":"Bash","hooks":[{"type":"command","command":"a","timeout":5}]}],
        },
    });
    std::fs::write(&hooks_path, serde_json::to_string_pretty(&body).unwrap()).unwrap();

    let clud_dir1 = clud_dir.clone();
    let hooks_path1 = hooks_path.clone();
    let clud_dir2 = clud_dir.clone();
    let hooks_path2 = hooks_path.clone();

    let t1 = thread::spawn(move || run(&clud_dir1, &hooks_path1));
    // Slight stagger so the OS scheduler doesn't accidentally serialize
    // them at the kernel layer before the lock ever matters.
    thread::sleep(Duration::from_millis(5));
    let t2 = thread::spawn(move || run(&clud_dir2, &hooks_path2));

    let (a_outcome, _) = t1.join().unwrap();
    let (b_outcome, _) = t2.join().unwrap();
    let total = a_outcome.changed() + b_outcome.changed();
    assert_eq!(
        total, 1,
        "exactly one of the racing runs must change the file; got {a_outcome:?} + {b_outcome:?}"
    );

    // File must still parse cleanly and have the normalized value.
    let on_disk: Value = serde_json::from_str(&std::fs::read_to_string(&hooks_path).unwrap())
        .expect("file must remain valid JSON after concurrent normalize");
    assert_eq!(
        on_disk["hooks"]["PreToolUse"][0]["hooks"][0]["timeout"].as_u64(),
        Some(30)
    );
}

#[test]
fn green_message_omitted_when_nothing_changes() {
    let tmp = TemporaryDirectory::new().unwrap();
    let (clud_dir, hooks_path) = layout(tmp.path());
    let body = json!({
        "hooks": {
            "PreToolUse": [{"matcher":"Bash","hooks":[{"type":"command","command":"a","timeout":30}]}],
        },
    });
    std::fs::write(&hooks_path, serde_json::to_string_pretty(&body).unwrap()).unwrap();

    let (outcome, stderr) = run(&clud_dir, &hooks_path);
    assert_eq!(outcome.changed(), 0);
    assert!(
        !stderr.contains("updated Codex hook timeout"),
        "no green line when no change: {stderr}"
    );
    assert!(!stderr.contains("\x1b[32m"), "no green ANSI when no change");
}

#[test]
fn normalize_value_walks_arbitrary_nesting() {
    // Direct test of the pure walker — useful for catching regressions
    // in the walk logic without the lock/file overhead.
    let mut v: Value = json!({
        "outer": {
            "list": [
                {"timeout": 5, "name": "yes"},
                {"timeout": 5.5, "name": "float-no"},
                {"timeout": 7, "name": "high-no"},
                {"nested": {"timeout": 5}},
            ],
            "timeout": 5,
        },
    });
    let n = normalize_value(&mut v);
    assert_eq!(n, 3);
    assert_eq!(v["outer"]["list"][0]["timeout"].as_u64(), Some(30));
    assert_eq!(v["outer"]["list"][1]["timeout"].as_f64(), Some(5.5));
    assert_eq!(v["outer"]["list"][2]["timeout"].as_u64(), Some(7));
    assert_eq!(
        v["outer"]["list"][3]["nested"]["timeout"].as_u64(),
        Some(30)
    );
    assert_eq!(v["outer"]["timeout"].as_u64(), Some(30));
}

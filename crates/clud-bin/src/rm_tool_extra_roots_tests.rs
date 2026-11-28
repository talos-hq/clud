//! #1668: the human-set, reasoned `safe_rm.extra_roots` override. Decision
//! table over injected [`extra_roots::EntryFacts`] first, then end-to-end
//! calls over temp dirs and temp settings (never the real `~/.clud`).

use super::*;
use crate::clud_settings::{safe_rm_extra_roots_from, SafeRmExtraRootEntry};
use extra_roots::ExtraRoot;

#[test]
fn settings_entry_without_a_reason_is_rejected_at_load_with_a_message() {
    let doc = serde_json::json!({"safe_rm": {"extra_roots": [
        {"path": "/srv/a", "reason": "bench output"},
        {"path": "/srv/b"},
        {"path": "/srv/c", "reason": ""},
        {"reason": "no path"},
    ]}});
    let (entries, rejected) = safe_rm_extra_roots_from(&doc);
    assert_eq!(
        entries,
        vec![SafeRmExtraRootEntry {
            path: "/srv/a".into(),
            reason: "bench output".into()
        }]
    );
    assert_eq!(rejected.len(), 3, "{rejected:?}");
    assert!(rejected[0].contains("[1] (/srv/b)") && rejected[0].contains("\"reason\""));
    assert!(rejected[1].contains("[2] (/srv/c)"));
    assert!(rejected[2].contains("\"path\""));
    let (entries, rejected) = safe_rm_extra_roots_from(&serde_json::json!({}));
    assert!(entries.is_empty() && rejected.is_empty());
    let (_, rejected) =
        safe_rm_extra_roots_from(&serde_json::json!({"safe_rm": {"extra_roots": "/srv"}}));
    assert!(rejected[0].contains("must be a list"));
}

#[test]
fn peek_reads_only_the_given_home() {
    let home = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    std::fs::create_dir_all(home.path().join(".clud")).unwrap();
    std::fs::write(
        home.path().join(".clud").join("settings.json"),
        r#"{"safe_rm":{"extra_roots":[{"path":"/srv/x"}]}}"#,
    )
    .unwrap();
    let (entries, rejected) = crate::clud_settings::peek_safe_rm_extra_roots_at(home.path());
    assert!(entries.is_empty());
    assert!(rejected[0].contains("reason"), "{rejected:?}");
}

/// A world with an extra root `<base>/extra` outside the session roots.
pub(crate) fn extra_world() -> (World, PathBuf) {
    let w = world();
    let extra = w.home.parent().unwrap().join("extra");
    std::fs::create_dir_all(&extra).unwrap();
    (w, extra)
}

pub(crate) fn run_extra(w: &World, roots: &mut Roots, list: &[&str]) -> (i32, String, String) {
    let options = parse_args(&args(list)).unwrap().unwrap();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run_with(Kind::Safe, &options, &w.ctx(), roots, &mut out, &mut err);
    (
        code,
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

pub(crate) fn with_extra(w: &World, extra: &Path) -> Roots {
    w.roots().with_extra_roots(
        vec![ExtraRoot {
            root: extra.to_path_buf(),
            reason: "nightly benchmark output".into(),
        }],
        Vec::new(),
    )
}

#[test]
fn without_the_setting_behavior_is_unchanged() {
    let (w, extra) = extra_world();
    let target = extra.join("out.txt");
    std::fs::write(&target, "x").unwrap();
    let mut roots = w.roots().with_extra_roots(Vec::new(), Vec::new());
    let (code, _, err) = run_extra(&w, &mut roots, &[target.to_str().unwrap()]);
    let (plain_code, _, plain_err) = w.run(Kind::Safe, &[target.to_str().unwrap()]);
    assert_eq!((code, &err), (plain_code, &plain_err));
    assert_eq!(code, 1);
    assert!(err.contains("outside the allowed roots"), "{err}");
    assert!(!err.contains("extra_roots"), "{err}");
    assert!(target.exists());
}

#[test]
fn entries_strictly_under_an_extra_root_are_trashed_and_audited_with_the_reason() {
    let (w, extra) = extra_world();
    let state = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let _guard = crate::gc::delete_audit::StateDirGuard::set(state.path());
    let target = extra.join("run1");
    std::fs::create_dir_all(target.join("a")).unwrap();
    let mut roots = with_extra(&w, &extra);
    let (code, _, err) = run_extra(&w, &mut roots, &["-r", target.to_str().unwrap()]);
    assert_eq!(code, 0, "{err}");
    assert!(!target.exists());
    let records = w.audit_records();
    let reason = records[0]["paths"][0]["reason"]
        .as_str()
        .unwrap_or_default();
    assert!(reason.contains("safe_rm.extra_roots"), "{records:?}");
    assert!(reason.contains("nightly benchmark output"), "{records:?}");
    let gc = std::fs::read_to_string(state.path().join(crate::gc::delete_audit::AUDIT_LOG_FILE))
        .unwrap_or_default();
    assert!(gc.contains("nightly benchmark output"), "{gc}");
}

#[test]
fn purge_under_an_extra_root_writes_the_reason_to_the_delete_audit() {
    let (w, extra) = extra_world();
    let state = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let _guard = crate::gc::delete_audit::StateDirGuard::set(state.path());
    let target = extra.join("blob.bin");
    std::fs::write(&target, "x").unwrap();
    let mut roots = with_extra(&w, &extra);
    let (code, _, err) = run_extra(&w, &mut roots, &["--purge", target.to_str().unwrap()]);
    assert_eq!(code, 0, "{err}");
    assert!(!target.exists());
    let gc = std::fs::read_to_string(state.path().join(crate::gc::delete_audit::AUDIT_LOG_FILE))
        .unwrap();
    let line: serde_json::Value = serde_json::from_str(gc.lines().last().unwrap()).unwrap();
    assert!(line["rule"]
        .as_str()
        .unwrap()
        .contains("nightly benchmark output"));
}

#[test]
fn a_path_inside_the_session_roots_does_not_cite_the_override() {
    let (w, extra) = extra_world();
    let target = w.root.join("build.log");
    std::fs::write(&target, "x").unwrap();
    let mut roots = with_extra(&w, &extra);
    let (code, _, err) = run_extra(&w, &mut roots, &[target.to_str().unwrap()]);
    assert_eq!(code, 0, "{err}");
    let records = w.audit_records();
    assert!(records[0]["paths"][0]["reason"].is_null(), "{records:?}");
}

#[test]
fn the_extra_root_itself_is_refused() {
    let (w, extra) = extra_world();
    let mut roots = with_extra(&w, &extra);
    let (code, _, err) = run_extra(&w, &mut roots, &["-r", extra.to_str().unwrap()]);
    assert_eq!(code, 1);
    assert!(err.contains("extra root itself"), "{err}");
    assert!(extra.exists());
}

#[test]
fn dropped_entries_are_reported_on_every_call() {
    let (w, extra) = extra_world();
    let target = extra.join("out.txt");
    std::fs::write(&target, "x").unwrap();
    let mut roots = w.roots().with_extra_roots(
        Vec::new(),
        vec!["safe_rm.extra_roots[0] (/srv) ignored: missing a non-empty \"reason\"".into()],
    );
    let (code, _, err) = run_extra(&w, &mut roots, &[target.to_str().unwrap()]);
    assert_eq!(code, 1);
    assert!(
        err.contains("safe_rm.extra_roots[0] (/srv) ignored"),
        "{err}"
    );
    assert!(target.exists(), "an entry without a reason is not honored");
}

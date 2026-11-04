use super::*;

fn write(path: &Path, len: usize) {
    std::fs::write(path, vec![b'x'; len]).unwrap();
}

fn names(list: &[&str]) -> Vec<String> {
    file_names(list)
}

#[test]
fn current_aliases_are_clean_and_stale_or_retired_ones_are_reported() {
    let dir = tempfile::tempdir().unwrap();
    let reference = dir.path().join("clud-ref");
    write(&reference, 10);
    let own = fingerprint(&reference).unwrap();
    let fresh = dir.path().join(crate::shim_registry::file_name("fresh"));
    write(&fresh, 10);
    let stale = dir.path().join(crate::shim_registry::file_name("stale"));
    write(&stale, 7);
    let retired = dir
        .path()
        .join(crate::shim_registry::file_name("old-alias"));
    write(&retired, 10);
    let files = names(&["fresh", "stale", "absent"]);
    let report = inspect_dir(
        dir.path(),
        &files,
        &names(&["old-alias", "older-alias"]),
        &own,
        false,
    );
    assert_eq!(
        report.findings,
        vec![
            Finding::Stale {
                path: stale,
                found: format!("{}+7", env!("CARGO_PKG_VERSION")),
            },
            Finding::Retired { path: retired },
        ]
    );
    assert!(report.unknown.is_empty(), "a shared dir lists no unknowns");
}

#[test]
fn an_owned_dir_reports_missing_aliases_and_lists_foreign_files() {
    let dir = tempfile::tempdir().unwrap();
    let reference = dir.path().join(".ref");
    write(&reference, 4);
    let own = fingerprint(&reference).unwrap();
    write(&dir.path().join(crate::shim_registry::file_name("rm")), 4);
    let foreign = dir.path().join("leftover-tool");
    write(&foreign, 99);
    // Another installer's staging copy is in flight, not an orphan.
    write(&dir.path().join(".gh.123.0.tmp"), 4);
    let report = inspect_dir(dir.path(), &names(&["rm", "gh"]), &[], &own, true);
    assert_eq!(
        report.findings,
        vec![Finding::Missing {
            path: dir.path().join(crate::shim_registry::file_name("gh")),
        }]
    );
    assert_eq!(report.unknown, vec![foreign.clone()]);
    assert!(
        foreign.exists(),
        "doctor never deletes what it does not own"
    );
}

#[test]
fn an_absent_owned_dir_is_not_a_missing_alias() {
    let dir = tempfile::tempdir().unwrap();
    let report = inspect_dir(
        &dir.path().join("never-made"),
        &names(&["rm"]),
        &[],
        "x",
        true,
    );
    assert_eq!(report, DirReport::default());
}

#[test]
fn findings_read_as_one_line_each() {
    let line = Finding::Retired {
        path: PathBuf::from("/h/.clud/state/shims/old-alias"),
    }
    .to_string();
    assert_eq!(line, "retired alias /h/.clud/state/shims/old-alias remains");
}

#[test]
fn matchers_reach_tools_by_name_alias_or_catch_all() {
    assert!(matcher_reaches("*", "ScheduleWakeup"));
    assert!(matcher_reaches("", "Agent"));
    assert!(matcher_reaches("Bash|Agent", "Agent"));
    assert!(
        !matcher_reaches("Bash", "BashOutput"),
        "matchers are anchored"
    );
    // Codex's aliases (#1954).
    assert!(matcher_reaches("Edit", "apply_patch"));
    assert!(matcher_reaches("Agent", "spawn_agent"));
    assert!(matcher_reaches("Bash", "exec_command"));
    assert!(!matcher_reaches("Bash", "apply_patch"));
}

#[test]
fn the_launch_fragment_covers_every_guarded_tool() {
    let report = coverage_report(None, None);
    assert_eq!(report.len(), 1);
    assert!(report[0].uncovered.is_empty(), "{:?}", report[0].uncovered);
}

#[test]
fn a_bash_only_config_reports_what_it_misses() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join(".codex")).unwrap();
    std::fs::write(
        root.path().join(".codex/hooks.json"),
        r#"{"hooks":{"PreToolUse":[
            {"matcher":"Bash","hooks":[{"type":"command","command":"clud-cmd-scan"}]},
            {"matcher":"*","hooks":[{"type":"command","command":"python other.py"}]}
        ]}}"#,
    )
    .unwrap();
    let report = coverage_report(None, Some(root.path()));
    assert_eq!(report.len(), 2);
    let codex = &report[1];
    assert_eq!(codex.matchers, vec!["Bash".to_string()]);
    assert!(codex.uncovered.contains(&"apply_patch"));
    assert!(codex.uncovered.contains(&"spawn_agent"));
    assert!(!codex.uncovered.contains(&"exec_command"));
}

/// `clud doctor --fix` on a stale helper-bin alias: a finding before, none
/// after, and the entry is now the running binary.
#[test]
fn fix_replaces_a_stale_helper_bin_alias() {
    let tmp = tempfile::tempdir().unwrap();
    let exe = tmp.path().join("install").join("clud");
    std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
    write(&exe, 20);
    let own = fingerprint(&exe).unwrap();
    let home = tmp.path().join("home");
    let helper_bin = home.join(".clud/state/helper-bin");
    std::fs::create_dir_all(&helper_bin).unwrap();
    for name in [
        crate::multicall::CMD_SCAN,
        crate::multicall::LEGACY_CMD_SCAN,
    ] {
        write(&helper_bin.join(crate::shim_registry::file_name(name)), 7);
    }

    let before = inspect(&exe, &own, Some(&home), "");
    assert_eq!(i32::from(!before.findings.is_empty()), 1, "{before:?}");

    let actions = repair(&exe, &before, Some(&home));
    assert!(
        actions.iter().all(|a| a.starts_with("fixed:")),
        "{actions:?}"
    );

    let after = inspect(&exe, &own, Some(&home), "");
    assert_eq!(i32::from(!after.findings.is_empty()), 0, "{after:?}");
    let scan = helper_bin.join(crate::shim_registry::file_name(crate::multicall::CMD_SCAN));
    assert_eq!(fingerprint(&scan).unwrap(), own);
}

/// A foreign `clud-cmd-scan` the user owns is reported, never touched.
#[test]
fn fix_leaves_a_foreign_scanner_outside_clud_dirs_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let exe = tmp.path().join("install").join("clud");
    std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
    write(&exe, 20);
    let own = fingerprint(&exe).unwrap();
    let home = tmp.path().join("home");
    let venv = tmp.path().join("venv");
    std::fs::create_dir_all(&venv).unwrap();
    let foreign = venv.join(crate::shim_registry::file_name(crate::multicall::CMD_SCAN));
    write(&foreign, 7);
    crate::platform::shim_fs::set_mode(&foreign, 0o755).unwrap();
    let path_env = std::env::join_paths([&venv])
        .unwrap()
        .into_string()
        .unwrap();

    let report = inspect(&exe, &own, Some(&home), &path_env);
    assert!(matches!(
        report.findings.as_slice(),
        [Finding::ForeignScanner { .. }]
    ));
    let actions = repair(&exe, &report, Some(&home));
    assert!(actions[0].starts_with("left alone:"), "{actions:?}");
    assert_eq!(std::fs::read(&foreign).unwrap(), vec![b'x'; 7]);
}

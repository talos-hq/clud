//! Decision table for `bash.block_cd` (zackees/clud#967 Phase 1).
//!
//! The table is asserted directly — policy × dialect × target shape — rather
//! than through the hook binary, so a change of verdict shows up as a failing
//! row naming the case rather than as a hook exit code.

use super::*;
use crate::repo_clud_config::BlockCd;
use std::fs;
use tempfile::TempDir;

fn root() -> PathBuf {
    if crate::platform::host::WINDOWS {
        PathBuf::from(r"C:\repo")
    } else {
        PathBuf::from("/repo")
    }
}

fn home() -> PathBuf {
    if crate::platform::host::WINDOWS {
        PathBuf::from(r"C:\Users\dev")
    } else {
        PathBuf::from("/home/dev")
    }
}

fn deny(command: &str, policy: CdPolicy) -> Option<String> {
    deny_in(command, policy, ShellDialect::Posix, &root())
}

fn deny_in(command: &str, policy: CdPolicy, dialect: ShellDialect, cwd: &Path) -> Option<String> {
    cd_denial_reason(
        command,
        dialect,
        policy,
        cwd,
        &[root()],
        Some(&home()),
        None,
    )
}

fn targets(command: &str, dialect: ShellDialect) -> Vec<CdTarget> {
    scan_session_cd(command, dialect, &root(), Some(&home()))
        .into_iter()
        .map(|occurrence| occurrence.target)
        .collect()
}

/// The `cd` operands as written, for assertions that must not depend on how
/// the host resolves a path spelling.
fn raw_targets(command: &str, dialect: ShellDialect) -> Vec<Option<String>> {
    scan_session_cd(command, dialect, &root(), Some(&home()))
        .into_iter()
        .map(|occurrence| occurrence.raw_target)
        .collect()
}

// -----------------------------------------------------------------
// Scanner: only a `cd` that moves the *session* counts.
// -----------------------------------------------------------------

#[test]
fn a_top_level_cd_is_session_mutating() {
    assert_eq!(targets("cd /tmp", ShellDialect::Posix).len(), 1);
    assert_eq!(targets("ls && cd /tmp", ShellDialect::Posix).len(), 1);
    assert_eq!(targets("cd /tmp; ls", ShellDialect::Posix).len(), 1);
    // Env assignments precede the command word without hiding it.
    assert_eq!(targets("FOO=1 cd /tmp", ShellDialect::Posix).len(), 1);
}

#[test]
fn a_subshell_cd_cannot_leak_and_is_ignored() {
    // The sanctioned workaround the denial message recommends: it must not
    // itself be denied, or the guard would have no escape hatch.
    assert!(targets("(cd /tmp && ls)", ShellDialect::Posix).is_empty());
    assert!(targets("ls; (cd /tmp && make)", ShellDialect::Posix).is_empty());
    assert!(targets("echo $(cd /tmp && pwd)", ShellDialect::Posix).is_empty());
    assert!(targets("echo `cd /tmp && pwd`", ShellDialect::Posix).is_empty());
}

#[test]
fn a_nested_shell_cd_changes_only_the_child() {
    assert!(targets("bash -c 'cd /tmp && ls'", ShellDialect::Posix).is_empty());
    assert!(targets("sh -c \"cd /tmp\"", ShellDialect::Posix).is_empty());
}

#[test]
fn cd_inside_quotes_or_a_heredoc_body_is_not_a_cd() {
    assert!(targets("echo \"cd /tmp\"", ShellDialect::Posix).is_empty());
    assert!(targets("echo 'cd /tmp'", ShellDialect::Posix).is_empty());
    let heredoc = "cat <<'EOF'\ncd /tmp\nEOF";
    assert!(targets(heredoc, ShellDialect::Posix).is_empty());
}

#[test]
fn a_cd_must_be_in_command_position() {
    // The pre-filter is loose by design; the scanner is not. `curl -sL` must
    // not read as PowerShell's `sl`, and a path ending in `cd` is not a `cd`.
    assert!(targets("curl -sL https://example.com", ShellDialect::PowerShell).is_empty());
    assert!(targets("./abcd --flag", ShellDialect::Posix).is_empty());
    assert!(targets("git log --format=cd", ShellDialect::Posix).is_empty());
}

#[test]
fn the_prefilter_admits_every_dialects_builtins() {
    assert!(command_may_change_directory("cd /tmp", ShellDialect::Posix));
    assert!(command_may_change_directory(
        "Set-Location C:\\tmp",
        ShellDialect::PowerShell
    ));
    assert!(command_may_change_directory(
        "sl C:\\tmp",
        ShellDialect::PowerShell
    ));
    assert!(!command_may_change_directory(
        "curl -sL https://example.com",
        ShellDialect::PowerShell
    ));
    assert!(!command_may_change_directory(
        "cargo build",
        ShellDialect::Posix
    ));
}

// -----------------------------------------------------------------
// Target resolution.
// -----------------------------------------------------------------

#[test]
fn home_spellings_resolve_because_they_are_how_agents_leave_a_repo() {
    assert_eq!(
        targets("cd ~", ShellDialect::Posix),
        vec![CdTarget::Path(home())]
    );
    assert_eq!(
        targets("cd $HOME", ShellDialect::Posix),
        vec![CdTarget::Path(home())]
    );
    assert_eq!(
        targets("cd %USERPROFILE%", ShellDialect::Cmd),
        vec![CdTarget::Path(home())]
    );
    assert_eq!(
        targets("cd $HOME/projects", ShellDialect::Posix),
        vec![CdTarget::Path(home().join("projects"))]
    );
}

#[test]
fn an_unknowable_target_is_reported_as_such_not_guessed() {
    assert_eq!(
        targets("cd -", ShellDialect::Posix),
        vec![CdTarget::Unresolvable]
    );
    assert_eq!(
        targets("cd $SOME_DIR", ShellDialect::Posix),
        vec![CdTarget::Unresolvable]
    );
}

#[test]
fn a_bare_cd_differs_by_dialect() {
    // POSIX goes home; PowerShell's no-arg Set-Location and cmd's `cd` do
    // not move at all (verified on Windows PowerShell 5.1).
    assert_eq!(
        targets("cd", ShellDialect::Posix),
        vec![CdTarget::Path(home())]
    );
    assert_eq!(
        targets("cd", ShellDialect::PowerShell),
        vec![CdTarget::NoOp]
    );
    assert_eq!(targets("cd", ShellDialect::Cmd), vec![CdTarget::NoOp]);
}

#[test]
fn flags_are_skipped_to_find_the_operand() {
    // Asserted on the operand *as written*, because what it resolves to is
    // host-dependent: `C:\tmp` is absolute on Windows but a relative name on
    // Linux, and `/tmp` the reverse. Resolution itself is covered above.
    assert_eq!(
        raw_targets("cd -P /tmp", ShellDialect::Posix),
        vec![Some("/tmp".to_string())]
    );
    assert_eq!(
        raw_targets("cd /d C:\\tmp", ShellDialect::Cmd),
        vec![Some("C:\\tmp".to_string())]
    );
    assert_eq!(
        raw_targets("Set-Location -Path C:\\tmp", ShellDialect::PowerShell),
        vec![Some("C:\\tmp".to_string())]
    );
    // A cmd `/d` flag must not be mistaken for a POSIX absolute operand.
    assert_eq!(
        raw_targets("cd /d", ShellDialect::Cmd),
        vec![None],
        "`/d` alone leaves no operand"
    );
}

// -----------------------------------------------------------------
// The decision table.
// -----------------------------------------------------------------

#[test]
fn strict_denies_drift_within_the_repo_because_hooks_break_on_any_drift() {
    // The wedge that motivated the issue was an *in-repo* cd.
    assert!(deny("cd src", CdPolicy::Strict).is_some());
    assert!(deny("cd ./crates/clud-bin", CdPolicy::Strict).is_some());
}

#[test]
fn strict_allows_cd_back_to_a_registered_root_as_the_recovery_path() {
    let spelled_absolute = format!("cd {}", root().display());
    assert!(deny(&spelled_absolute, CdPolicy::Strict).is_none());
    // Spelled relatively from a subdirectory, it is the same directory.
    let subdir = root().join("src");
    assert!(deny_in("cd ..", CdPolicy::Strict, ShellDialect::Posix, &subdir).is_none());
}

#[test]
fn strict_denies_an_unresolvable_target_but_relaxed_does_not() {
    // Strict must be able to prove the destination is a root; relaxed
    // narrows only on evidence of an escape, and has none here.
    assert!(deny("cd $SOME_DIR", CdPolicy::Strict).is_some());
    assert!(deny("cd $SOME_DIR", CdPolicy::Relaxed).is_none());
    assert!(deny("cd -", CdPolicy::Strict).is_some());
    assert!(deny("cd -", CdPolicy::Relaxed).is_none());
}

#[test]
fn relaxed_allows_in_repo_movement_and_denies_leaving() {
    // The Phase 5 exit criterion: a fully-migrated repo can `cd` freely
    // inside its registered trees, and only an escape of *all* of them is
    // denied.
    assert!(deny("cd src", CdPolicy::Relaxed).is_none());
    assert!(deny("cd ./crates", CdPolicy::Relaxed).is_none());
    assert!(deny("cd ~", CdPolicy::Relaxed).is_some());
    assert!(deny("cd ../elsewhere", CdPolicy::Relaxed).is_some());
}

#[test]
fn relaxed_allows_movement_between_registered_roots() {
    // Phase 3 widened the pinned set from {parent root} to all registered
    // roots; moving from one root's tree into another's is not an escape.
    let roots = if crate::platform::host::WINDOWS {
        vec![
            PathBuf::from(r"C:\repo"),
            PathBuf::from(r"C:\extern\dep"),
            PathBuf::from(r"C:\child"),
        ]
    } else {
        vec![
            PathBuf::from("/repo"),
            PathBuf::from("/extern/dep"),
            PathBuf::from("/child"),
        ]
    };
    let from_extern = roots[1].join("src");
    let from_child = roots[2].join("lib");
    let reason = |command: &str, cwd: &Path| {
        cd_denial_reason(
            command,
            ShellDialect::Posix,
            CdPolicy::Relaxed,
            cwd,
            &roots,
            Some(&home()),
            None,
        )
    };
    assert!(reason("cd /repo", &from_extern).is_none());
    assert!(
        reason("cd ..", &from_child).is_none(),
        "back into the child root"
    );
    assert!(
        reason("cd ../..", &from_child).is_some(),
        "out of every registered root"
    );
    assert!(reason("cd /elsewhere", &from_extern).is_some());
    // Strict still requires the destination to *be* a root: in-tree
    // subdirectories stay denied even in a multi-root session.
    assert!(reason("cd src", &from_extern).is_none());
    assert!(cd_denial_reason(
        "cd src",
        ShellDialect::Posix,
        CdPolicy::Strict,
        &roots[1],
        &roots,
        Some(&home()),
        None,
    )
    .is_some());
}

#[test]
fn a_bare_cd_that_cannot_move_is_never_denied() {
    assert!(deny_in("cd", CdPolicy::Strict, ShellDialect::PowerShell, &root()).is_none());
    assert!(deny_in("cd", CdPolicy::Strict, ShellDialect::Cmd, &root()).is_none());
    // POSIX bare `cd` does move — to home, which is an escape.
    assert!(deny_in("cd", CdPolicy::Strict, ShellDialect::Posix, &root()).is_some());
}

#[test]
fn policy_off_and_an_empty_root_set_both_decide_nothing() {
    assert!(deny("cd /tmp", CdPolicy::Off).is_none());
    assert!(cd_denial_reason(
        "cd /tmp",
        ShellDialect::Posix,
        CdPolicy::Strict,
        &root(),
        &[],
        Some(&home()),
        None,
    )
    .is_none());
}

#[test]
fn the_denial_names_the_fix_the_recovery_path_and_the_override() {
    let reason = deny("cd src", CdPolicy::Strict).expect("denied");
    assert!(reason.contains("(cd DIR && CMD)"), "{reason}");
    assert!(reason.contains("git -C DIR"), "{reason}");
    assert!(reason.contains("bash.block_cd"), "{reason}");
    assert!(reason.contains(BLOCK_CD_RULE_ID), "{reason}");
    assert!(reason.contains("recover"), "{reason}");
}

#[test]
fn a_strict_denial_quotes_the_hook_that_earned_it() {
    let reason = cd_denial_reason(
        "cd src",
        ShellDialect::Posix,
        CdPolicy::Strict,
        &root(),
        &[root()],
        Some(&home()),
        Some("`uv run python ci/hooks/check.py` in .claude/settings.json"),
    )
    .expect("denied");
    assert!(reason.contains("ci/hooks/check.py"), "{reason}");
}

// -----------------------------------------------------------------
// `"auto"` resolution.
// -----------------------------------------------------------------

fn scan_with(sensitive: bool, any: bool) -> HookCwdScan {
    HookCwdScan {
        any_hooks: any,
        dispatcher_managed: false,
        sensitive: if sensitive {
            vec![SensitiveHook {
                source: PathBuf::from(".claude/settings.json"),
                command: "uv run python ci/hooks/check.py".to_string(),
            }]
        } else {
            Vec::new()
        },
        broken_git_prefix: Vec::new(),
        pwd_walk_prefix: Vec::new(),
    }
}

fn scan_with_dispatcher(dispatcher_managed: bool, sensitive: bool) -> HookCwdScan {
    HookCwdScan {
        any_hooks: false,
        dispatcher_managed,
        sensitive: if sensitive {
            vec![SensitiveHook {
                source: PathBuf::from(".claude/settings.json"),
                command: "uv run python ci/hooks/check.py".to_string(),
            }]
        } else {
            Vec::new()
        },
        broken_git_prefix: Vec::new(),
        pwd_walk_prefix: Vec::new(),
    }
}

#[test]
fn auto_resolves_against_the_hooks_actually_in_scope() {
    let sensitive = scan_with(true, true);
    let safe = scan_with(false, true);
    let none = scan_with(false, false);

    assert_eq!(
        resolve_policy(BlockCd::Auto, true, &sensitive),
        CdPolicy::Strict
    );
    assert_eq!(
        resolve_policy(BlockCd::Auto, true, &safe),
        CdPolicy::Relaxed
    );
    assert_eq!(resolve_policy(BlockCd::Auto, true, &none), CdPolicy::Off);
    // Outside a repo there is no root to pin to.
    assert_eq!(
        resolve_policy(BlockCd::Auto, false, &sensitive),
        CdPolicy::Off
    );
}

/// The Phase 5 truth table across migration states (#967 Phase 5, #966 D13):
/// setting × hook-sensitivity × dispatcher opt-in × in-repo ⇒ verdict.
#[test]
fn auto_resolver_truth_table_across_migration_states() {
    use crate::repo_clud_config::BlockCd;
    // Raw hooks only — the Phase 1 states, unchanged by migration.
    let raw_sensitive = scan_with(true, true);
    let raw_safe = scan_with(false, true);
    let nothing = scan_with(false, false);
    // Dispatcher opt-in, no raw hooks — "fully on clud hooks".
    let migrated = scan_with_dispatcher(true, false);
    // Migration is *not* complete while a cwd-sensitive raw hook remains.
    let migrated_but_raw_sensitive = scan_with_dispatcher(true, true);
    // Opt-in with no raw hooks outside a repo cannot happen in practice
    // (opt-in implies a repo), but the resolver must still answer off.
    let migrated_outside = scan_with_dispatcher(true, false);

    let cases: &[(BlockCd, bool, &HookCwdScan, CdPolicy)] = &[
        // Unmigrated: sensitive raw hooks keep strict (Phase 1 behavior).
        (BlockCd::Auto, true, &raw_sensitive, CdPolicy::Strict),
        // Unmigrated with cwd-safe raw hooks: nothing breaks on drift.
        (BlockCd::Auto, true, &raw_safe, CdPolicy::Relaxed),
        // No hooks / not a repo: no policy at all.
        (BlockCd::Auto, true, &nothing, CdPolicy::Off),
        (BlockCd::Auto, false, &raw_sensitive, CdPolicy::Off),
        // Migrated: relaxed is earned even though the scan sees no raw hooks.
        (BlockCd::Auto, true, &migrated, CdPolicy::Relaxed),
        // Sensitive raw hooks keep strict even after migration — the harness
        // still fires that copy unrooted.
        (
            BlockCd::Auto,
            true,
            &migrated_but_raw_sensitive,
            CdPolicy::Strict,
        ),
        (BlockCd::Auto, false, &migrated_outside, CdPolicy::Off),
        // Explicit settings ignore the environment entirely.
        (BlockCd::Always, true, &nothing, CdPolicy::Strict),
        (BlockCd::Never, true, &raw_sensitive, CdPolicy::Off),
        (BlockCd::Always, false, &nothing, CdPolicy::Off),
    ];
    for (setting, in_repo, scan, expected) in cases {
        assert_eq!(
            resolve_policy(*setting, *in_repo, scan),
            *expected,
            "setting={setting:?} in_repo={in_repo} scan={scan:?}"
        );
    }
}

#[test]
fn explicit_settings_do_not_consult_the_hooks() {
    let none = scan_with(false, false);
    assert_eq!(
        resolve_policy(BlockCd::Always, true, &none),
        CdPolicy::Strict
    );
    assert_eq!(
        resolve_policy(BlockCd::Never, true, &scan_with(true, true)),
        CdPolicy::Off
    );
    assert_eq!(resolve_policy(BlockCd::Always, false, &none), CdPolicy::Off);
}

// -----------------------------------------------------------------
// Hook cwd-sensitivity classification.
// -----------------------------------------------------------------

#[test]
fn a_path_binary_or_absolute_command_is_cwd_immune() {
    assert!(!is_cwd_sensitive_hook_command("clud-cmd-scan"));
    assert!(!is_cwd_sensitive_hook_command(
        "clud-cmd-scan; exit $LASTEXITCODE"
    ));
    assert!(!is_cwd_sensitive_hook_command("/usr/local/bin/guard.sh"));
    assert!(!is_cwd_sensitive_hook_command("python -c \"import sys\""));
}

#[test]
fn a_relative_script_path_is_cwd_sensitive() {
    assert!(is_cwd_sensitive_hook_command(
        "uv run python ci/hooks/check-on-stop.py"
    ));
    assert!(is_cwd_sensitive_hook_command("./scripts/guard.sh"));
    assert!(is_cwd_sensitive_hook_command("bash .claude/hooks/lint.sh"));
}

#[test]
fn a_self_rooting_prefix_makes_the_relative_path_safe() {
    assert!(!is_cwd_sensitive_hook_command(
        "cd \"$CLAUDE_PROJECT_DIR\" && uv run python ci/hooks/check-on-stop.py"
    ));
    assert!(!is_cwd_sensitive_hook_command(
        "cd \"$CLUD_PROJECT_DIR\" && ./scripts/guard.sh"
    ));
}

#[test]
fn the_broken_git_rev_parse_prefix_is_detected_and_stays_sensitive() {
    let broken = "cd \"$(git rev-parse --show-superproject-working-tree 2>/dev/null || git \
                  rev-parse --show-toplevel 2>/dev/null || echo .)\" && uv run python \
                  ci/hooks/check-on-stop.py";
    assert!(has_broken_git_rev_parse_prefix(broken));
    // It *looks* self-rooting, which is exactly why it wedged a session: the
    // `||` fallback is dead code, so the relative path is still exposed.
    assert!(is_cwd_sensitive_hook_command(broken));
    assert!(!has_broken_git_rev_parse_prefix(
        "cd \"$CLAUDE_PROJECT_DIR\" && uv run python ci/hooks/check-on-stop.py"
    ));
}

// -----------------------------------------------------------------
// Scanning real config files — the issue's exit criteria.
// -----------------------------------------------------------------

fn write_json(path: &Path, body: &str) {
    fs::create_dir_all(path.parent().unwrap()).expect("mkdir");
    fs::write(path, body).expect("write");
}

#[test]
fn a_stop_hook_is_scanned_even_though_hook_health_reads_only_pretooluse() {
    // The FastLED wedge came from a Stop hook; a PreToolUse-only scan would
    // have resolved `"auto"` to escape-only and allowed the cd that wedged it.
    let tmp = TempDir::new().unwrap();
    write_json(
        &tmp.path().join(".claude").join("settings.json"),
        r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"uv run python ci/hooks/check-on-stop.py"}]}]}}"#,
    );
    let scan = scan_hook_cwd_sensitivity(tmp.path(), None);
    assert!(scan.any_hooks);
    assert_eq!(scan.sensitive.len(), 1);
    assert_eq!(resolve_policy(BlockCd::Auto, true, &scan), CdPolicy::Strict);
    let hint = scan.hint().expect("hint");
    assert!(hint.contains("check-on-stop.py"), "{hint}");
}

#[test]
fn a_repo_whose_hooks_are_all_path_binaries_resolves_to_relaxed() {
    let tmp = TempDir::new().unwrap();
    write_json(
        &tmp.path().join(".claude").join("settings.json"),
        r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"clud-cmd-scan"}]}]}}"#,
    );
    let scan = scan_hook_cwd_sensitivity(tmp.path(), None);
    assert!(scan.any_hooks);
    assert!(!scan.dispatcher_managed);
    assert!(scan.sensitive.is_empty());
    assert_eq!(
        resolve_policy(BlockCd::Auto, true, &scan),
        CdPolicy::Relaxed
    );
}

#[test]
fn a_hooks_json_opt_in_is_dispatcher_managed_even_with_no_raw_hooks() {
    // The Phase 5 core: a fully-migrated repo's scan sees no raw hooks at
    // all, yet `"auto"` must not resolve to off — its declared hooks are
    // dispatcher-managed and cwd-immune, so the repo earns relaxed.
    let tmp = TempDir::new().unwrap();
    write_json(
        &tmp.path().join(".clud").join("hooks.json"),
        r#"{"hooks":{"Stop":[{"command":"uv run python ci/hooks/check-on-stop.py"}]}}"#,
    );
    // Note the declared command *looks* cwd-sensitive (a relative script
    // path) — that must not matter, because the dispatcher roots it (D10).
    let scan = scan_hook_cwd_sensitivity(tmp.path(), None);
    assert!(!scan.any_hooks);
    assert!(scan.dispatcher_managed);
    assert!(scan.sensitive.is_empty());
    assert_eq!(
        resolve_policy(BlockCd::Auto, true, &scan),
        CdPolicy::Relaxed
    );
}

#[test]
fn a_hooks_json_opt_in_does_not_mask_a_sensitive_raw_hook() {
    // Migrating is not complete while a cwd-sensitive hook is still declared
    // in a frontend config — the harness fires that copy itself, unrooted, so
    // any drift still breaks it (DD-063).
    let tmp = TempDir::new().unwrap();
    write_json(
        &tmp.path().join(".clud").join("hooks.json"),
        r#"{"hooks":{"Stop":[{"command":"uv run python ci/hooks/check-on-stop.py"}]}}"#,
    );
    write_json(
        &tmp.path().join(".claude").join("settings.json"),
        r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"uv run python ci/hooks/check-on-stop.py"}]}]}}"#,
    );
    let scan = scan_hook_cwd_sensitivity(tmp.path(), None);
    assert!(scan.dispatcher_managed);
    assert_eq!(scan.sensitive.len(), 1);
    assert_eq!(resolve_policy(BlockCd::Auto, true, &scan), CdPolicy::Strict);
}

#[test]
fn an_empty_hooks_json_is_not_an_opt_in() {
    let tmp = TempDir::new().unwrap();
    write_json(&tmp.path().join(".clud").join("hooks.json"), r#"{}"#);
    let scan = scan_hook_cwd_sensitivity(tmp.path(), None);
    assert!(!scan.dispatcher_managed);
    assert_eq!(resolve_policy(BlockCd::Auto, true, &scan), CdPolicy::Off);
}

#[test]
fn a_repo_with_no_hooks_gets_no_policy_at_all() {
    // Exit criterion: repos with no hooks see no behavior change.
    let tmp = TempDir::new().unwrap();
    let scan = scan_hook_cwd_sensitivity(tmp.path(), None);
    assert_eq!(scan, HookCwdScan::default());
    assert_eq!(resolve_policy(BlockCd::Auto, true, &scan), CdPolicy::Off);
}

#[test]
fn codex_hook_configs_are_scanned_too() {
    let tmp = TempDir::new().unwrap();
    write_json(
        &tmp.path().join(".codex").join("hooks.json"),
        r#"{"hooks":{"PreToolUse":[{"hooks":[{"command":"uv run python ci/hooks/guard.py"}]}]}}"#,
    );
    let scan = scan_hook_cwd_sensitivity(tmp.path(), None);
    assert_eq!(scan.sensitive.len(), 1);
}

#[test]
fn the_broken_prefix_is_recorded_with_its_source_for_the_hooks_report() {
    let tmp = TempDir::new().unwrap();
    write_json(
        &tmp.path().join(".claude").join("settings.json"),
        r#"{"hooks":{"Stop":[{"hooks":[{"command":"cd \"$(git rev-parse --show-superproject-working-tree 2>/dev/null || git rev-parse --show-toplevel 2>/dev/null || echo .)\" && uv run python ci/hooks/check.py"}]}]}}"#,
    );
    let scan = scan_hook_cwd_sensitivity(tmp.path(), None);
    assert_eq!(scan.broken_git_prefix.len(), 1);
    assert_eq!(scan.sensitive.len(), 1, "broken prefix is still sensitive");
}

#[test]
fn an_unparsable_hook_config_is_skipped_rather_than_fatal() {
    let tmp = TempDir::new().unwrap();
    write_json(
        &tmp.path().join(".claude").join("settings.json"),
        "{not json",
    );
    assert_eq!(
        scan_hook_cwd_sensitivity(tmp.path(), None),
        HookCwdScan::default()
    );
}

// -----------------------------------------------------------------
// `CwdChanged` backstop: drift the PreToolUse scanner cannot see.
// -----------------------------------------------------------------

/// The `CwdChanged` drift predicate, per policy: strict requires the new cwd
/// to *be* a root; relaxed only that it stay inside one of the registered
/// trees; off (and an empty root set) never warns.
#[test]
fn drift_warning_matches_the_policy_it_was_resolved_from() {
    let inside = root().join("src");
    let escaped = home();

    // Strict: even an in-tree subdirectory is drift.
    let strict = drift_warning(&inside, CdPolicy::Strict, &[root()]);
    assert!(strict.is_some(), "strict must flag in-tree drift");
    assert!(drift_warning(&root(), CdPolicy::Strict, &[root()]).is_none());

    // Relaxed: in-tree movement is fine, an escape of every root is not.
    assert!(drift_warning(&inside, CdPolicy::Relaxed, &[root()]).is_none());
    let relaxed = drift_warning(&escaped, CdPolicy::Relaxed, &[root()]);
    assert!(relaxed.is_some());
    assert!(relaxed.unwrap().contains("CwdChanged"), "names the event");

    // Off and empty-root-set never warn.
    assert!(drift_warning(&escaped, CdPolicy::Off, &[root()]).is_none());
    assert!(drift_warning(&escaped, CdPolicy::Strict, &[]).is_none());
    assert!(drift_warning(&escaped, CdPolicy::Relaxed, &[]).is_none());
}

#[test]
fn drift_warning_judges_against_the_whole_registered_root_set() {
    // The session can legally sit in any registered root's tree; only a cwd
    // outside all of them drifts.
    let roots = if crate::platform::host::WINDOWS {
        vec![PathBuf::from(r"C:\repo"), PathBuf::from(r"C:\extern\dep")]
    } else {
        vec![PathBuf::from("/repo"), PathBuf::from("/extern/dep")]
    };
    assert!(drift_warning(&roots[1], CdPolicy::Strict, &roots).is_none());
    assert!(drift_warning(&roots[1].join("src"), CdPolicy::Relaxed, &roots).is_none());
    assert!(drift_warning(&roots[1].join("src"), CdPolicy::Strict, &roots).is_some());
    assert!(drift_warning(&home(), CdPolicy::Relaxed, &roots).is_some());
}

// -----------------------------------------------------------------
// Repo-root discovery.
// -----------------------------------------------------------------

#[test]
fn nearest_repo_root_distinguishes_no_repo_from_repo_at_cwd() {
    let tmp = TempDir::new().unwrap();
    let nested = tmp.path().join("crates").join("clud-bin");
    fs::create_dir_all(&nested).unwrap();
    assert_eq!(nearest_repo_root(&nested), None);

    fs::create_dir_all(tmp.path().join(".git")).unwrap();
    let found = nearest_repo_root(&nested).expect("root");
    assert_eq!(
        crate::path_norm::normalize_for_key(&found),
        crate::path_norm::normalize_for_key(tmp.path())
    );
}

#[test]
fn a_git_file_counts_so_linked_worktrees_resolve() {
    let tmp = TempDir::new().unwrap();
    fs::write(
        tmp.path().join(".git"),
        "gitdir: /elsewhere/.git/worktrees/wt",
    )
    .unwrap();
    assert!(nearest_repo_root(tmp.path()).is_some());
}

// ─── #972: a `$PWD` walk picks the wrong project ──────────────────────
//
// The observed failure was a Stop hook installed in FastLED binding to the
// `fbuild` checkout under `.extern-repos/`, running *its* hook, and turning a
// "quick lint" into a ~500-crate cargo build — twice, ~17 minutes, ending in
// an opaque build-backend error that named no hook at all.

/// The real wrapper from the issue. It is already `cwd_sensitive` via its
/// relative script path, but that generic verdict describes the wrong hazard:
/// the command *does* `cd` to an absolute path. What makes it dangerous is
/// which path it picks.
#[test]
fn the_pwd_walk_wrapper_is_named_specifically_not_just_cwd_sensitive() {
    let wrapper = "[ \"$FL_HOOK_SKIP\" = \"1\" ] && exit 0; S=ci/hooks/check-on-stop.py; \
                   d=$PWD; while [ \"$d\" != / ] && { [ ! -f \"$d/pyproject.toml\" ] || \
                   [ ! -f \"$d/$S\" ]; }; do d=$(dirname \"$d\"); done; \
                   if [ -f \"$d/pyproject.toml\" ]; then cd \"$d\" && uv run python \"$S\"; fi";
    assert!(has_pwd_walk_root_prefix(wrapper));
    assert!(is_cwd_sensitive_hook_command(wrapper));
    // Not the other specific remedy — these are different mistakes.
    assert!(!has_broken_git_rev_parse_prefix(wrapper));
}

/// The signature is all three parts together. Each alone is ordinary shell
/// that a correct hook may well contain, so requiring the conjunction is what
/// keeps this from firing on hooks that are fine.
#[test]
fn an_incomplete_signature_is_not_a_pwd_walk() {
    // Mentions $PWD, never climbs.
    assert!(!has_pwd_walk_root_prefix(
        "cd \"$PWD\" && uv run python ci/hooks/check-on-stop.py"
    ));
    // Climbs once against a known path, seeds from no cwd.
    assert!(!has_pwd_walk_root_prefix(
        "cd \"$(dirname \"$CLAUDE_PROJECT_DIR/pyproject.toml\")\" && ./guard.sh"
    ));
    // Walks, but tests no project marker — not root discovery.
    assert!(!has_pwd_walk_root_prefix(
        "d=$PWD; while [ -n \"$d\" ]; do d=$(dirname \"$d\"); done"
    ));
    // The recommended form.
    assert!(!has_pwd_walk_root_prefix(
        "cd \"$CLAUDE_PROJECT_DIR\" && uv run python ci/hooks/check-on-stop.py"
    ));
}

/// The denial `cd /elsewhere` produces under the relaxed policy.
fn full_denial() -> String {
    deny("cd /elsewhere", CdPolicy::Relaxed).expect("denied")
}

#[test]
fn first_block_in_a_session_is_full_then_short() {
    let dir = TempDir::new().unwrap();
    let first = condense_at(Some(dir.path()), Some("s1"), full_denial());
    assert_eq!(first, full_denial());
    let second = condense_at(Some(dir.path()), Some("s1"), full_denial());
    // The full text is itself compact since #2441, so bound the repeat in
    // absolute terms as well as against the first refusal.
    assert!(second.len() < first.len(), "{second}");
    assert!(second.len() <= 200, "{second}");
    assert!(second.contains("[block-cd]"), "{second}");
    assert!(second.contains("cd /elsewhere"), "{second}");
    assert!(second.contains("(cd DIR && CMD)"), "{second}");
    assert!(
        second.contains(&format!(
            "CLUD_BAD_CMD_OVERRIDE={BLOCK_CD_RULE_ID}:<reason>"
        )),
        "{second}"
    );
}

#[test]
fn a_different_session_gets_the_full_text_again() {
    let dir = TempDir::new().unwrap();
    condense_at(Some(dir.path()), Some("s1"), full_denial());
    let other = condense_at(Some(dir.path()), Some("s2"), full_denial());
    assert_eq!(other, full_denial());
}

#[test]
fn stale_seen_markers_are_pruned_when_a_new_one_is_written() {
    let dir = TempDir::new().unwrap();
    let seen = dir.path().join(SEEN_DIR_NAME);
    fs::create_dir_all(&seen).unwrap();
    let stale = seen.join("stale.seen");
    fs::write(&stale, b"1").unwrap();
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(30 * 24 * 60 * 60);
    fs::File::options()
        .write(true)
        .open(&stale)
        .unwrap()
        .set_modified(old)
        .unwrap();
    condense_at(Some(dir.path()), Some("fresh"), full_denial());
    assert!(!stale.exists(), "stale marker survived");
    assert!(seen_file(dir.path(), "fresh").exists());
}

#[test]
fn unreadable_state_or_no_session_fails_open_to_full() {
    let dir = TempDir::new().unwrap();
    let blocker = dir.path().join("not-a-dir");
    fs::write(&blocker, b"x").unwrap();
    for _ in 0..2 {
        assert_eq!(
            condense_at(Some(&blocker), Some("s1"), full_denial()),
            full_denial()
        );
    }
    assert_eq!(condense_at(None, Some("s1"), full_denial()), full_denial());
    for _ in 0..2 {
        assert_eq!(
            condense_at(Some(dir.path()), None, full_denial()),
            full_denial()
        );
    }
}

/// The remedy names the session project root, and says why a walk is wrong —
/// an operator reading it should not have to rediscover the extern-checkout
/// interaction.
#[test]
fn the_pwd_walk_remedy_names_the_root_and_the_reason() {
    assert!(crate::block_bad_cmd::PWD_WALK_PREFIX_FIX.contains("CLAUDE_PROJECT_DIR"));
    assert!(crate::block_bad_cmd::PWD_WALK_PREFIX_FIX.contains(".extern-repos"));
}

// ---------------------------------------------------------------------
// #2354: a blocked `cd DIR && CMD` runs in a subshell instead.
// ---------------------------------------------------------------------

fn sub(command: &str) -> Option<String> {
    subshell_substitute(command, ShellDialect::Posix)
}

#[test]
fn a_leading_cd_chain_is_wrapped_in_a_subshell() {
    assert_eq!(sub("cd x && ls").as_deref(), Some("(cd x && ls)"));
    assert_eq!(sub("  cd x; ls -la  ").as_deref(), Some("(cd x; ls -la)"));
    assert_eq!(
        sub("cd x && ls | wc -l").as_deref(),
        Some("(cd x && ls | wc -l)")
    );
}

#[test]
fn unsafe_forms_keep_the_denial() {
    for command in [
        "cd x && exit 3",
        "cd x && exec ls",
        "cd x && export A=1",
        "cd x && set -e",
        "cd x && source env.sh",
        "cd x && . env.sh",
        "cd x && alias l=ls",
        "cd x && unset A",
        "cd x && pushd y",
        "cd x && popd",
        "cd x && sleep 5 &",
        "cd x && ls # note",
        "cd x && cat <<EOF\nhi\nEOF",
        "cd x && echo hi \\",
        "cd x &&\nls",
        "ls && cd x",
        "A=1; cd x && ls",
        "f() { :; }; cd x && ls",
        "cd x && (ls",
        "",
    ] {
        assert_eq!(sub(command), None, "{command:?}");
    }
    assert_eq!(
        subshell_substitute("cd x && ls", ShellDialect::PowerShell),
        None
    );
}

fn substitution_response(
    input: serde_json::Value,
    mode: Option<&str>,
) -> Option<serde_json::Value> {
    let raw = serde_json::json!({
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "cwd": "/repo",
        "tool_input": input,
    })
    .to_string();
    let payload =
        crate::block_bad_cmd::parse_payload(&raw, Path::new("/repo")).expect("payload parses");
    let rewritten = sub(&payload.command)?;
    let notice = substitution_notice(&payload.command, &rewritten);
    crate::block_bad_cmd::cd_substitution_output(&payload, rewritten, &notice, "PreToolUse", mode)
}

fn bash_input() -> serde_json::Value {
    serde_json::json!({"command": "cd x && ls", "description": "d"})
}

#[test]
fn bypass_permissions_substitutes_with_allow() {
    let out = substitution_response(bash_input(), Some("bypassPermissions")).unwrap();
    let specific = &out["hookSpecificOutput"];
    assert_eq!(specific["permissionDecision"], "allow");
    assert_eq!(specific["updatedInput"]["command"], "(cd x && ls)");
    assert_eq!(specific["updatedInput"]["description"], "d");
    let context = specific["additionalContext"].as_str().unwrap();
    assert!(context.starts_with("[clud] ran `(cd x && ls)` instead of `cd x && ls`"));
}

#[test]
fn other_permission_modes_substitute_with_ask() {
    for mode in ["default", "acceptEdits", "plan"] {
        let out = substitution_response(bash_input(), Some(mode)).unwrap();
        let specific = &out["hookSpecificOutput"];
        assert_eq!(specific["permissionDecision"], "ask", "{mode}");
        assert_eq!(
            specific["updatedInput"]["command"], "(cd x && ls)",
            "{mode}"
        );
    }
}

#[test]
fn a_missing_permission_mode_keeps_the_deny() {
    assert_eq!(substitution_response(bash_input(), None), None);
}

#[test]
fn a_non_object_tool_input_keeps_the_deny() {
    let out = substitution_response(serde_json::json!("cd x && ls"), Some("bypassPermissions"));
    assert_eq!(out, None);
}

#[test]
fn a_codex_session_is_one_without_a_claude_project_dir() {
    assert!(crate::block_bad_cmd::codex_without_project_dir(None));
    assert!(!crate::block_bad_cmd::codex_without_project_dir(Some(
        "/repo".as_ref()
    )));
}

#[test]
fn a_repeat_denial_is_still_shortened() {
    let dir = TempDir::new().unwrap();
    let full = || deny("cd /elsewhere && make", CdPolicy::Strict).expect("denied");
    let first = condense_at(Some(dir.path()), Some("s2"), full());
    let second = condense_at(Some(dir.path()), Some("s2"), full());
    assert!(second.len() < first.len());
}

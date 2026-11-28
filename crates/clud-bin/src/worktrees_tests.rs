use super::*;

// ----- parse_duration -----

#[test]
fn parse_duration_seconds() {
    assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
}

#[test]
fn parse_duration_minutes() {
    assert_eq!(parse_duration("5m").unwrap(), Duration::from_secs(300));
}

#[test]
fn parse_duration_hours() {
    assert_eq!(parse_duration("2h").unwrap(), Duration::from_secs(7_200));
}

#[test]
fn parse_duration_days() {
    assert_eq!(parse_duration("1d").unwrap(), Duration::from_secs(86_400));
    assert_eq!(
        parse_duration("7d").unwrap(),
        Duration::from_secs(7 * 86_400)
    );
}

#[test]
fn parse_duration_uppercase_unit() {
    assert_eq!(parse_duration("3H").unwrap(), Duration::from_secs(10_800));
}

#[test]
fn parse_duration_trims_whitespace() {
    assert_eq!(
        parse_duration("  1d  ").unwrap(),
        Duration::from_secs(86_400)
    );
}

#[test]
fn parse_duration_rejects_empty() {
    assert!(parse_duration("").is_err());
    assert!(parse_duration("   ").is_err());
}

#[test]
fn parse_duration_rejects_zero() {
    assert!(parse_duration("0d").is_err());
    assert!(parse_duration("0h").is_err());
}

#[test]
fn parse_duration_rejects_missing_unit() {
    assert!(parse_duration("30").is_err());
}

#[test]
fn parse_duration_rejects_missing_value() {
    assert!(parse_duration("d").is_err());
}

#[test]
fn parse_duration_rejects_negative() {
    assert!(parse_duration("-1d").is_err());
}

#[test]
fn parse_duration_rejects_fractional() {
    assert!(parse_duration("1.5d").is_err());
}

#[test]
fn parse_duration_rejects_unknown_unit() {
    for bad in &["1y", "1w", "10x"] {
        assert!(
            parse_duration(bad).is_err(),
            "expected {bad} to be rejected"
        );
    }
}

#[test]
fn parse_duration_rejects_overflow() {
    // 2^64 days * 86400 secs overflows u64.
    assert!(parse_duration("18446744073709551615d").is_err());
}

// ----- parse_worktree_porcelain -----

#[test]
fn porcelain_single_entry() {
    let raw = "\
worktree /repo
HEAD abc123
branch refs/heads/main
";
    let v = parse_worktree_porcelain(raw);
    assert_eq!(v.len(), 1);
    assert_eq!(v[0].path, PathBuf::from("/repo"));
    assert_eq!(v[0].head.as_deref(), Some("abc123"));
    assert_eq!(v[0].branch.as_deref(), Some("main"));
    assert!(!v[0].bare);
    assert!(!v[0].detached);
    assert!(!v[0].locked);
    assert!(!v[0].prunable);
}

#[test]
fn porcelain_multiple_entries_with_locked_and_prunable() {
    let raw = "\
worktree /repo
HEAD aaa
branch refs/heads/main

worktree /tmp/wt-a
HEAD bbb
branch refs/heads/feature/a
locked

worktree /tmp/wt-b
HEAD ccc
branch refs/heads/feature/b
prunable gitdir file points to non-existent location
";
    let v = parse_worktree_porcelain(raw);
    assert_eq!(v.len(), 3);
    assert_eq!(v[0].path, PathBuf::from("/repo"));
    assert!(!v[0].locked);
    assert_eq!(v[1].path, PathBuf::from("/tmp/wt-a"));
    assert!(v[1].locked);
    assert!(!v[1].prunable);
    assert_eq!(v[2].path, PathBuf::from("/tmp/wt-b"));
    assert!(!v[2].locked);
    assert!(v[2].prunable);
}

#[test]
fn porcelain_detached_head() {
    let raw = "\
worktree /repo
HEAD abc123
detached
";
    let v = parse_worktree_porcelain(raw);
    assert_eq!(v.len(), 1);
    assert!(v[0].detached);
    assert!(v[0].branch.is_none());
}

#[test]
fn porcelain_bare_repo() {
    let raw = "\
worktree /srv/repo.git
bare
";
    let v = parse_worktree_porcelain(raw);
    assert_eq!(v.len(), 1);
    assert!(v[0].bare);
}

#[test]
fn porcelain_handles_trailing_newline() {
    let raw = "\
worktree /repo
HEAD abc123
branch refs/heads/main

";
    let v = parse_worktree_porcelain(raw);
    assert_eq!(v.len(), 1);
    assert_eq!(v[0].branch.as_deref(), Some("main"));
}

#[test]
fn porcelain_strips_refs_heads_prefix() {
    let raw = "\
worktree /repo
HEAD abc
branch refs/heads/feature/issue-83
";
    let v = parse_worktree_porcelain(raw);
    assert_eq!(v[0].branch.as_deref(), Some("feature/issue-83"));
}

#[test]
fn porcelain_handles_crlf() {
    let raw = "worktree /repo\r\nHEAD abc\r\nbranch refs/heads/main\r\n\r\n";
    let v = parse_worktree_porcelain(raw);
    assert_eq!(v.len(), 1);
    assert_eq!(v[0].branch.as_deref(), Some("main"));
}

#[test]
fn porcelain_empty_input() {
    assert!(parse_worktree_porcelain("").is_empty());
    assert!(parse_worktree_porcelain("\n\n").is_empty());
}

/// Issue #110: the `locked` porcelain line may carry a reason payload
/// (e.g. `claude agent agent-abf (pid 12345)`). The `gc` liveness probe
/// needs the reason string preserved so it can extract the pid.
#[test]
fn porcelain_captures_locked_reason() {
    let raw = "\
worktree /tmp/wt-with-reason
HEAD aaa
branch refs/heads/feature/x
locked claude agent agent-abf9 (pid 12345)
";
    let v = parse_worktree_porcelain(raw);
    assert_eq!(v.len(), 1);
    assert!(v[0].locked);
    assert_eq!(
        v[0].locked_reason.as_deref(),
        Some("claude agent agent-abf9 (pid 12345)")
    );
}

#[test]
fn porcelain_bare_locked_has_no_reason() {
    let raw = "\
worktree /tmp/wt-bare-lock
HEAD aaa
branch refs/heads/feature/x
locked
";
    let v = parse_worktree_porcelain(raw);
    assert_eq!(v.len(), 1);
    assert!(v[0].locked);
    assert!(v[0].locked_reason.is_none());
}

// ----- decide_action -----

fn opts(stale_after: Duration, force: bool) -> CleanOptions {
    CleanOptions {
        stale_after,
        dry_run: false,
        yes: true,
        force,
    }
}

fn days(n: u64) -> Duration {
    Duration::from_secs(86_400 * n)
}

fn inputs(status: WorktreeStatus, age: Duration) -> StalenessInputs {
    StalenessInputs {
        status,
        age,
        lock_status: LockStatus::Unlocked,
        locked_hard_age: days(7),
    }
}

fn locked_inputs(
    status: WorktreeStatus,
    age: Duration,
    lock_status: LockStatus,
) -> StalenessInputs {
    StalenessInputs {
        status,
        age,
        lock_status,
        locked_hard_age: days(7),
    }
}

#[test]
fn decide_clean_and_old_is_removed() {
    let act = decide_action(
        inputs(WorktreeStatus::Clean, days(2)),
        &opts(Duration::from_secs(86_400), false),
    );
    assert!(matches!(act, Action::Remove(_)));
}

#[test]
fn decide_clean_but_fresh_is_ignored() {
    let act = decide_action(
        inputs(WorktreeStatus::Clean, Duration::from_secs(60)),
        &opts(Duration::from_secs(86_400), false),
    );
    assert_eq!(act, Action::Ignore);
}

#[test]
fn decide_branch_gone_is_always_removed() {
    // Even when fresh, a [gone] upstream → remove.
    let act = decide_action(
        inputs(WorktreeStatus::BranchGone, Duration::from_secs(1)),
        &opts(Duration::from_secs(86_400), false),
    );
    assert!(matches!(act, Action::Remove(_)));
}

#[test]
fn decide_dirty_without_force_is_skipped() {
    let act = decide_action(
        inputs(WorktreeStatus::Dirty, days(30)),
        &opts(Duration::from_secs(86_400), false),
    );
    assert!(matches!(act, Action::Skip(_)));
}

#[test]
fn decide_dirty_with_force_is_removed() {
    let act = decide_action(
        inputs(WorktreeStatus::Dirty, days(30)),
        &opts(Duration::from_secs(86_400), true),
    );
    assert!(matches!(act, Action::Remove(_)));
}

#[test]
fn decide_unpushed_without_force_is_skipped() {
    let act = decide_action(
        inputs(WorktreeStatus::Unpushed, days(30)),
        &opts(Duration::from_secs(86_400), false),
    );
    assert!(matches!(act, Action::Skip(_)));
}

#[test]
fn decide_unpushed_with_force_is_removed() {
    let act = decide_action(
        inputs(WorktreeStatus::Unpushed, days(30)),
        &opts(Duration::from_secs(86_400), true),
    );
    assert!(matches!(act, Action::Remove(_)));
}

#[test]
fn decide_no_upstream_fresh_is_ignored() {
    let act = decide_action(
        inputs(WorktreeStatus::NoUpstream, Duration::from_secs(60)),
        &opts(Duration::from_secs(86_400), false),
    );
    assert_eq!(act, Action::Ignore);
}

#[test]
fn decide_no_upstream_stale_skipped_without_force() {
    let act = decide_action(
        inputs(WorktreeStatus::NoUpstream, days(30)),
        &opts(Duration::from_secs(86_400), false),
    );
    assert!(matches!(act, Action::Skip(_)));
}

#[test]
fn decide_no_upstream_stale_removed_with_force() {
    let act = decide_action(
        inputs(WorktreeStatus::NoUpstream, days(30)),
        &opts(Duration::from_secs(86_400), true),
    );
    assert!(matches!(act, Action::Remove(_)));
}

#[test]
fn decide_fresh_live_locks_are_skipped_even_with_force() {
    for status in [
        WorktreeStatus::Clean,
        WorktreeStatus::Dirty,
        WorktreeStatus::Unpushed,
        WorktreeStatus::NoUpstream,
        WorktreeStatus::BranchGone,
    ] {
        let act = decide_action(
            locked_inputs(status, days(7), LockStatus::LivePid(12345)),
            &opts(Duration::from_secs(86_400), true),
        );
        assert!(
            matches!(act, Action::Skip(_)),
            "live locked + {status:?} must be Skip, got {act:?}"
        );
    }
}

#[test]
fn decide_stale_live_lock_can_remove_clean_worktree_after_hard_age() {
    let act = decide_action(
        locked_inputs(WorktreeStatus::Clean, days(8), LockStatus::LivePid(12345)),
        &opts(Duration::from_secs(86_400), false),
    );
    let Action::Remove(reason) = act else {
        panic!("expected hard-aged live lock to be removable when clean, got {act:?}");
    };
    assert!(reason.contains("stale lock (pid 12345 still alive; hard age exceeded)"));
    assert!(reason.contains("clean + stale"));
}

#[test]
fn decide_fresh_dead_or_unknown_locks_are_skipped() {
    for lock_status in [LockStatus::DeadPid(12345), LockStatus::NoPid] {
        let act = decide_action(
            locked_inputs(WorktreeStatus::BranchGone, days(7), lock_status),
            &opts(Duration::from_secs(86_400), true),
        );
        assert!(
            matches!(act, Action::Skip(_)),
            "{lock_status:?} at hard-age boundary must be skipped, got {act:?}"
        );
    }
}

#[test]
fn decide_stale_dead_lock_can_remove_clean_worktree() {
    let act = decide_action(
        locked_inputs(WorktreeStatus::Clean, days(8), LockStatus::DeadPid(12345)),
        &opts(Duration::from_secs(86_400), false),
    );
    let Action::Remove(reason) = act else {
        panic!("expected stale dead lock to be removable when clean, got {act:?}");
    };
    assert!(reason.contains("stale lock (dead pid 12345)"));
    assert!(reason.contains("clean + stale"));
}

#[test]
fn decide_stale_no_pid_lock_can_remove_branch_gone_worktree() {
    let act = decide_action(
        locked_inputs(WorktreeStatus::BranchGone, days(8), LockStatus::NoPid),
        &opts(Duration::from_secs(86_400), false),
    );
    let Action::Remove(reason) = act else {
        panic!("expected stale no-pid lock to be removable when branch is gone, got {act:?}");
    };
    assert!(reason.contains("stale lock (no pid)"));
    assert!(reason.contains("upstream branch gone"));
}

#[test]
fn decide_stale_dead_lock_still_requires_force_for_dirty_worktree() {
    let without_force = decide_action(
        locked_inputs(WorktreeStatus::Dirty, days(8), LockStatus::DeadPid(12345)),
        &opts(Duration::from_secs(86_400), false),
    );
    assert!(matches!(without_force, Action::Skip(_)));

    let with_force = decide_action(
        locked_inputs(WorktreeStatus::Dirty, days(8), LockStatus::DeadPid(12345)),
        &opts(Duration::from_secs(86_400), true),
    );
    assert!(matches!(with_force, Action::Remove(_)));
}

#[test]
fn locked_hard_age_defaults_to_seven_days() {
    assert_eq!(
        worktrees_locked_age::locked_hard_age_from_raw(None),
        days(7)
    );
    assert_eq!(
        worktrees_locked_age::locked_hard_age_from_raw(Some("3")),
        days(3)
    );
    assert_eq!(
        worktrees_locked_age::locked_hard_age_from_raw(Some("not-a-number")),
        days(7)
    );
}

fn classified_locked(
    name: &str,
    status: WorktreeStatus,
    age: Duration,
    reason: Option<&str>,
) -> Classified {
    Classified {
        entry: WorktreeEntry {
            path: PathBuf::from(format!("/tmp/{name}")),
            head: None,
            branch: None,
            bare: false,
            detached: false,
            locked: true,
            locked_reason: reason.map(str::to_string),
            prunable: false,
        },
        status,
        age,
        is_main: false,
        verdict_row: None,
        verdict_timed_out: false,
    }
}

#[test]
fn build_plan_uses_lock_reason_pid_liveness() {
    let rows = vec![
        classified_locked(
            "live",
            WorktreeStatus::Clean,
            days(6),
            Some("claude agent agent-live (pid 111)"),
        ),
        classified_locked(
            "dead",
            WorktreeStatus::Clean,
            days(8),
            Some("claude agent agent-dead (pid 222)"),
        ),
    ];
    let probe = crate::session_registry::MockLivenessProbe::with_alive([111]);
    let plan = build_plan_with_liveness(&rows, &opts(days(1), false), &probe, days(7));

    assert_eq!(plan.candidates.len(), 1);
    assert!(plan.candidates[0].entry.path.ends_with("dead"));
    assert!(plan.candidates[0].reason.contains("dead pid 222"));
    assert_eq!(plan.skipped.len(), 1);
    assert!(plan.skipped[0].entry.path.ends_with("live"));
    assert!(plan.skipped[0].reason.contains("live pid 111"));
}

#[test]
fn worktree_process_ref_matches_executable_or_command_line() {
    let worktree = Path::new(r"C:\repo\.claude\worktrees\fix-branch");

    assert!(process_ref_matches_worktree(
        worktree,
        Some(Path::new(
            r"C:\repo\.claude\worktrees\fix-branch\target\debug\tool.exe"
        )),
        None,
        &[r"C:/repo/.claude/worktrees/fix-branch/target/debug/tool.exe".into()]
    ));
    assert!(process_ref_matches_worktree(
        worktree,
        None,
        None,
        &[
            "python".into(),
            r"C:/repo/.claude/worktrees/fix-branch/scripts/check.py".into()
        ]
    ));
    assert!(process_ref_matches_worktree(
        worktree,
        None,
        Some(Path::new(r"\\?\C:\repo\.claude\worktrees\fix-branch")),
        &["python".into()]
    ));
    assert!(!process_ref_matches_worktree(
        worktree,
        Some(Path::new(
            r"C:\repo\.claude\worktrees\other-branch\tool.exe"
        )),
        None,
        &[
            "python".into(),
            r"C:/repo/.claude/worktrees/other-branch/check.py".into()
        ]
    ));
}

#[test]
fn remove_worktree_path_refuses_when_live_process_references_worktree() {
    let tmp = kernal_api::platform::fs::TemporaryDirectory::new().expect("tempdir");
    let repo = tmp.path().join("repo");
    let wt = tmp.path().join("wt");
    std::fs::create_dir_all(&repo).expect("repo dir");
    std::fs::create_dir_all(&wt).expect("worktree dir");

    let err = remove_worktree_path_with_git_and_process_refs(
        &repo,
        &wt,
        false,
        |_, _| panic!("git should not run while process refs exist"),
        |_| {
            vec![WorktreeProcessRef {
                pid: 4242,
                parent_pid: Some(4000),
                name: "fbuild-daemon".to_string(),
                command: format!("fbuild-daemon --root {}", wt.display()),
                exe: None,
                cwd: Some(wt.clone()),
            }]
        },
    )
    .expect_err("live process ref should block worktree removal");

    assert!(err.contains("live process"));
    assert!(err.contains("4242"));
    assert!(wt.exists());
}

#[test]
fn unforced_worktree_removal_never_deletes_source_left_after_git_success() {
    let tmp = kernal_api::platform::fs::TemporaryDirectory::new().expect("tempdir");
    let repo = tmp.path().join("repo");
    let wt = tmp.path().join("wt");
    std::fs::create_dir_all(&repo).expect("repo dir");
    std::fs::create_dir_all(&wt).expect("worktree dir");
    std::fs::write(wt.join("file.txt"), "content").expect("worktree file");

    let calls = std::cell::RefCell::new(Vec::<Vec<String>>::new());
    let outcome = remove_worktree_path_with_git(&repo, &wt, false, |cwd, args| {
        assert_eq!(cwd, repo.as_path());
        calls
            .borrow_mut()
            .push(args.iter().map(|s| s.to_string()).collect());
        Ok(String::new())
    })
    .expect_err("unforced removal must not erase a recreated source directory");

    assert!(outcome.contains("refusing recursive fallback"));
    assert!(wt.join("file.txt").exists());
    let calls = calls.into_inner();
    assert_eq!(
        calls.len(),
        1,
        "no prune or recursive fallback after the refusal"
    );
}

#[test]
fn remove_worktree_path_falls_back_when_git_remove_fails_and_dir_remains() {
    let tmp = kernal_api::platform::fs::TemporaryDirectory::new().expect("tempdir");
    let repo = tmp.path().join("repo");
    let wt = tmp.path().join("wt");
    std::fs::create_dir_all(&repo).expect("repo dir");
    std::fs::create_dir_all(&wt).expect("worktree dir");

    let wt_arg = wt.to_string_lossy().to_string();
    let calls = std::cell::RefCell::new(Vec::<Vec<String>>::new());
    let outcome = remove_worktree_path_with_git(&repo, &wt, true, |_, args| {
        calls
            .borrow_mut()
            .push(args.iter().map(|s| s.to_string()).collect());
        if args == ["worktree", "remove", "--force", wt_arg.as_str()] {
            Err("locked worktree".to_string())
        } else {
            Ok(String::new())
        }
    })
    .expect("fallback should remove directory and prune");

    assert_eq!(outcome, RemovalOutcome::FallbackAfterGitFailure);
    assert!(!wt.exists());
    let calls = calls.into_inner();
    assert_eq!(
        calls[0],
        vec![
            "worktree".to_string(),
            "remove".to_string(),
            "--force".to_string(),
            wt.to_string_lossy().to_string()
        ]
    );
    assert_eq!(
        calls[1],
        vec![
            "worktree".to_string(),
            "unlock".to_string(),
            wt.to_string_lossy().to_string()
        ]
    );
    assert_eq!(
        calls[2],
        vec![
            "worktree".to_string(),
            "prune".to_string(),
            "--expire=now".to_string()
        ]
    );
}

#[test]
fn fmt_age_picks_largest_unit() {
    assert_eq!(fmt_age(Duration::from_secs(5)), "5s");
    assert_eq!(fmt_age(Duration::from_secs(90)), "1m");
    assert_eq!(fmt_age(Duration::from_secs(3700)), "1h");
    assert_eq!(fmt_age(Duration::from_secs(86_400 * 3)), "3d");
}

#[test]
fn ancestry_cleanup_checks_container_binds_before_attempting_git() {
    let tmp = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let source = tmp.path().join("source");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("work"), "active").unwrap();
    let result = remove_worktree_path_if_unused(tmp.path(), &source, true, &|_| {
        Some("pinned: container bind references source tree")
    });
    assert_eq!(
        result,
        Err("pinned: container bind references source tree".into())
    );
    assert!(source.join("work").exists());
}

#[test]
fn unforced_worktree_removal_preserves_source_after_git_refusal() {
    let tmp = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let source = tmp.path().join("source");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("uncommitted"), "new work").unwrap();
    let error = remove_worktree_path_with_git(tmp.path(), &source, false, |_, args| {
        assert!(!args.contains(&"--force"));
        Err("worktree is dirty".into())
    })
    .unwrap_err();
    assert_eq!(error, "worktree is dirty");
    assert!(source.join("uncommitted").exists());
}

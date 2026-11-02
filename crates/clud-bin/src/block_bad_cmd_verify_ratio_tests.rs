use super::*;

#[test]
fn calls_are_classified_by_tool_and_command() {
    assert_eq!(classify("Edit", ""), Some(Class::Edit));
    assert_eq!(classify("Write", ""), Some(Class::Edit));
    assert_eq!(
        classify("Bash", "soldr cargo clippy --workspace --all-targets"),
        Some(Class::Verify)
    );
    assert_eq!(
        classify("Bash", "cargo test --release -p core"),
        Some(Class::Verify)
    );
    assert_eq!(
        classify("Bash", "bosn ci run --workspace . --wait"),
        Some(Class::Verify)
    );
    assert_eq!(
        classify("Bash", "sed -i 's/a/b/' x.rs && cargo build"),
        Some(Class::Edit)
    );
    assert_eq!(classify("Bash", "git status"), None);
    assert_eq!(classify("Read", ""), None);
}

#[test]
fn python_source_writes_count_as_edits_without_counting_reads_or_text() {
    let repository = tempfile::tempdir().unwrap();
    std::fs::create_dir(repository.path().join(".git")).unwrap();
    std::fs::create_dir_all(repository.path().join("crates/clud-bin/src")).unwrap();
    std::fs::create_dir(repository.path().join("src")).unwrap();
    std::fs::write(
        repository.path().join("crates/clud-bin/src/example.rs"),
        "old",
    )
    .unwrap();
    let writes = [
        "python - <<'PY'\np = 'crates/clud-bin/src/example.rs'\nopen(p, 'w').write('new')\nPY",
        "python - <<'PY'\np = 'crates/clud-bin/src/example.rs'\ns = open(p).read()\ns = s.replace('old', 'new')\nopen(p, 'w').write(s)\nPY",
        "python - <<'PY'\nfrom pathlib import Path\nPath('crates/clud-bin/src/example.rs').write_text('new')\nPY",
        "python - <<'PY'\nfrom pathlib import Path\nPath('crates/clud-bin/src/example.rs').write_bytes(b'new')\nPY",
        "python -c \"p='src/a.rs'; open(p,'w')\"",
        "python -c \"from pathlib import Path; Path('src/a.rs').write_text('x')\"",
        "python -c \"open('src/a.rs','w');\"",
        "python -c \"open('src/a.rs','a')\"",
        "python -c \"open('src/a.rs','x')\"",
        "python -c \"open('src/a.rs','wb')\"",
        "python3 -c \"open('src/a.rs','w')\"", // python-name-lint: allow -- external agent command compatibility
    ];
    for command in writes {
        assert_eq!(
            classify_kind_at("Bash", command, Some(repository.path())),
            Some(Kind::Edit),
            "{command}"
        );
    }
    let non_edits = [
        "python - <<'PY'\nfrom pathlib import Path\nprint(Path('crates/clud-bin/src/example.rs').read_text())\nPY",
        "python - <<'PY'\np = 'crates/clud-bin/src/example.rs'\ns = open(p).read()\ns = s.replace('old', 'new')\nPY",
        "python - <<'PY'\nprint(\"open('crates/clud-bin/src/example.rs', 'w')\")\nPY",
        "python - <<'PY'\n# Path('crates/clud-bin/src/example.rs').write_text('x')\nprint('read only')\nPY",
        "python - <<'PY'\nfrom pathlib import Path\nPath('/tmp/report.log').write_text('diagnostic')\nPY",
        "python - <<'PY'\nopen(dynamic_path, 'w').write('unknown')\nPY",
        "bosn ci report 123 --json",
        "python -c \"m='w'; open('src/a.rs', m)\"",
        "python -c \"import json; json.load(open('src/a.rs'))\"",
        "python -c \"open('src/a.rs','r')\"",
        "python -c \"p='src/a.rs' + '.tmp'; open(p, 'w')\"",
        "python -c \"p='src/a.rs'; p += '.tmp'; open(p, 'w')\"",
        "python -c \"False and open('src/a.rs','w')\"",
        "python -c \"raise SystemExit; open('src/a.rs','w')\"",
        "python -c \"obj.open('src/a.rs','w')\"",
        "python -c \"open(p,'w'); p='src/a.rs'\"",
        "python -c \"p='src/a.rs'; del p; open(p,'w')\"",
        "python -c \"1/0; open('src/a.rs','w')\"",
        "python -c \"1%0; open('src/a.rs','w')\"",
        "python -c \"; open('src/a.rs','w')\"",
        "python -c \"open('src/a.rs','w');;\"",
        "python -c \"missing_function(); open('src/a.rs','w')\"",
        "python -c \"open('src/a.rs','w'); missing(\"",
        "false; python -c \"open('src/a.rs','w')\"",
        "python -c \"from pathlib import Path; obj.Path('src/a.rs').write_text('x')\"",
        "python -c \"from pathlib import Path; Path.write_text = print; Path('src/a.rs').write_text('x')\"",
        "python -c \"from pathlib import Path; Path('src/a.rs').write_text(b'x')\"",
        "python -c \"from pathlib import Path; Path('src/a.rs').write_bytes('x')\"",
        "python -c \"from pathlib import Path; Path('src/a.rs').write_bytes(b 'x')\"",
        "python -c \"from pathlib import Path; Path('src/a.rs').write_bytes(b'é')\"",
        "python - <<'PY' </dev/null\nopen('src/a.rs','w')\nPY",
        "python - 3<<'PY'\nopen('src/a.rs','w')\nPY",
        "python - <<'PY'\np='x'\n  open('src/a.rs','w')\nPY",
        "python - <<'PY'\np='a\nb'\nopen('src/a.rs','w')\nPY",
        "python -c \"__debug__='src/a.rs'; open(__debug__,'w')\"",
        "python -c \"from pathlib import Path as Fake; Path('src/a.rs').write_text('x')\"",
        "python -c \"from fake import open; open('src/a.rs','w')\"",
        "false && python -c \"open('src/a.rs','w')\"",
        "python - <<PY\nopen('src/$NAME.rs','w')\nPY",
        "python -c \"open('src/a.rs','w')\" </definitely/nonexistent/file",
    ];
    for command in non_edits {
        assert_eq!(
            classify_kind_at("Bash", command, Some(repository.path())),
            Some(Kind::Other),
            "{command}"
        );
    }
    let outside = tempfile::tempdir().unwrap();
    assert_eq!(
        classify_kind_at("Bash", writes[0], Some(outside.path())),
        Some(Kind::Other)
    );
    let oversized = format!("python -c \"open('src/a.rs','w'); {}\"", "x".repeat(40_000));
    assert_eq!(
        classify_kind_at("Bash", &oversized, Some(repository.path())),
        Some(Kind::Other)
    );
}

#[test]
fn python_attempt_recognition_uses_the_payload_shell_dialect() {
    let repository = tempfile::tempdir().unwrap();
    std::fs::create_dir(repository.path().join(".git")).unwrap();
    std::fs::create_dir(repository.path().join("src")).unwrap();
    let command = "python -c \"open('src/a.rs','w')\"";
    let payload = |tool: &str| {
        super::super::parse_payload_value(
            &serde_json::json!({"tool_name": tool, "tool_input": {"command": command}}),
            repository.path(),
        )
        .unwrap()
    };
    assert_eq!(classify_payload(&payload("Bash")), Some(Kind::Edit));
    assert_eq!(classify_payload(&payload("PowerShell")), Some(Kind::Other));
}

#[test]
fn python_edit_attempt_rejects_paths_through_external_or_dangling_symlinks() {
    let repository = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::create_dir(repository.path().join(".git")).unwrap();
    std::fs::create_dir(repository.path().join("src")).unwrap();
    match crate::platform::gc_test_fs::symlink(
        outside.path(),
        repository.path().join("src/external"),
    ) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::Unsupported => return,
        Err(error) => panic!("create external symlink: {error}"),
    }
    crate::platform::gc_test_fs::symlink(
        outside.path().join("missing.rs"),
        repository.path().join("src/dangling.rs"),
    )
    .unwrap();
    for command in [
        "python -c \"open('src/external/a.rs','w')\"",
        "python -c \"open('src/dangling.rs','w')\"",
    ] {
        assert_eq!(
            classify_kind_at("Bash", command, Some(repository.path())),
            Some(Kind::Other)
        );
    }
}

fn run(ledger: &mut Ledger, classes: &str, ratio: u32) -> Vec<Counts> {
    let mut notes = Vec::new();
    for letter in classes.chars() {
        let class = if letter == 'e' {
            Class::Edit
        } else {
            Class::Verify
        };
        let (next, note) = decide(ledger, class, ratio);
        *ledger = next;
        notes.extend(note);
    }
    notes
}

#[test]
fn verification_crowding_out_edits_trips_one_note_then_stays_quiet() {
    let mut ledger = Ledger::default();
    // #1988's session: one edit, then the whole gate, over and over.
    let notes = run(&mut ledger, "evvvvevvvvevv", DEFAULT_RATIO);
    assert_eq!(
        notes,
        vec![Counts {
            edits: 3,
            verifies: 10
        }]
    );
    // The same pattern right after the note is not repeated within a window.
    assert!(run(&mut ledger, "vvvvvvvvvvvvvvvvvvvvvvvvvvvvv", DEFAULT_RATIO).is_empty());
    assert_eq!(run(&mut ledger, "v", DEFAULT_RATIO).len(), 1);
}

#[test]
fn a_balanced_loop_a_short_session_or_ratio_zero_never_notes() {
    let mut ledger = Ledger::default();
    assert!(run(&mut ledger, &"evv".repeat(20), DEFAULT_RATIO).is_empty());
    let mut ledger = Ledger::default();
    assert!(run(&mut ledger, "vvvvvvvvv", DEFAULT_RATIO).is_empty());
    let mut ledger = Ledger::default();
    assert!(run(&mut ledger, &"v".repeat(60), 0).is_empty());
}

#[test]
fn the_window_keeps_only_the_newest_calls() {
    let mut ledger = Ledger::default();
    run(&mut ledger, &"e".repeat(40), DEFAULT_RATIO);
    assert_eq!(ledger.window.len(), WINDOW);
}

#[test]
fn the_ratio_comes_from_env_then_settings_then_default() {
    let settings = serde_json::json!({"hooks": {"verify_edit_ratio": 5}});
    assert_eq!(ratio_from(Some("0"), Some(&settings)), 0);
    assert_eq!(ratio_from(None, Some(&settings)), 5);
    assert_eq!(ratio_from(Some("junk"), None), DEFAULT_RATIO);
}

#[test]
fn state_round_trips_per_session_and_fails_open_without_one() {
    let dir = tempfile::tempdir().unwrap();
    for _ in 0..MIN_VERIFY - 1 {
        assert_eq!(observe_at(dir.path(), "s1", Class::Verify, 3), None);
    }
    assert!(observe_at(dir.path(), "s1", Class::Verify, 3).is_some());
    assert_eq!(observe_at(dir.path(), "s2", Class::Verify, 3), None);
    assert_eq!(observe_at(dir.path(), "", Class::Verify, 3), None);
}

#[test]
fn the_note_is_context_not_a_permission_decision() {
    let json = context_json(&note_text(
        Counts {
            edits: 1,
            verifies: 12,
        },
        3,
    ));
    let output = &json["hookSpecificOutput"];
    assert!(output.get("permissionDecision").is_none());
    assert!(output["additionalContext"]
        .as_str()
        .unwrap()
        .contains("12 lint/build/test/CI calls"));
}

const LIMITS: Limits = Limits {
    ratio: DEFAULT_RATIO,
    share: DEFAULT_SHARE,
};

fn call(kind: Kind, now_ms: u64) -> Call {
    Call {
        kind,
        wide: false,
        workspace_lint: false,
        package: None,
        id: None,
        dedupe_id: None,
        now_ms,
    }
}

#[test]
fn python_write_in_a_mixed_sequence_counts_as_an_edit() {
    let repository = tempfile::tempdir().unwrap();
    std::fs::create_dir(repository.path().join(".git")).unwrap();
    std::fs::create_dir(repository.path().join("src")).unwrap();
    let cwd = Some(repository.path());
    let mut ledger = Ledger::default();
    let steps = [
        "soldr cargo test -p a",
        "python - <<'PY'\nopen('src/a.rs', 'w').write('x')\nPY",
        "soldr cargo clippy -p a",
    ];
    for command in steps {
        let kind = classify_kind_at("Bash", command, cwd).unwrap();
        ledger = observe(&ledger, &call(kind, 1), LIMITS).0;
    }
    assert_eq!(ledger.window, "vev");
}

#[test]
fn a_redelivered_tool_call_is_counted_once() {
    let mut first = call(Kind::Test, 1);
    first.dedupe_id = Some("toolu_1".to_string());
    let (once, _) = observe(&Ledger::default(), &first, LIMITS);
    let (twice, verdict) = observe(&once, &first, LIMITS);
    assert_eq!(once, twice);
    assert_eq!(verdict, None);
    let mut other = first.clone();
    other.dedupe_id = Some("toolu_2".to_string());
    assert_eq!(observe(&twice, &other, LIMITS).0.window, "vv");
}

#[test]
fn concurrent_hook_invocations_do_not_lose_ledger_updates() {
    let dir = tempfile::tempdir().unwrap();
    let handles: Vec<_> = (0..8)
        .map(|n| {
            let state = dir.path().to_path_buf();
            std::thread::spawn(move || {
                let mut c = call(Kind::Test, 1);
                c.dedupe_id = Some(format!("toolu_{n}"));
                observe_call_at(&state, "s1", &c, LIMITS)
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap();
    }
    let (_, ledger) = load(&session_file(dir.path(), "s1"));
    assert_eq!(ledger.window.len(), 8);
}

#[test]
fn kinds_split_verification_for_the_stats_view() {
    assert_eq!(classify_kind("Bash", "cargo clippy -p x"), Some(Kind::Lint));
    assert_eq!(classify_kind("Bash", "cargo build"), Some(Kind::Build));
    assert_eq!(classify_kind("Bash", "pytest tests/x.py"), Some(Kind::Test));
    assert_eq!(
        classify_kind("Bash", "bosn ci run --job unit"),
        Some(Kind::Ci)
    );
    assert_eq!(classify_kind("Bash", "git status"), Some(Kind::Other));
    assert_eq!(classify_kind("Read", ""), None);
    assert_eq!(classify("Bash", "git status"), None);
}

#[test]
fn wide_flags_packages_and_workspace_lint_are_recognized() {
    assert!(is_wide("cargo clippy --workspace --all-targets"));
    assert!(is_wide("cargo test --release -p x"));
    assert!(!is_wide("cargo clippy -p x --all-targets"));
    assert_eq!(
        package_of("/r/crates/clud-bin/src/a.rs").as_deref(),
        Some("clud-bin")
    );
    assert_eq!(package_of("docs/a.md"), None);
    assert!(is_workspace_lint(Kind::Lint, "bash lint"));
    assert!(is_workspace_lint(Kind::Ci, "bosn ci run --workspace ."));
    assert!(!is_workspace_lint(Kind::Ci, "bosn ci run --job unit"));
    assert!(!is_workspace_lint(Kind::Test, "cargo test --workspace"));
}

#[test]
fn a_post_tool_use_adds_the_elapsed_time_to_the_calls_kind() {
    let mut test = call(Kind::Test, 1_000);
    test.id = Some("t1".into());
    let (ledger, _) = observe(&Ledger::default(), &test, LIMITS);
    let ledger = finish(&ledger, "t1", 61_000).unwrap();
    assert_eq!(
        ledger.totals[&Kind::Test],
        Total {
            calls: 1,
            ms: 60_000
        }
    );
    assert!(ledger.pending.is_empty());
    assert_eq!(
        finish(&ledger, "t1", 99_000),
        None,
        "a second finish is a no-op"
    );
    assert_eq!(finish(&ledger, "unknown", 99_000), None);
}

#[test]
fn verification_holding_most_of_the_bash_wall_time_trips_one_note() {
    let mut ledger = Ledger::default();
    // 3 min of other shell work, 8 min of tests: 72% of 11 min.
    for (n, (kind, ms)) in [(Kind::Other, 180_000), (Kind::Test, 480_000)]
        .into_iter()
        .enumerate()
    {
        let mut c = call(kind, 0);
        c.id = Some(format!("c{n}"));
        ledger = observe(&ledger, &c, LIMITS).0;
        ledger = finish(&ledger, &format!("c{n}"), ms).unwrap();
    }
    assert_eq!(verify_share(&ledger.totals), Some(72));
    // A balanced window, so only the share signal can trip.
    ledger.window = "ev".repeat(5);
    let (ledger, verdict) = observe(&ledger, &call(Kind::Test, 0), LIMITS);
    assert!(
        matches!(&verdict, Some(Verdict::Note(n)) if n.contains("72%")),
        "{verdict:?}"
    );
    assert_eq!(observe(&ledger, &call(Kind::Test, 0), LIMITS).1, None);
    let off = Limits { share: 0, ..LIMITS };
    let mut fresh = ledger.clone();
    fresh.quiet = 0;
    assert_eq!(observe(&fresh, &call(Kind::Test, 0), off).1, None);
}

#[test]
fn the_share_is_not_judged_before_ten_minutes_of_bash() {
    let mut totals = BTreeMap::new();
    totals.insert(
        Kind::Test,
        Total {
            calls: 1,
            ms: 300_000,
        },
    );
    assert_eq!(verify_share(&totals), None);
}

#[test]
fn repeated_wide_runs_after_single_package_edits_trip_the_modifier_note() {
    let mut ledger = Ledger::default();
    let mut notes = Vec::new();
    for _ in 0..MODIFIER_REPEATS {
        let mut edit = call(Kind::Edit, 0);
        edit.package = Some("clud-bin".into());
        ledger = observe(&ledger, &edit, LIMITS).0;
        let mut wide = call(Kind::Lint, 0);
        wide.wide = true;
        let (next, verdict) = observe(&ledger, &wide, LIMITS);
        ledger = next;
        notes.extend(verdict);
    }
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(matches!(&notes[0], Verdict::Note(n) if n.contains("-p clud-bin")));
}

#[test]
fn wide_runs_after_multi_package_edits_are_fine() {
    let mut ledger = Ledger::default();
    for _ in 0..5 {
        for package in ["a", "b"] {
            let mut edit = call(Kind::Edit, 0);
            edit.package = Some(package.into());
            ledger = observe(&ledger, &edit, LIMITS).0;
        }
        let mut wide = call(Kind::Lint, 0);
        wide.wide = true;
        let (next, verdict) = observe(&ledger, &wide, LIMITS);
        ledger = next;
        assert_eq!(verdict, None);
    }
}

#[test]
fn after_a_note_the_next_workspace_lint_is_denied_exactly_once() {
    let mut ledger = Ledger::default();
    let mut verdicts = Vec::new();
    for letter in "evvvvevvvvevv".chars() {
        let kind = if letter == 'e' {
            Kind::Edit
        } else {
            Kind::Test
        };
        let (next, verdict) = observe(&ledger, &call(kind, 0), LIMITS);
        ledger = next;
        verdicts.extend(verdict);
    }
    assert!(
        matches!(verdicts.as_slice(), [Verdict::Note(_)]),
        "{verdicts:?}"
    );
    let mut lint = call(Kind::Lint, 0);
    lint.workspace_lint = true;
    let (ledger, verdict) = observe(&ledger, &lint, LIMITS);
    assert!(matches!(verdict, Some(Verdict::Deny(_))), "{verdict:?}");
    // Repeating the command runs it: the deny is spent.
    let (_, verdict) = observe(&ledger, &lint, LIMITS);
    assert_eq!(verdict, None);
}

#[test]
fn no_note_means_no_escalation() {
    let mut lint = call(Kind::Lint, 0);
    lint.workspace_lint = true;
    let mut ledger = Ledger::default();
    for _ in 0..5 {
        let (next, verdict) = observe(&ledger, &lint, LIMITS);
        ledger = next;
        assert_eq!(verdict, None);
    }
}

#[test]
fn a_1988_ledger_without_the_new_fields_still_loads() {
    let old: Ledger = serde_json::from_str(r#"{"window":"evv","quiet":2}"#).unwrap();
    assert_eq!(old.window, "evv");
    assert!(old.totals.is_empty() && !old.noted);
}

#[test]
fn timing_and_stats_round_trip_through_the_session_file() {
    let dir = tempfile::tempdir().unwrap();
    let mut test = call(Kind::Test, 0);
    test.id = Some("t1".into());
    observe_call_at(dir.path(), "s1", &test, LIMITS);
    finish_at(dir.path(), "s1", "t1", 90_000).unwrap();
    let mut edit = call(Kind::Edit, 0);
    edit.package = Some("x".into());
    observe_call_at(dir.path(), "s1", &edit, LIMITS);
    let text = stats_at(dir.path(), "s1").unwrap();
    assert!(text.contains("test        1      1m30s    100%"), "{text}");
    assert!(text.contains("edit        1          -       -"), "{text}");
    assert!(text.contains("bash wall time: 1m30s"), "{text}");
    assert_eq!(stats_at(dir.path(), "nobody"), None);
    assert_eq!(finish_at(dir.path(), "nobody", "t1", 1), None);
}

#[test]
fn the_share_limit_comes_from_env_then_settings_then_default() {
    let settings = serde_json::json!({"hooks": {"verify_wall_share": 50}});
    assert_eq!(share_from(Some("0"), Some(&settings)), 0);
    assert_eq!(share_from(None, Some(&settings)), 50);
    assert_eq!(share_from(None, None), DEFAULT_SHARE);
}

#[test]
fn pruning_ledgers_never_removes_a_sessions_lock() {
    let dir = tempfile::tempdir().expect("tempdir");
    let ledger = session_file(dir.path(), "s1");
    let lock = lock_path(&ledger);
    drop(SessionLock::acquire(&ledger));
    assert!(lock.exists());
    assert_ne!(lock.parent(), ledger.parent());
    std::fs::create_dir_all(ledger.parent().expect("dir")).expect("mkdir");
    std::fs::write(&ledger, b"{}").expect("ledger");
    let far_future = SystemTime::now() + std::time::Duration::from_secs(30 * 24 * 3600);
    prune(ledger.parent().expect("dir"), far_future);
    assert!(!ledger.exists(), "old ledger is pruned");
    assert!(lock.exists(), "lock survives the prune");
}

#[test]
fn posting_text_that_mentions_lint_is_not_a_lint_run_2470() {
    let cmd = "printf 'ran bash lint && cargo clippy --workspace; all green\\n' | gh issue comment 2033 --body-file -";
    assert_eq!(classify("Bash", cmd), None);
    assert!(!is_workspace_lint(Kind::Other, cmd));
    assert_eq!(
        classify("Bash", "echo \"bash lint\" && bash lint"),
        Some(Class::Verify)
    );
    assert_eq!(
        classify("Bash", "bash -c \"cargo clippy --workspace\""),
        Some(Class::Verify)
    );
}

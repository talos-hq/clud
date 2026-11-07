use super::*;

#[test]
fn models_cheapest_json_is_a_known_self_contained_command() {
    let args = parse(&["clud", "models", "cheapest", "--json"]);
    match args.command {
        Some(Command::Models {
            subcommand: ModelsSubcommand::Cheapest { json },
        }) => assert!(json),
        other => panic!("expected models cheapest --json, got {other:?}"),
    }
    assert!(args.passthrough.is_empty());
}

#[test]
fn test_clean_worktrees_with_dry_run() {
    let args = parse(&["clud", "--clean-worktrees", "--dry-run"]);
    assert!(args.clean_worktrees);
    assert!(args.dry_run);
}

#[test]
fn test_fix_hooks_flag() {
    let args = parse(&["clud", "--fix-hooks"]);
    assert!(args.fix_hooks);
    assert!(!args.no_fix_hooks);
    assert!(!args.dry_run);
}

#[test]
fn test_no_fix_hooks_flag() {
    let args = parse(&["clud", "--no-fix-hooks"]);
    assert!(args.no_fix_hooks);
    assert!(!args.fix_hooks);
}

#[test]
fn test_fix_hooks_dry_run_with_backend_selection() {
    let args = parse(&["clud", "--fix-hooks", "--dry-run", "--codex"]);
    assert!(args.fix_hooks);
    assert!(args.dry_run);
    assert!(args.codex);
    assert!(!args.claude);
}

#[test]
fn test_yes_short_flag() {
    let args = parse(&["clud", "--clean-worktrees", "-y"]);
    assert!(args.yes);
}

// ---------- issue #110: `clud gc` subcommand group ----------

/// `clud gc` with no subcommand must parse successfully and yield
/// `Some(Command::Gc { subcommand: None })` so the runtime can print
/// help and exit 0.
#[test]
fn test_gc_bare_subcommand_yields_none() {
    let args = parse(&["clud", "gc"]);
    match args.command {
        Some(Command::Gc { ref subcommand }) => assert!(subcommand.is_none()),
        _ => panic!("expected Gc subcommand"),
    }
}

#[test]
fn test_gc_list_targets_json() {
    let args = parse(&["clud", "gc", "list", "--targets", "--json"]);
    assert!(matches!(
        args.command,
        Some(Command::Gc {
            subcommand: Some(GcSubcommand::List {
                targets: true,
                json: true,
                kind: None,
            }),
        })
    ));
}

#[test]
fn test_gc_list() {
    let args = parse(&["clud", "gc", "list"]);
    match args.command {
        Some(Command::Gc {
            subcommand: Some(GcSubcommand::List { json, kind, .. }),
        }) => {
            assert!(!json);
            assert!(kind.is_none());
        }
        _ => panic!("expected Gc::List"),
    }
}

#[test]
fn test_gc_list_json() {
    // Issue #135: `clud gc list --json` emits JSON for downstream tooling.
    let args = parse(&["clud", "gc", "list", "--json"]);
    match args.command {
        Some(Command::Gc {
            subcommand: Some(GcSubcommand::List { json, kind, .. }),
        }) => {
            assert!(json);
            assert!(kind.is_none());
        }
        _ => panic!("expected Gc::List --json"),
    }
}

#[test]
fn test_gc_list_kind_filter() {
    let args = parse(&["clud", "gc", "list", "--kind", "trash"]);
    match args.command {
        Some(Command::Gc {
            subcommand: Some(GcSubcommand::List { json, kind, .. }),
        }) => {
            assert!(!json);
            assert_eq!(kind.as_deref(), Some("trash"));
        }
        _ => panic!("expected Gc::List --kind trash"),
    }
}

#[test]
fn test_gc_prune_kind_filter() {
    let args = parse(&["clud", "gc", "prune", "--kind", "worktree"]);
    match args.command {
        Some(Command::Gc {
            subcommand:
                Some(GcSubcommand::Prune {
                    dry_run,
                    ref kind_pos,
                    ref kind,
                    ..
                }),
        }) => {
            assert!(!dry_run);
            assert!(kind_pos.is_none());
            assert_eq!(kind.as_deref(), Some("worktree"));
        }
        _ => panic!("expected Gc::Prune"),
    }
}

#[test]
fn test_gc_purge_without_kind_parses_for_runtime_error() {
    let args = parse(&["clud", "gc", "purge"]);
    match args.command {
        Some(Command::Gc {
            subcommand: Some(GcSubcommand::Purge { ref kind, .. }),
        }) => {
            assert!(
                kind.is_none(),
                "runtime prints the custom missing-kind error"
            );
        }
        _ => panic!("expected bare Gc::Purge"),
    }
}

#[test]
fn test_gc_purge_dry_run_yes_kind() {
    let args = parse(&[
        "clud",
        "gc",
        "purge",
        "--dry-run",
        "--yes",
        "--kind",
        "worktree",
    ]);
    match args.command {
        Some(Command::Gc {
            subcommand:
                Some(GcSubcommand::Purge {
                    dry_run,
                    yes,
                    ref kind_pos,
                    ref kind,
                    ..
                }),
        }) => {
            assert!(dry_run);
            assert!(yes);
            assert!(kind_pos.is_none());
            assert_eq!(kind.as_deref(), Some("worktree"));
        }
        _ => panic!("expected Gc::Purge with flags"),
    }
}

/// Issue #506 changed `gc purge <ARG>` from a clap parse error into a
/// positional KIND, so the pre-#110 duration positional (`gc purge 7d`)
/// now parses here and is rejected at runtime by `validate_pre_daemon`
/// as an unknown kind (see `gc::cli` tests).
#[test]
fn test_gc_purge_legacy_duration_positional_parses_as_kind() {
    let args = parse(&["clud", "gc", "purge", "7d"]);
    match args.command {
        Some(Command::Gc {
            subcommand: Some(GcSubcommand::Purge { ref kind_pos, .. }),
        }) => assert_eq!(kind_pos.as_deref(), Some("7d")),
        _ => panic!("expected Gc::Purge with positional kind"),
    }
}

// ---------- issue #506: positional KIND + `all` pseudo-kind ----------

#[test]
fn test_gc_purge_positional_all_parses() {
    let args = parse(&["clud", "gc", "purge", "all", "--yes"]);
    match args.command {
        Some(Command::Gc {
            subcommand:
                Some(GcSubcommand::Purge {
                    yes, ref kind_pos, ..
                }),
        }) => {
            assert!(yes);
            assert_eq!(kind_pos.as_deref(), Some("all"));
        }
        _ => panic!("expected Gc::Purge with positional `all`"),
    }
}

#[test]
fn test_gc_prune_positional_kind_parses() {
    let args = parse(&["clud", "gc", "prune", "worktree"]);
    match args.command {
        Some(Command::Gc {
            subcommand: Some(GcSubcommand::Prune { ref kind_pos, .. }),
        }) => assert_eq!(kind_pos.as_deref(), Some("worktree")),
        _ => panic!("expected Gc::Prune with positional kind"),
    }
}

#[test]
fn test_gc_purge_positional_conflicts_with_kind_flag() {
    let argv: Vec<String> = [
        "clud", "gc", "purge", "trash", "--kind", "worktree", "--yes",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    assert!(
        Args::try_parse_from(argv).is_err(),
        "positional KIND and --kind together must be a parse error"
    );
}

#[test]
fn test_gc_all_defaults_to_prune() {
    let args = parse(&["clud", "gc", "all"]);
    match args.command {
        Some(Command::Gc {
            subcommand:
                Some(GcSubcommand::All {
                    purge,
                    dry_run,
                    yes,
                    ..
                }),
        }) => {
            assert!(!purge);
            assert!(!dry_run);
            assert!(!yes);
        }
        _ => panic!("expected Gc::All"),
    }
}

#[test]
fn test_gc_all_purge_yes() {
    let args = parse(&["clud", "gc", "all", "--purge", "--yes"]);
    match args.command {
        Some(Command::Gc {
            subcommand:
                Some(GcSubcommand::All {
                    purge,
                    dry_run,
                    yes,
                    ..
                }),
        }) => {
            assert!(purge);
            assert!(!dry_run);
            assert!(yes);
        }
        _ => panic!("expected Gc::All --purge --yes"),
    }
}

#[test]
fn test_no_daemon_flag() {
    // Issue #135: `--no-daemon` disables auto-spawn.
    let args = parse(&["clud", "--no-daemon", "-p", "hi"]);
    assert!(args.no_daemon);
}

#[test]
fn test_gc_reconcile() {
    let args = parse(&["clud", "gc", "reconcile"]);
    match args.command {
        Some(Command::Gc {
            subcommand: Some(GcSubcommand::Reconcile),
        }) => {}
        _ => panic!("expected Gc::Reconcile"),
    }
}

#[test]
fn test_config_bare_subcommand_parses() {
    let args = parse(&["clud", "config"]);
    match args.command {
        Some(Command::Config { subcommand: None }) => {}
        other => panic!("expected bare Config command, got {other:?}"),
    }
    assert!(args.passthrough.is_empty());
}

#[test]
fn test_config_show_subcommand_parses() {
    let args = parse(&["clud", "config", "show"]);
    match args.command {
        Some(Command::Config {
            subcommand: Some(ConfigSubcommand::Show { json }),
        }) => assert!(!json),
        other => panic!("expected Config::Show, got {other:?}"),
    }
    assert!(args.passthrough.is_empty());
}

#[test]
fn test_config_show_json_subcommand_parses() {
    let args = parse(&["clud", "config", "show", "--json"]);
    match args.command {
        Some(Command::Config {
            subcommand: Some(ConfigSubcommand::Show { json }),
        }) => assert!(json),
        other => panic!("expected Config::Show --json, got {other:?}"),
    }
    assert!(args.passthrough.is_empty());
}

#[test]
fn test_config_edit_local_editor_subcommand_parses() {
    let args = parse(&[
        "clud",
        "config",
        "edit",
        "--local",
        "--editor",
        "code --wait",
    ]);
    match args.command {
        Some(Command::Config {
            subcommand: Some(ConfigSubcommand::Edit { local, ref editor }),
        }) => {
            assert!(local);
            assert_eq!(editor.as_deref(), Some("code --wait"));
        }
        other => panic!("expected Config::Edit --local --editor, got {other:?}"),
    }
    assert!(args.passthrough.is_empty());
}

#[test]
fn test_config_flags_remain_backend_passthrough_outside_config_subcommand() {
    let args = parse(&["clud", "--local", "--editor", "vim"]);

    assert!(args.command.is_none());
    assert_eq!(args.passthrough, vec!["--local", "--editor", "vim"]);
}

#[test]
fn test_trash_command_parses_paths_and_cross_volume() {
    let args = parse(&[
        "clud",
        "trash",
        "--cross-volume",
        "target/foo.dll",
        "bar.exe",
    ]);
    match args.command {
        Some(Command::Trash {
            cross_volume,
            paths,
        }) => {
            assert!(cross_volume);
            assert_eq!(
                paths,
                vec![PathBuf::from("target/foo.dll"), PathBuf::from("bar.exe")]
            );
        }
        _ => panic!("expected Trash command"),
    }
}

#[test]
fn test_daemon_restart_subcommand_parses() {
    let args = parse(&["clud", "daemon", "restart"]);
    match args.command {
        Some(Command::Daemon {
            subcommand: DaemonSubcommand::Restart,
        }) => {}
        other => panic!("expected Daemon::Restart, got {other:?}"),
    }
}

#[test]
fn test_daemon_stop_subcommand_parses() {
    let args = parse(&["clud", "daemon", "stop"]);
    match args.command {
        Some(Command::Daemon {
            subcommand: DaemonSubcommand::Stop,
        }) => {}
        other => panic!("expected Daemon::Stop, got {other:?}"),
    }
}

#[test]
fn test_daemon_running_process_json_subcommand_parses() {
    let args = parse(&["clud", "daemon", "running-process", "--json"]);
    match args.command {
        Some(Command::Daemon {
            subcommand: DaemonSubcommand::RunningProcess { json },
        }) => assert!(json),
        other => panic!("expected Daemon::RunningProcess --json, got {other:?}"),
    }
}

#[test]
fn test_daemon_servicedef_alias_subcommand_parses() {
    let args = parse(&["clud", "daemon", "servicedef"]);
    match args.command {
        Some(Command::Daemon {
            subcommand: DaemonSubcommand::RunningProcess { json },
        }) => assert!(!json),
        other => panic!("expected Daemon::RunningProcess alias, got {other:?}"),
    }
}

#[test]
fn test_top_subcommand_parses() {
    let args = parse(&["clud", "top"]);
    match args.command {
        Some(Command::Top(options)) => {
            let crate::args::TopArgs {
                json,
                once,
                watch,
                tree,
                flat,
                sort,
                limit,
                since,
                originator,
            } = *options;
            assert!(!json);
            assert!(!once);
            assert!(!watch);
            assert!(!tree);
            assert!(!flat);
            assert_eq!(sort, TopSort::Cpu);
            assert_eq!(limit, 20);
            assert!(since.is_none());
            assert!(originator.is_none());
        }
        other => panic!("expected Top, got {other:?}"),
    }
    assert!(args.passthrough.is_empty());
}

#[test]
fn test_top_json_subcommand_parses() {
    let args = parse(&["clud", "top", "--json"]);
    match args.command {
        Some(Command::Top(options)) => assert!(options.json),
        other => panic!("expected Top --json, got {other:?}"),
    }
    assert!(args.passthrough.is_empty());
}

#[test]
fn test_top_once_flat_sort_limit_since_originator_parses() {
    let args = parse(&[
        "clud",
        "top",
        "--once",
        "--flat",
        "--sort",
        "rss",
        "--limit",
        "7",
        "--since",
        "5m",
        "--originator",
        "CLUD:123",
    ]);
    match args.command {
        Some(Command::Top(options)) => {
            let crate::args::TopArgs {
                once,
                flat,
                sort,
                limit,
                since,
                originator,
                ..
            } = *options;
            assert!(once);
            assert!(flat);
            assert_eq!(sort, TopSort::Rss);
            assert_eq!(limit, 7);
            assert_eq!(since.as_deref(), Some("5m"));
            assert_eq!(originator.as_deref(), Some("CLUD:123"));
        }
        other => panic!("expected Top with options, got {other:?}"),
    }
}

#[test]
fn test_symbols_bare_subcommand_yields_none() {
    let args = parse(&["clud", "symbols"]);
    match args.command {
        Some(Command::Symbols { ref subcommand }) => assert!(subcommand.is_none()),
        other => panic!("expected Symbols, got {other:?}"),
    }
    assert!(args.passthrough.is_empty());
}

#[test]
fn test_symbols_install_subcommand_parses() {
    let args = parse(&["clud", "symbols", "install"]);
    match args.command {
        Some(Command::Symbols {
            subcommand: Some(SymbolsSubcommand::Install),
        }) => {}
        other => panic!("expected Symbols::Install, got {other:?}"),
    }
    assert!(args.passthrough.is_empty());
}

#[test]
fn test_symbols_verify_all_subcommand_parses() {
    let args = parse(&["clud", "symbols", "verify", "--all"]);
    match args.command {
        Some(Command::Symbols {
            subcommand: Some(SymbolsSubcommand::Verify { all }),
        }) => assert!(all),
        other => panic!("expected Symbols::Verify --all, got {other:?}"),
    }
    assert!(args.passthrough.is_empty());
}

#[test]
fn test_symbols_verify_default_all_false() {
    let args = parse(&["clud", "symbols", "verify"]);
    match args.command {
        Some(Command::Symbols {
            subcommand: Some(SymbolsSubcommand::Verify { all }),
        }) => assert!(!all),
        other => panic!("expected Symbols::Verify (no --all), got {other:?}"),
    }
}

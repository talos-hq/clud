use super::*;
use tempfile::tempdir;

pub(crate) fn write(path: &Path, body: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
}

#[test]
fn format_launch_warnings_empty_is_none() {
    assert_eq!(format_launch_warnings(&[]), None);
}

#[test]
fn format_launch_warnings_composes_every_warning_into_one_block() {
    let warnings = vec!["a".to_string(), "b".to_string()];
    assert_eq!(
        format_launch_warnings(&warnings).as_deref(),
        Some("[clud] warning: a\n[clud] warning: b\n")
    );
}

#[test]
fn launch_hook_gate_follows_effective_harness_not_provider_flag() {
    let cross_route_args = Args::parse_from(["clud", "--codex", "--harness", "claude"]);
    let claude_harness = crate::backend::resolve_launch_target(
        false,
        true,
        false,
        Some(crate::backend::HarnessSelection::Claude),
        None,
        None,
    )
    .unwrap();
    assert!(!should_check_launch(&cross_route_args, claude_harness));
    assert!(should_warn_uv_launch(&cross_route_args));

    let bare_args = Args::parse_from(["clud"]);
    let saved_codex = crate::backend::resolve_launch_target(
        false,
        false,
        false,
        None,
        Some(crate::backend::ModelProvider::Codex),
        Some(crate::backend::HarnessSelection::Default),
    )
    .unwrap();
    assert!(should_check_launch(&bare_args, saved_codex));
}

#[test]
fn risky_uv_hook_invocations_are_counted_once_per_settings_file() {
    let temp = tempdir().unwrap();
    let repo = temp.path().join("repo");
    let home = temp.path().join("home");
    write(
        &repo.join(".claude/settings.json"),
        r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[
            {"type":"command","command":"cd \"$d\" && uv run python ci/hooks/a.py"},
            {"type":"command","command":"uv run python ci/hooks/b.py"},
            {"type":"command","command":"uv run --frozen --no-sync python ci/hooks/c.py"},
            {"type":"command","command":"uv run --no-project python ci/hooks/d.py"},
            {"type":"command","command":"uv run --frozen python ci/hooks/e.py"}
        ]}]}}"#,
    );

    let report = inspect_paths(&repo, Some(&home));
    let uv_warnings = uv_run_hook_warnings(&report.claude);
    assert_eq!(uv_warnings.len(), 1);
    assert!(uv_warnings[0].contains("3 Claude PreToolUse hook(s)"));
    assert!(uv_warnings[0].contains("--frozen` alone still syncs"));
    assert!(report.warnings.contains(&uv_warnings[0]));
}

#[test]
fn uv_hook_flags_belong_to_its_options_not_a_script_or_later_command() {
    assert!(warnings::uv_run_without_safe_flag(
        "uv run python a.py --no-sync"
    ));
    assert!(warnings::uv_run_without_safe_flag(
        "uv run python a.py; uv run --no-sync python b.py"
    ));
    assert!(!warnings::uv_run_without_safe_flag(
        "uv run --with foo --no-sync python b.py"
    ));
    assert!(warnings::uv_run_without_safe_flag(
        "uv run --script a.py --no-sync"
    ));
}

pub(crate) fn trusted_state_for(path: &Path, group: usize, handler: usize) -> String {
    format!(
        "[hooks.state.'{}:{}:{group}:{handler}']\ntrusted_hash = \"sha256:test\"\n",
        path.to_string_lossy(),
        CODEX_PRE_TOOL_USE_STATE
    )
}

#[test]
fn claude_settings_parser_normalizes_pre_tool_matchers() {
    let temp = tempdir().unwrap();
    let repo = temp.path().join("repo");
    let home = temp.path().join("home");
    write(
        &repo.join(".claude").join("settings.json"),
        r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"python check.py"}]}]}}"#,
    );

    let report = inspect_paths(&repo, Some(&home));

    assert_eq!(
        report.claude.all_matchers(),
        BTreeSet::from(["Bash".to_string()])
    );
    assert!(report.claude.hooks[0].active);
}

#[test]
fn codex_current_shape_parses_and_legacy_shape_warns() {
    let temp = tempdir().unwrap();
    let repo = temp.path().join("repo");
    let home = temp.path().join("home");
    let current = repo.join(".codex").join("hooks.json");
    write(
        &current,
        r#"{"hooks":{"PreToolUse":[{"matcher":"*","hooks":[{"type":"command","command":"python hook.py"}]}]}}"#,
    );
    write(
        &home.join(".codex").join("config.toml"),
        &format!(
            "[projects.'{}']\ntrust_level = \"trusted\"\n{}",
            codex_project_key(&repo),
            trusted_state_for(&current, 0, 0)
        ),
    );

    let report = inspect_paths(&repo, Some(&home));
    assert_eq!(
        report.codex.all_matchers(),
        BTreeSet::from(["*".to_string()])
    );
    assert!(report.codex.hooks[0].active);

    write(
        &current,
        r#"{"PreToolUse":[{"matcher":"Bash","hooks":[]}]}"#,
    );
    let report = inspect_paths(&repo, Some(&home));
    assert!(report
        .warnings
        .iter()
        .any(|warning| warning.contains("legacy root-level")));
}

#[test]
fn one_sided_hook_warnings_are_bidirectional() {
    let temp = tempdir().unwrap();
    let repo = temp.path().join("repo");
    let home = temp.path().join("home");
    write(
        &repo.join(".claude").join("settings.json"),
        r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{}]}]}}"#,
    );
    let report = inspect_paths(&repo, Some(&home));
    assert!(report
        .warnings
        .iter()
        .any(|warning| warning.contains("Claude PreToolUse hooks exist")
            && warning.contains("clud --fix-hooks")));

    fs::remove_file(repo.join(".claude").join("settings.json")).unwrap();
    let codex = repo.join(".codex").join("hooks.json");
    write(
        &codex,
        r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{}]}]}}"#,
    );
    write(
        &home.join(".codex").join("config.toml"),
        &format!(
            "[projects.'{}']\ntrust_level = \"trusted\"\n{}",
            codex_project_key(&repo),
            trusted_state_for(&codex, 0, 0)
        ),
    );
    let report = inspect_paths(&repo, Some(&home));
    assert!(report
        .warnings
        .iter()
        .any(|warning| warning.contains("Codex PreToolUse hooks exist")
            && warning.contains("clud --fix-hooks")));
}

#[test]
fn matcher_mismatch_warns() {
    let temp = tempdir().unwrap();
    let repo = temp.path().join("repo");
    let home = temp.path().join("home");
    write(
        &repo.join(".claude").join("settings.json"),
        r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{}]}]}}"#,
    );
    let codex = repo.join(".codex").join("hooks.json");
    write(
        &codex,
        r#"{"hooks":{"PreToolUse":[{"matcher":"Read","hooks":[{}]}]}}"#,
    );
    write(
        &home.join(".codex").join("config.toml"),
        &format!(
            "[projects.'{}']\ntrust_level = \"trusted\"\n{}",
            codex_project_key(&repo),
            trusted_state_for(&codex, 0, 0)
        ),
    );

    let report = inspect_paths(&repo, Some(&home));

    assert!(report
        .warnings
        .iter()
        .any(|warning| warning.contains("matchers differ")));
}

#[test]
fn windows_codex_project_key_normalizes_extended_path() {
    let path = PathBuf::from(r"\\?\C:\Users\Niteris\Dev\Repo");
    assert_eq!(codex_project_key(&path), r"c:\users\niteris\dev\repo");
    assert!(is_extended_key_for(
        r"\\?\C:\Users\Niteris\Dev\Repo",
        r"c:\users\niteris\dev\repo"
    ));
}

#[test]
fn extended_path_only_trust_config_warns() {
    let temp = tempdir().unwrap();
    // A Windows-shaped relative path is writable on the Windows checkout but
    // resolves under the read-only Linux source mount. Keep this fixture in temp.
    let repo = temp.path().join("repo");
    let home = temp.path().join("home");
    let codex = repo.join(".codex").join("hooks.json");
    write(
        &codex,
        r#"{"hooks":{"PreToolUse":[{"matcher":"*","hooks":[{}]}]}}"#,
    );
    let extended_repo_key = format!(r"\\?\{}", repo.display());
    write(
        &home.join(".codex").join("config.toml"),
        &format!("[projects.'{extended_repo_key}']\ntrust_level = \"trusted\"\n"),
    );

    let report = inspect_paths(&repo, Some(&home));

    assert!(report
        .warnings
        .iter()
        .any(|warning| warning.contains("extended `\\\\?\\...` path key")));
}

#[test]
fn deterministic_trust_repair_adds_canonical_key() {
    let temp = tempdir().unwrap();
    let config = temp.path().join("home").join(".codex").join("config.toml");
    add_codex_project_trust(&config, r"c:\users\niteris\dev\repo").unwrap();

    let text = fs::read_to_string(config).unwrap();
    assert!(text.contains(r#"trust_level = "trusted""#));
    assert!(text.contains(r#"c:\users\niteris\dev\repo"#));
    assert!(!text.contains("trusted_hash"));
}

#[test]
fn legacy_codex_hooks_feature_warns_and_plans_deterministic_repair() {
    let temp = tempdir().unwrap();
    let repo = temp.path().join("repo");
    let config = repo.join(".codex").join("config.toml");
    write(&config, "[features]\ncodex_hooks = true\n");

    let report = inspect_paths(&repo, None);

    assert_eq!(
        report.codex_legacy_hook_feature_configs,
        vec![config.clone()]
    );
    assert!(report
        .warnings
        .iter()
        .any(|warning| warning.contains("deprecated `[features].codex_hooks`")));
    assert!(plan_repairs(&report).into_iter().any(|action| matches!(
        action,
        RepairAction::MigrateCodexHooksFeatureFlag { config_path } if config_path == config
    )));
}

#[test]
fn legacy_codex_hooks_feature_migration_moves_value_and_preserves_other_config() {
    let temp = tempdir().unwrap();
    let config = temp.path().join("home").join(".codex").join("config.toml");
    write(
        &config,
        "[features]\ncodex_hooks = true\nmodel = \"gpt-5\"\n\n[projects.repo]\ntrust_level = \"trusted\"\n",
    );

    migrate_codex_hooks_feature_flag(&config).unwrap();

    let text = fs::read_to_string(config).unwrap();
    assert!(text.contains("hooks = true"), "{text}");
    assert!(!text.contains("codex_hooks"), "{text}");
    assert!(text.contains("model = \"gpt-5\""), "{text}");
    assert!(text.contains("[projects.repo]"), "{text}");
}

#[test]
fn legacy_codex_hooks_feature_migration_preserves_existing_hooks_value() {
    let temp = tempdir().unwrap();
    let config = temp.path().join("home").join(".codex").join("config.toml");
    write(&config, "[features]\ncodex_hooks = false\nhooks = true\n");

    migrate_codex_hooks_feature_flag(&config).unwrap();

    let text = fs::read_to_string(config).unwrap();
    assert!(text.contains("hooks = true"), "{text}");
    assert!(!text.contains("codex_hooks"), "{text}");
}

#[test]
fn catch_all_codex_hook_satisfies_specific_claude_matchers() {
    let temp = tempdir().unwrap();
    let repo = temp.path().join("repo");
    let home = temp.path().join("home");
    write(
        &repo.join(".claude").join("settings.json"),
        r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{}]},{"matcher":"Read","hooks":[{}]}]}}"#,
    );
    let codex = repo.join(".codex").join("hooks.json");
    write(
        &codex,
        r#"{"hooks":{"PreToolUse":[{"matcher":"*","hooks":[{}]}]}}"#,
    );
    write(
        &home.join(".codex").join("config.toml"),
        &format!(
            "[projects.'{}']\ntrust_level = \"trusted\"\n{}",
            codex_project_key(&repo),
            trusted_state_for(&codex, 0, 0)
        ),
    );

    let report = inspect_paths(&repo, Some(&home));

    assert!(
        !report
            .warnings
            .iter()
            .any(|warning| warning.contains("matchers differ")),
        "catch-all Codex matcher should satisfy specific Claude matchers: {:?}",
        report.warnings
    );
    assert!(
        plan_repairs(&report)
            .into_iter()
            .filter(|action| matches!(action, RepairAction::BackendPrompt { .. }))
            .collect::<Vec<_>>()
            .is_empty(),
        "catch-all target coverage should not request per-tool migrations"
    );
}

#[test]
fn same_command_claude_hooks_plan_one_codex_catch_all_prompt() {
    let temp = tempdir().unwrap();
    let repo = temp.path().join("repo");
    let home = temp.path().join("home");
    write(
        &repo.join(".claude").join("settings.json"),
        r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"python check.py"}]},{"matcher":"Read","hooks":[{"type":"command","command":"python check.py"}]}]}}"#,
    );

    let report = inspect_paths(&repo, Some(&home));
    let prompts = plan_repairs(&report)
        .into_iter()
        .filter(|action| matches!(action, RepairAction::BackendPrompt { .. }))
        .collect::<Vec<_>>();

    assert_eq!(prompts.len(), 1);
    assert!(matches!(
        &prompts[0],
        RepairAction::BackendPrompt {
            target: HookFrontend::Codex,
            matcher,
            prompt,
            ..
        } if matcher == "*" && prompt.contains("catch-all")
    ));
}

#[test]
fn different_command_claude_hooks_stay_per_matcher_prompts() {
    let temp = tempdir().unwrap();
    let repo = temp.path().join("repo");
    let home = temp.path().join("home");
    write(
        &repo.join(".claude").join("settings.json"),
        r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"python bash.py"}]},{"matcher":"Read","hooks":[{"type":"command","command":"python read.py"}]}]}}"#,
    );

    let report = inspect_paths(&repo, Some(&home));
    let prompts = plan_repairs(&report)
        .into_iter()
        .filter(|action| matches!(action, RepairAction::BackendPrompt { .. }))
        .collect::<Vec<_>>();

    assert_eq!(prompts.len(), 2);
    assert!(prompts.iter().all(|action| match action {
        RepairAction::BackendPrompt {
            matcher, prompt, ..
        } => matcher != "*" && prompt.contains("exactly one hook"),
        _ => false,
    }));
}

#[test]
fn the_broken_git_rev_parse_prefix_is_warned_about_with_the_fix() {
    // zackees/clud#967 Phase 1. Deliberately a Stop hook: the parser behind
    // `report.claude` reads only PreToolUse, so a warning built from that
    // summary alone would miss the exact shape that wedged a real session.
    let temp = tempdir().unwrap();
    let repo = temp.path().join("repo");
    let home = temp.path().join("home");
    write(
        &repo.join(".claude").join("settings.json"),
        r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"cd \"$(git rev-parse --show-superproject-working-tree 2>/dev/null || git rev-parse --show-toplevel 2>/dev/null || echo .)\" && uv run python ci/hooks/check-on-stop.py"}]}]}}"#,
    );

    let report = inspect_paths(&repo, Some(&home));

    let warning = report
        .warnings
        .iter()
        .find(|warning| warning.contains("--show-superproject-working-tree"))
        .expect("broken git rev-parse prefix warned");
    assert!(warning.contains("exits 0 with empty output"), "{warning}");
    assert!(warning.contains("CLAUDE_PROJECT_DIR"), "{warning}");
}

#[test]
fn a_self_rooted_hook_earns_no_git_rev_parse_warning() {
    let temp = tempdir().unwrap();
    let repo = temp.path().join("repo");
    let home = temp.path().join("home");
    write(
        &repo.join(".claude").join("settings.json"),
        r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"cd \"$CLAUDE_PROJECT_DIR\" && uv run python ci/hooks/check-on-stop.py"}]}]}}"#,
    );

    let report = inspect_paths(&repo, Some(&home));

    assert!(!report
        .warnings
        .iter()
        .any(|warning| warning.contains("--show-superproject-working-tree")));
}

#[test]
fn a_hook_declared_in_both_places_is_warned_about_as_a_double_fire() {
    // Migrating a hook means moving it. A copy left in the frontend's own
    // settings still fires there, unrooted — which is the failure the
    // declaration was supposed to fix.
    let temp = tempdir().unwrap();
    let repo = temp.path().join("repo");
    let home = temp.path().join("home");
    let command = "uv run python ci/hooks/check.py";
    write(
        &repo.join(".claude").join("settings.json"),
        &format!(
            r#"{{"hooks":{{"PreToolUse":[{{"matcher":"Bash","hooks":[{{"type":"command","command":"{command}"}}]}}]}}}}"#
        ),
    );
    write(
        &repo.join(".clud").join("hooks.json"),
        &format!(r#"{{"hooks":{{"PreToolUse":[{{"command":"{command}"}}]}}}}"#),
    );

    let report = inspect_paths(&repo, Some(&home));

    let warning = report
        .warnings
        .iter()
        .find(|warning| warning.contains("runs twice"))
        .expect("double declaration warned");
    assert!(warning.contains(command), "{warning}");
    assert!(warning.contains("settings.json"), "{warning}");
}

#[test]
fn a_hook_that_only_moved_earns_no_double_fire_warning() {
    let temp = tempdir().unwrap();
    let repo = temp.path().join("repo");
    let home = temp.path().join("home");
    write(
        &repo.join(".claude").join("settings.json"),
        r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"clud-cmd-scan"}]}]}}"#,
    );
    write(
        &repo.join(".clud").join("hooks.json"),
        r#"{"hooks":{"PreToolUse":[{"command":"uv run python ci/hooks/check.py"}]}}"#,
    );

    let report = inspect_paths(&repo, Some(&home));

    assert!(!report.warnings.iter().any(|w| w.contains("runs twice")));
}

#[test]
fn codex_project_trusted_reads_the_projects_table() {
    let temp = tempdir().unwrap();
    let repo = temp.path().join("repo");
    let home = temp.path().join("home");
    let config = home.join(".codex").join("config.toml");
    let key = codex_project_key(&repo);

    assert!(
        !codex_project_trusted(&repo, &home),
        "no config at all is not trusted"
    );

    write(
        &config,
        &format!("[projects.'{key}']\ntrust_level = \"trusted\"\n"),
    );
    assert!(codex_project_trusted(&repo, &home));

    // Any other value — or no value — is not trusted.
    write(
        &config,
        &format!("[projects.'{key}']\ntrust_level = \"ask\"\n"),
    );
    assert!(!codex_project_trusted(&repo, &home));
    write(&config, &format!("[projects.'{key}']\n"));
    assert!(!codex_project_trusted(&repo, &home));

    // A different project's trust does not leak in.
    write(
        &config,
        "[projects.\"/elsewhere\"]\ntrust_level = \"trusted\"\n",
    );
    assert!(!codex_project_trusted(&repo, &home));

    // An unparsable config is not trusted.
    write(&config, "{not toml");
    assert!(!codex_project_trusted(&repo, &home));
}

#[test]
fn codex_trust_from_custom_home_accepts_extended_windows_key() {
    let temp = tempdir().unwrap();
    let config = temp.path().join("config.toml");
    write(
        &config,
        r#"[projects.'\\?\C:\work\repo']
trust_level = "trusted"
"#,
    );
    assert!(codex_project_trusted_in_config(
        Path::new(r"c:\work\repo"),
        &config
    ));
    assert!(!codex_project_trusted_in_config(
        Path::new(r"c:\work\other"),
        &config
    ));
    write(
        &config,
        "[projects.'C:/Work/Repo']\ntrust_level = 'trusted'\n",
    );
    assert!(codex_project_trusted_in_config(
        Path::new(r"c:\work\repo"),
        &config
    ));
}

use super::*;

#[test]
fn latest_orphan_sweep_picks_the_newest_finished_event() {
    let lines = [
        r#"{"op":"orphan_sweep_started","ts_ms":100}"#,
        r#"{"op":"orphan_sweep_finished","ts_ms":200,"found":5,"reaped":3}"#,
        r#"{"op":"some_other_event","ts_ms":300}"#,
        r#"{"op":"orphan_sweep_finished","ts_ms":250,"found":1,"reaped":0}"#,
        r#"not json"#,
    ];
    let latest = latest_orphan_sweep(lines.iter().map(|s| s.to_string())).unwrap();
    assert_eq!(latest.ts_ms, 250);
    assert_eq!((latest.found, latest.reaped), (1, 0));
}

#[test]
fn latest_orphan_sweep_is_none_when_never_swept() {
    let lines = [r#"{"op":"orphan_sweep_started","ts_ms":100}"#];
    assert!(latest_orphan_sweep(lines.iter().map(|s| s.to_string())).is_none());
}

#[test]
fn sentinel_is_preferred_over_the_event_log() {
    let tmp = tempfile::TempDir::new().unwrap();
    // No sentinel yet → None, so the caller falls back to the log scan.
    assert!(sentinel_orphan_sweep(tmp.path()).is_none());

    std::fs::write(
        super::super::server::orphan_sweep_sentinel_path(tmp.path()),
        r#"{"ts_ms":1700000000000,"found":7,"reaped":3}"#,
    )
    .unwrap();
    let status = sentinel_orphan_sweep(tmp.path()).expect("sentinel parses");
    assert_eq!(status.ts_ms, 1_700_000_000_000);
    assert_eq!((status.found, status.reaped), (7, 3));

    // A corrupt sentinel falls back rather than panicking.
    std::fs::write(
        super::super::server::orphan_sweep_sentinel_path(tmp.path()),
        "{ not json",
    )
    .unwrap();
    assert!(sentinel_orphan_sweep(tmp.path()).is_none());
}

#[test]
fn orphan_status_also_searches_the_rotated_log() {
    // Regression: the shared event log rotates at 1 MB, so a burst of
    // unrelated events can push every sweep record into `<log>.1`. Reading
    // only the active file reported a false "no sweep recorded — STALE".
    let paths = orphan_status_log_paths(Path::new("state"));
    assert_eq!(paths.len(), 2, "active + one rotated backup");
    assert!(paths[0].ends_with("daemon-events.jsonl"));
    assert!(
        paths[1].ends_with("daemon-events.jsonl.1"),
        "second path must be the rotated backup, got {:?}",
        paths[1]
    );
}

#[test]
fn sweep_is_stale_past_two_intervals_or_never() {
    let interval = 60_000;
    assert!(
        sweep_is_stale(None, 1_000_000, interval),
        "never swept → stale"
    );
    let fresh = OrphanSweepStatus {
        ts_ms: 1_000_000,
        found: 0,
        reaped: 0,
    };
    // 1× interval later: fresh.
    assert!(!sweep_is_stale(Some(&fresh), 1_060_000, interval));
    // Exactly 2× is the boundary (not yet stale); just past it is stale.
    assert!(!sweep_is_stale(Some(&fresh), 1_120_000, interval));
    assert!(sweep_is_stale(Some(&fresh), 1_120_001, interval));
}

fn parse_args(argv: &[&str]) -> Args {
    Args::parse_from_raw(argv.iter().map(|value| (*value).to_string()).collect())
}

// Regression for the symptom reported after PR #151:
//   clud  (no args, interactive terminal)
//   → [clud] daemon session sess-...
//   → Error: Input must be provided either through stdin or as a
//     prompt argument when using --print
//
// Cause: the centralized daemon mapped `LaunchMode::Subprocess`
// straight through to `SessionKind::Subprocess`, and the worker's
// subprocess path uses `StdinMode::Null`. Claude saw no TTY,
// dropped into `--print` mode, and bailed for lack of a prompt.
// Interactive launches must force PTY so the worker hands the
// backend a pseudo-terminal it can drive.
#[test]
fn interactive_launch_forces_pty_even_when_plan_says_subprocess() {
    assert!(matches!(
        select_session_kind(LaunchMode::Subprocess, false, false),
        SessionKind::Pty
    ));
}

#[test]
fn interactive_pty_plan_stays_pty() {
    assert!(matches!(
        select_session_kind(LaunchMode::Pty, false, false),
        SessionKind::Pty
    ));
}

#[test]
fn prompted_subprocess_plan_stays_subprocess() {
    // `clud -p "hi"` — claude consumes the prompt arg, no TTY needed.
    assert!(matches!(
        select_session_kind(LaunchMode::Subprocess, false, true),
        SessionKind::Subprocess
    ));
}

#[test]
fn prompted_pty_plan_stays_pty() {
    assert!(matches!(
        select_session_kind(LaunchMode::Pty, false, true),
        SessionKind::Pty
    ));
}

#[test]
fn repeat_jobs_always_subprocess() {
    // Repeat jobs are background, have their own embedded prompt,
    // and never need an attached TTY — even for the interactive
    // case the override must win.
    assert!(matches!(
        select_session_kind(LaunchMode::Subprocess, true, false),
        SessionKind::Subprocess
    ));
    assert!(matches!(
        select_session_kind(LaunchMode::Pty, true, false),
        SessionKind::Subprocess
    ));
    assert!(matches!(
        select_session_kind(LaunchMode::Subprocess, true, true),
        SessionKind::Subprocess
    ));
}

#[test]
fn backend_prompt_classification_drives_centralized_session_kind() {
    for (argv, backend, expected) in [
        (
            vec![
                "clud",
                "--codex",
                "do",
                "https://github.com/zackees/clud/issues/1036",
            ],
            crate::backend::Backend::Codex,
            true,
        ),
        (
            vec!["clud", "--codex", "up"],
            crate::backend::Backend::Codex,
            true,
        ),
        (
            vec![
                "clud",
                "--codex",
                "grind",
                "https://github.com/zackees/clud/issues",
            ],
            crate::backend::Backend::Codex,
            true,
        ),
        // #1173: `grind` is one interactive PTY session on every harness; the
        // harness-support check happens in `main`, not here.
        (
            vec!["clud", "grind", "https://github.com/zackees/clud/issues"],
            crate::backend::Backend::Claude,
            true,
        ),
        (
            vec!["clud", "do", "https://github.com/zackees/clud/issues/1036"],
            crate::backend::Backend::Claude,
            true,
        ),
        (
            vec![
                "clud",
                "--harness",
                "deepseek",
                "do",
                "https://github.com/zackees/clud/issues/1036",
            ],
            crate::backend::Backend::DeepSeek,
            false,
        ),
    ] {
        let args = parse_args(&argv);
        let mut plan = crate::command::build_launch_plan(&args, backend, backend.executable_name());
        // A real terminal normally gives interactive Codex/Claude a subprocess
        // plan that inherits the TTY. Centralization must still upgrade it to a
        // daemon PTY, while DeepSeek's headless prompt remains a subprocess.
        plan.launch_mode = LaunchMode::Subprocess;
        let kind = select_session_kind_for_plan(&args, &plan, false);
        assert_eq!(
            matches!(kind, SessionKind::Pty),
            expected,
            "argv={argv:?}, command={:?}",
            plan.command
        );
    }
}

#[test]
fn transcript_forces_centralized_daemon() {
    let args = Args {
        no_land: false,
        unsafe_mode: false,
        web_term: false,
        kitty_term: false,
        set_web_term: None,
        provider: None,
        unified: false,
        failover: None,
        failover_allow_metered: false,
        allow_model: Vec::new(),
        provider_only: Vec::new(),
        zdr: false,
        set_openrouter_zdr: None,
        saved_openrouter_zdr: None,
        no_data_collection: false,
        redact_secrets: false,
        no_redact_secrets: false,
        mode: None,
        effort: None,
        context_window: None,
        prompt: Some("hi".into()),
        message: None,
        continue_session: false,
        resume: None,
        claude: false,
        codex: false,
        deepseek: false,
        kimi: false,
        openrouter: false,
        harness: None,
        subprocess: false,
        pty: false,
        graphics: crate::graphics::GraphicsMode::Auto,
        graphics_image: None,
        demo_gfx_sixel: false,
        model: None,
        safe: false,
        unattended: false,
        allow_plan_mode: false,
        last: false,
        resume_mode: Default::default(),
        coauthor: None,
        dry_run: false,
        do_meta: false,
        do_rendered_prompt: None,
        do_goal: None,
        detach: false,
        detachable: false,
        session_name: None,
        transcript: Some(PathBuf::from("session.log")),
        backlog_size: None,
        verbose: false,
        no_dnd: false,
        clean_worktrees: false,
        fix_hooks: false,
        no_fix_hooks: false,
        stale_after: "1d".into(),
        yes: false,
        installer: false,
        install_current: false,
        install_version: None,
        no_modify_path: false,
        installer_json: false,
        force: false,
        experimental_daemon_centralized: false,
        daemon_state_dir: None,
        daemon_mode: None,
        no_daemon: false,
        keep_orphans: false,
        quiet_orphans: false,
        explain_orphans: false,
        no_cpu_banner: false,
        command: None,
        passthrough: Vec::new(),
        codex_config_overrides: Vec::new(),
        resolved_model_selection: None,
        raw_argv: Vec::new(),
        continue_index: None,
        inline_api_key: None,
    };
    assert!(experimental_enabled(&args));
}

#[test]
fn repeat_command_pins_resolved_provider_and_effective_harness() {
    let args = Args::parse_from_raw(
        [
            "clud",
            "--codex",
            "--harness",
            "claude",
            "loop",
            "--repeat",
            "1h",
            "task",
        ]
        .into_iter()
        .map(str::to_string)
        .collect(),
    );
    let target = crate::backend::resolve_launch_target(
        args.claude,
        args.codex,
        args.deepseek,
        args.harness,
        None,
        None,
    )
    .unwrap();
    let plan = crate::command::build_launch_plan_for_target(&args, target, "claude");
    let command = build_repeat_once_command(&args, &plan).unwrap();
    assert!(command.windows(1).any(|part| part == ["--codex"]));
    assert!(command
        .windows(2)
        .any(|part| part == ["--harness", "claude"]));
}

#[test]
fn repeat_command_preserves_deepseek_provider() {
    let args = Args::parse_from_raw(
        ["clud", "--deepseek", "loop", "--repeat", "1h", "task"]
            .into_iter()
            .map(str::to_string)
            .collect(),
    );
    let target = crate::backend::resolve_launch_target(
        args.claude,
        args.codex,
        args.deepseek,
        args.harness,
        None,
        None,
    )
    .unwrap();
    let plan = crate::command::build_launch_plan_for_target(&args, target, "claude");
    let command = build_repeat_once_command(&args, &plan).unwrap();
    assert!(command.windows(1).any(|part| part == ["--deepseek"]));
    assert!(command
        .windows(2)
        .any(|part| part == ["--harness", "claude"]));
}

/// Kimi twin of `repeat_command_preserves_deepseek_provider`. Uses the
/// provider-neutral `resolve_launch_target_with_provider` entry point (see
/// the comment on `repeat_command_flag_matches_descriptor_cli_flag_for_every_
/// anthropic_compat_provider` below for why).
#[test]
fn repeat_command_preserves_kimi_provider() {
    let args = Args::parse_from_raw(
        ["clud", "--kimi", "loop", "--repeat", "1h", "task"]
            .into_iter()
            .map(str::to_string)
            .collect(),
    );
    let target = crate::backend::resolve_launch_target_with_provider(
        args.explicit_model_provider(),
        args.harness,
        None,
        None,
    )
    .unwrap();
    let plan = crate::command::build_launch_plan_for_target(&args, target, "claude");
    let command = build_repeat_once_command(&args, &plan).unwrap();
    assert!(command.windows(1).any(|part| part == ["--kimi"]));
    assert!(command
        .windows(2)
        .any(|part| part == ["--harness", "claude"]));
}

/// Guardrail for #937 Phase 2B: `build_repeat_once_command`'s provider match
/// (`daemon/entry.rs`) stays a hand-written, compiler-enforced exhaustive
/// match rather than becoming registry-driven, per the design in #936/#937.
/// That match is therefore exactly the kind of silent-drift risk the design
/// calls out: the compiler forces *some* arm to exist for a new provider,
/// but never checks that the literal it pushes matches the registry's
/// `cli_flag`. This test closes that gap: for every provider that has an
/// Anthropic-compat descriptor, the flag this surface actually emits on the
/// re-exec command line must equal `descriptor.cli_flag` byte-for-byte.
#[test]
fn repeat_command_flag_matches_descriptor_cli_flag_for_every_anthropic_compat_provider() {
    for descriptor in crate::provider_registry::ANTHROPIC_COMPAT_PROVIDERS {
        let args = Args::parse_from_raw(
            [
                "clud",
                descriptor.cli_flag,
                "loop",
                "--repeat",
                "1h",
                "task",
            ]
            .into_iter()
            .map(str::to_string)
            .collect(),
        );
        // Provider-neutral entry point, not the 3-bool `resolve_launch_target`
        // wrapper: that wrapper only knows about `claude`/`codex`/`deepseek`
        // and silently drops any provider (e.g. Kimi's `args.kimi`) it wasn't
        // written to read, which would make this loop's assertion below pass
        // for the wrong reason (or not run at all) for a provider added after
        // the wrapper was last touched.
        let target = crate::backend::resolve_launch_target_with_provider(
            args.explicit_model_provider(),
            args.harness,
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            target.model_provider, descriptor.provider,
            "descriptor.cli_flag must resolve back to its own provider"
        );
        let plan = crate::command::build_launch_plan_for_target(&args, target, "claude");
        let command = build_repeat_once_command(&args, &plan).unwrap();
        assert!(
            command.windows(1).any(|part| part == [descriptor.cli_flag]),
            "expected {:?} to contain the literal flag {:?}",
            command,
            descriptor.cli_flag
        );
    }
}

#[test]
fn repeat_command_preserves_unified_routing_without_a_direct_provider_flag() {
    let args = Args::parse_from_raw(
        ["clud", "--unified", "loop", "--repeat", "1h", "task"]
            .into_iter()
            .map(str::to_string)
            .collect(),
    );
    let target = crate::backend::resolve_routed_launch_target(
        args.routing_mode(),
        None,
        args.harness,
        None,
        None,
    )
    .unwrap();
    let plan = crate::command::build_launch_plan_for_target(&args, target, "claude");
    let command = build_repeat_once_command(&args, &plan).unwrap();
    assert!(command.windows(1).any(|part| part == ["--unified"]));
    assert!(!command
        .iter()
        .any(|part| { matches!(part.as_str(), "--claude" | "--codex" | "--deepseek") }));
}

#[test]
fn repeat_command_pins_the_normalized_model_effort_and_context() {
    let args = Args::parse_from_raw(
        [
            "clud",
            "--deepseek",
            "--model",
            "deepseek-v4-pro[1m]",
            "--effort",
            "max",
            "loop",
            "--repeat",
            "1h",
            "task",
        ]
        .into_iter()
        .map(str::to_string)
        .collect(),
    );
    let target = crate::backend::resolve_launch_target(
        args.claude,
        args.codex,
        args.deepseek,
        args.harness,
        None,
        None,
    )
    .unwrap();
    let plan = crate::command::build_launch_plan_for_target(&args, target, "claude");
    let command = build_repeat_once_command(&args, &plan).unwrap();
    assert!(command
        .windows(2)
        .any(|part| part == ["--model", "deepseek-v4-pro[1m]"]));
    assert!(command.windows(2).any(|part| part == ["--effort", "max"]));
    assert!(command
        .windows(2)
        .any(|part| part == ["--context-window", "1m"]));
}

#[test]
fn repeat_command_pins_the_resolved_codex_catalog_default() {
    let mut args = Args::parse_from_raw(
        ["clud", "--codex", "loop", "--repeat", "1h", "task"]
            .into_iter()
            .map(str::to_string)
            .collect(),
    );
    args.resolved_model_selection = crate::provider_catalog::resolve_for_launch(
        crate::backend::ModelProvider::Codex,
        None,
        None,
        None,
        None,
        true,
    )
    .unwrap();
    let target = crate::backend::resolve_launch_target(
        args.claude,
        args.codex,
        args.deepseek,
        args.harness,
        None,
        None,
    )
    .unwrap();
    let plan = crate::command::build_launch_plan_for_target(&args, target, "codex");

    // A later settings resolution must not alter the plan that the daemon
    // already recorded for this repeat.  The repeat command is derived from
    // `plan`, not from the mutable launch arguments.
    args.resolved_model_selection = crate::provider_catalog::resolve_for_launch(
        crate::backend::ModelProvider::Codex,
        Some("codex-terra"),
        Some("high"),
        None,
        None,
        true,
    )
    .unwrap();
    let command = build_repeat_once_command(&args, &plan).unwrap();
    assert!(command
        .windows(2)
        .any(|part| part == ["--model", "gpt-6-sol"]));
    assert!(command.windows(2).any(|part| part == ["--effort", "low"]));
    assert!(!command
        .iter()
        .any(|part| part.contains("terra") || part == "high"));
}

/// Kimi twin of `repeat_command_pins_the_normalized_model_effort_and_context`.
#[test]
fn repeat_command_pins_the_normalized_kimi_model_effort_and_context() {
    let args = Args::parse_from_raw(
        [
            "clud",
            "--kimi",
            "--model",
            "kimi-k3[1m]",
            "--effort",
            "max",
            "loop",
            "--repeat",
            "1h",
            "task",
        ]
        .into_iter()
        .map(str::to_string)
        .collect(),
    );
    let target = crate::backend::resolve_launch_target_with_provider(
        args.explicit_model_provider(),
        args.harness,
        None,
        None,
    )
    .unwrap();
    let plan = crate::command::build_launch_plan_for_target(&args, target, "claude");
    let command = build_repeat_once_command(&args, &plan).unwrap();
    assert!(command
        .windows(2)
        .any(|part| part == ["--model", "kimi-k3[1m]"]));
    assert!(command.windows(2).any(|part| part == ["--effort", "max"]));
    assert!(command
        .windows(2)
        .any(|part| part == ["--context-window", "1m"]));
}

#[test]
fn repeat_command_preserves_an_unknown_future_wire_model() {
    let args = Args::parse_from_raw(
        [
            "clud",
            "--codex",
            "--model",
            "gpt-5.7-nova",
            "loop",
            "--repeat",
            "1h",
            "task",
        ]
        .into_iter()
        .map(str::to_string)
        .collect(),
    );
    let target = crate::backend::resolve_launch_target(
        args.claude,
        args.codex,
        args.deepseek,
        args.harness,
        None,
        None,
    )
    .unwrap();
    let plan = crate::command::build_launch_plan_for_target(&args, target, "codex");
    let command = build_repeat_once_command(&args, &plan).unwrap();
    let model_index = command.iter().position(|part| part == "--model").unwrap();
    let repeated = crate::provider_catalog::resolve(
        Some(crate::backend::ModelProvider::Codex),
        Some(&command[model_index + 1]),
        None,
        None,
    )
    .unwrap()
    .unwrap();
    assert_eq!(repeated.model.as_deref(), Some("gpt-5.7-nova"));
    assert_eq!(repeated.wire_model.as_deref(), Some("gpt-5.7-nova"));
}

#[test]
fn repeat_command_preserves_mixed_case_and_marker_shaped_custom_models() {
    for model in ["My-Gateway-Model", "claude-wire-special"] {
        let args = Args::parse_from_raw(
            [
                "clud", "--claude", "--model", model, "loop", "--repeat", "1h", "task",
            ]
            .into_iter()
            .map(str::to_string)
            .collect(),
        );
        let target = crate::backend::resolve_launch_target(
            args.claude,
            args.codex,
            args.deepseek,
            args.harness,
            None,
            None,
        )
        .unwrap();
        let plan = crate::command::build_launch_plan_for_target(&args, target, "claude");
        let command = build_repeat_once_command(&args, &plan).unwrap();
        assert!(command.windows(2).any(|part| part == ["--model", model]));
    }
}

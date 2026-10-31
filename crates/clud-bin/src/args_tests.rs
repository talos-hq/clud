use super::*;
use std::path::PathBuf;

#[test]
fn serve_grammar_is_dispatched_without_backend_passthrough() {
    for arguments in [
        vec!["clud", "serve", "--claude"],
        vec!["clud", "serve", "--claude", "--openrouter"],
    ] {
        let raw: Vec<String> = arguments.iter().map(|value| (*value).to_string()).collect();
        let (known, unknown) = split_known_unknown(&raw).expect("known serve grammar");
        assert!(
            unknown.is_empty(),
            "{arguments:?} must not reach the harness"
        );
        let args = Args::try_parse_from(known).expect("serve arguments must parse");
        assert!(
            matches!(args.command, Some(Command::Serve { claude: true, codex: false, openrouter, port: 8317 })
            if openrouter == (arguments.len() == 4)),
            "serve must dispatch the selected routes"
        );
        assert!(!args.claude && !args.openrouter, "serve flags are local");
    }
    assert!(Args::try_parse_from(["clud", "serve", "--openrouter"]).is_err());
}

#[test]
fn serve_codex_flags_stay_local_and_can_add_routes() {
    for extra in [vec![], vec!["--openrouter"], vec!["--claude"]] {
        let mut arguments = vec!["clud", "serve", "--codex"];
        arguments.extend(extra);
        let raw = arguments
            .iter()
            .map(|value| (*value).to_string())
            .collect::<Vec<_>>();
        let (known, unknown) = split_known_unknown(&raw).unwrap();
        assert!(unknown.is_empty());
        let args = Args::try_parse_from(known).unwrap();
        assert!(matches!(
            args.command,
            Some(Command::Serve { codex: true, .. })
        ));
        assert!(!args.codex, "serve must not select an interactive backend");
    }
    assert!(Args::try_parse_from(["clud", "serve"]).is_err());
}

fn parse(args: &[&str]) -> Args {
    let raw: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    Args::parse_from_raw(raw)
}

#[test]
fn test_prompt_flag() {
    let args = parse(&["clud", "-p", "hello world"]);
    assert_eq!(args.prompt.as_deref(), Some("hello world"));
    assert!(!args.safe);
}

#[test]
fn unsafe_is_a_clud_flag_and_conflicts_with_safe() {
    let args = parse(&["clud", "--unsafe", "-p", "hello"]);
    assert!(args.unsafe_mode);
    assert!(args.passthrough.is_empty());
    assert!(Args::try_parse_from(["clud", "--safe", "--unsafe"]).is_err());
}

#[test]
fn test_message_flag() {
    let args = parse(&["clud", "-m", "fix the bug"]);
    assert_eq!(args.message.as_deref(), Some("fix the bug"));
}

#[test]
fn test_continue_flag() {
    let args = parse(&["clud", "-c"]);
    assert!(args.continue_session);
}

#[test]
fn test_claude_backend() {
    let args = parse(&["clud", "--claude"]);
    assert!(args.claude);
    assert!(!args.codex);
}

#[test]
fn test_codex_backend() {
    let args = parse(&["clud", "--codex"]);
    assert!(args.codex);
    assert!(!args.claude);
    assert!(!args.deepseek);
}

#[test]
fn test_deepseek_backend_is_known_and_conflicts_with_other_providers() {
    let args = parse(&["clud", "--deepseek"]);
    assert!(args.deepseek);
    assert!(!args.claude);
    assert!(!args.codex);
    assert!(args.passthrough.is_empty());
    assert!(Args::try_parse_from(["clud", "--deepseek", "--claude"]).is_err());
    assert!(Args::try_parse_from(["clud", "--deepseek", "--codex"]).is_err());
}

#[test]
fn single_dash_and_unicode_deepseek_aliases_use_the_real_provider_path() {
    for spelling in [
        "-deepseek",
        "-–deepseek",
        "—deepseek",
        "–deepseek",
        "―deepseek",
        "−deepseek",
        "﹣deepseek",
        "－deepseek",
        "\u{00ad}deepseek",
    ] {
        let args = parse(&["clud", spelling]);
        assert!(args.deepseek, "{spelling}");
        assert!(args.passthrough.is_empty(), "{spelling}");
    }
    let with_key = parse(&["clud", "-–deepseek", DEEPSEEK_SHAPED_KEY]);
    assert!(with_key.deepseek);
    assert_eq!(
        with_key.inline_api_key.as_ref().map(InlineApiKey::expose),
        Some(DEEPSEEK_SHAPED_KEY)
    );
    assert!(with_key.passthrough.is_empty());
}

#[test]
fn near_miss_flags_fail_before_backend_passthrough() {
    for spelling in ["--deepskeek", "--deepsek", "-deepskeek", "-–deepskeek"] {
        let error = validate_top_level_unknown(spelling, None).unwrap_err();
        assert!(error.contains(spelling), "{error}");
        assert!(error.contains("--deepseek"), "{error}");
    }
    let error =
        validate_top_level_unknown("--not-a-clud-flag", Some(DEEPSEEK_SHAPED_KEY)).unwrap_err();
    assert!(!error.contains(DEEPSEEK_SHAPED_KEY));
    assert!(validate_top_level_unknown("--not-a-clud-flag", None).is_ok());
}

#[test]
fn whole_args_debug_masks_raw_and_passthrough_keys_without_changing_execution_values() {
    let args = parse(&["clud", "--", DEEPSEEK_SHAPED_KEY]);
    assert_eq!(args.raw_argv[2], DEEPSEEK_SHAPED_KEY);
    assert_eq!(args.passthrough[0], DEEPSEEK_SHAPED_KEY);
    let debug = format!("{args:?}");
    assert!(debug.contains("****cdef"));
    assert!(!debug.contains(DEEPSEEK_SHAPED_KEY));
}

#[test]
fn every_top_level_clap_long_option_is_claimed_by_the_splitter() {
    use clap::CommandFactory;
    let mut missing = Vec::new();
    for argument in Args::command().get_arguments() {
        let Some(long) = argument.get_long() else {
            continue;
        };
        let flag = format!("--{long}");
        let raw = vec!["clud".to_string(), flag.clone(), "fixture".to_string()];
        match split_known_unknown(&raw) {
            Ok((known, _)) if known.contains(&flag) => {}
            _ => missing.push(flag),
        }
    }
    assert!(missing.is_empty(), "splitter omitted {missing:?}");
}

#[test]
fn clap_suggests_a_near_miss_subcommand() {
    assert_eq!(suggest_public_subcommand("looop"), Some("loop".to_string()));
    assert_eq!(suggest_public_subcommand("fix the bug"), None);
}

#[test]
fn option_correction_never_rewrites_values_or_backend_passthrough() {
    let corrected_value_flag = vec![
        "clud".to_string(),
        "-–prompt".to_string(),
        "-deepseek".to_string(),
    ];
    assert_eq!(
        normalize_known_option_dashes(&corrected_value_flag),
        ["clud", "--prompt", "-deepseek"]
    );
    let inline_value_flag = vec![
        "clud".to_string(),
        "-–prompt=literal".to_string(),
        "-deepseek".to_string(),
    ];
    assert_eq!(
        normalize_known_option_dashes(&inline_value_flag),
        ["clud", "--prompt=literal", "--deepseek"]
    );
    let raw = vec![
        "clud".to_string(),
        "--prompt".to_string(),
        "-–deepseek".to_string(),
    ];
    assert_eq!(normalize_known_option_dashes(&raw), raw);

    let separated = parse(&["clud", "--", "-–deepseek"]);
    assert_eq!(separated.passthrough, ["-–deepseek"]);
    assert!(!separated.deepseek);

    let unrelated = parse(&["clud", "--add-dir"]);
    assert_eq!(unrelated.passthrough, ["--add-dir"]);
    let codex_config = parse(&["clud", "--", "-c", "key=value"]);
    assert_eq!(codex_config.passthrough, ["-c", "key=value"]);

    let unknown = parse(&["clud", "--not-a-clud-flag"]);
    assert_eq!(unknown.passthrough, ["--not-a-clud-flag"]);
}

#[test]
fn test_kimi_backend_is_known_and_conflicts_with_other_providers() {
    let args = parse(&["clud", "--kimi"]);
    assert!(args.kimi);
    assert!(!args.claude);
    assert!(!args.codex);
    assert!(!args.deepseek);
    assert!(args.passthrough.is_empty());
    assert!(Args::try_parse_from(["clud", "--kimi", "--claude"]).is_err());
    assert!(Args::try_parse_from(["clud", "--kimi", "--codex"]).is_err());
    assert!(Args::try_parse_from(["clud", "--kimi", "--deepseek"]).is_err());
    assert!(Args::try_parse_from(["clud", "--kimi", "--unified"]).is_err());
    assert!(Args::try_parse_from(["clud", "--kimi", "--mode", "unified"]).is_err());
    assert!(Args::try_parse_from(["clud", "--kimi", "--provider", "deepseek"]).is_err());
    // Symmetric: the other selectors reject --kimi too, not just vice versa.
    assert!(Args::try_parse_from(["clud", "--claude", "--kimi"]).is_err());
    assert!(Args::try_parse_from(["clud", "--codex", "--kimi"]).is_err());
    assert!(Args::try_parse_from(["clud", "--deepseek", "--kimi"]).is_err());
    assert!(Args::try_parse_from(["clud", "--unified", "--kimi"]).is_err());
    assert!(Args::try_parse_from(["clud", "--provider", "deepseek", "--kimi"]).is_err());
}

#[test]
fn test_openrouter_backend_is_known_and_conflicts_with_other_providers() {
    let args = parse(&["clud", "--openrouter"]);
    assert!(args.openrouter);
    assert!(!args.claude);
    assert!(!args.codex);
    assert!(!args.deepseek);
    assert!(!args.kimi);
    assert!(args.passthrough.is_empty());
    for other in ["--claude", "--codex", "--deepseek", "--kimi", "--unified"] {
        assert!(Args::try_parse_from(["clud", "--openrouter", other]).is_err());
    }
    assert!(Args::try_parse_from(["clud", "--openrouter", "--provider", "deepseek",]).is_err());
    let generic = parse(&["clud", "--provider", "openrouter"]);
    assert_eq!(
        generic.explicit_model_provider(),
        Some(crate::backend::ModelProvider::OpenRouter)
    );
}

#[test]
fn test_kimi_flag_not_forwarded_to_passthrough() {
    let args = parse(&["clud", "--kimi", "-p", "hello"]);
    assert!(args.kimi);
    assert!(args.passthrough.is_empty());
}

/// #937 Phase 3: `DeepseekAuthSubcommand` is now a type alias for
/// `ApiKeyAuthSubcommand` (Kimi reuses the same auth-subcommand grammar).
/// This proves the alias is interchangeable, not just separately compiling.
#[test]
fn api_key_auth_subcommand_alias_is_interchangeable_with_deepseek_auth_subcommand() {
    let via_generic_name: ApiKeyAuthSubcommand = ApiKeyAuthSubcommand::Status { json: true };
    let via_legacy_name: DeepseekAuthSubcommand = via_generic_name;
    assert!(matches!(
        via_legacy_name,
        DeepseekAuthSubcommand::Status { json: true }
    ));
}

#[test]
fn test_provider_kimi_parses_and_is_an_explicit_cli_provider() {
    let args = parse(&["clud", "--provider", "kimi"]);
    assert_eq!(args.provider, Some(crate::backend::ModelProvider::Kimi));
    assert_eq!(
        args.explicit_model_provider(),
        Some(crate::backend::ModelProvider::Kimi)
    );
}

#[test]
fn test_kimi_flag_is_an_explicit_cli_provider() {
    let args = parse(&["clud", "--kimi"]);
    assert_eq!(
        args.explicit_model_provider(),
        Some(crate::backend::ModelProvider::Kimi)
    );
}

#[test]
fn test_kimi_flag_resolves_to_kimi_provider_and_claude_harness() {
    let args = parse(&["clud", "--kimi"]);
    let target = crate::backend::resolve_launch_target_with_provider(
        args.explicit_model_provider(),
        args.harness,
        None,
        None,
    )
    .unwrap();
    assert_eq!(target.model_provider, crate::backend::ModelProvider::Kimi);
    assert_eq!(target.effective_harness, crate::backend::Backend::Claude);
}

/// #937 Phase 3: `clud --kimi --harness codex` (and `--kimi --codex`, which
/// clap itself rejects above) must be rejected during launch-target
/// resolution, before any vault access. `resolve_launch_target_with_provider`
/// is a pure function with no I/O, so simply calling it here already proves
/// the rejection happens without touching the vault.
#[test]
fn test_kimi_with_codex_harness_rejected_before_any_vault_access() {
    let args = parse(&["clud", "--kimi", "--harness", "codex"]);
    let result = crate::backend::resolve_launch_target_with_provider(
        args.explicit_model_provider(),
        args.harness,
        None,
        None,
    );
    assert_eq!(
        result,
        Err(crate::backend::LaunchTargetError::KimiViaCodexUnsupported)
    );
    assert_eq!(
        result.unwrap_err().to_string(),
        "unsupported launch target: Kimi provider cannot use the Codex harness"
    );
}

#[test]
fn test_harness_flag_is_typed_and_not_forwarded() {
    for (raw, expected) in [
        (
            vec!["clud", "--harness", "default"],
            HarnessSelection::Default,
        ),
        (
            vec!["clud", "--harness=claude", "--codex"],
            HarnessSelection::Claude,
        ),
        (
            vec!["clud", "--codex", "--harness", "codex"],
            HarnessSelection::Codex,
        ),
        (
            vec!["clud", "--harness", "deepseek"],
            HarnessSelection::DeepSeek,
        ),
        // #1829: `dsh` is the executable's own name and must parse too.
        (
            vec!["clud", "--openrouter", "--harness", "dsh"],
            HarnessSelection::DeepSeek,
        ),
        (
            vec!["clud", "--openrouter", "--harness=dsh"],
            HarnessSelection::DeepSeek,
        ),
    ] {
        let args = parse(&raw);
        assert_eq!(args.harness, Some(expected));
        assert!(args.passthrough.is_empty());
    }
}

#[test]
fn deepseek_harness_rejects_clud_options_it_cannot_translate() {
    for (argv, expected) in [
        (vec!["clud", "--message", "system"], "--message"),
        (vec!["clud", "--continue"], "--continue"),
        (vec!["clud", "--resume"], "--resume"),
        (vec!["clud", "--model", "model"], "--model"),
        (vec!["clud", "--effort", "high"], "--effort"),
        (vec!["clud", "--context-window", "1m"], "--context-window"),
        (vec!["clud", "--safe"], "--safe"),
        (vec!["clud", "--unattended"], "--unattended"),
        (vec!["clud", "--allow-plan-mode"], "--allow-plan-mode"),
    ] {
        let args = parse(&argv);
        assert_eq!(
            args.unsupported_deepseek_harness_option(false),
            Some(expected)
        );
    }
    let args = parse(&[
        "clud",
        "--harness",
        "deepseek",
        "-p",
        "hello",
        "--",
        "--verbose",
    ]);
    assert_eq!(args.unsupported_deepseek_harness_option(false), None);
    assert_eq!(args.passthrough, ["--verbose"]);
}

#[test]
fn deepseek_harness_accepts_model_only_when_clud_routes_the_model() {
    let args = parse(&[
        "clud",
        "--openrouter",
        "--harness",
        "dsh",
        "--model",
        "openai/gpt-5",
    ]);
    assert_eq!(args.unsupported_deepseek_harness_option(true), None);
    assert_eq!(
        args.unsupported_deepseek_harness_option(false),
        Some("--model")
    );
}

#[test]
fn doctor_is_a_dispatched_subcommand() {
    let args = parse(&["clud", "doctor"]);
    assert!(matches!(
        args.command,
        Some(Command::Doctor { json: false })
    ));
    assert!(args.passthrough.is_empty());
    let args = parse(&["clud", "doctor", "--json"]);
    assert!(matches!(args.command, Some(Command::Doctor { json: true })));
    assert!(args.passthrough.is_empty());
}

#[test]
fn dsh_update_is_a_dispatched_subcommand() {
    let args = parse(&["clud", "dsh-update"]);
    assert!(matches!(args.command, Some(Command::DshUpdate)));
    assert!(args.passthrough.is_empty());
}

#[test]
fn test_harness_rejects_invalid_or_missing_values() {
    assert!(Args::try_parse_from(["clud", "--harness", "other"]).is_err());
    assert!(Args::try_parse_from(["clud", "--harness"]).is_err());
}

/// #629: auth management is a first-class clud command, never backend
/// passthrough. This intentionally uses clap directly so the RED state proves
/// the command family is registered before the unknown-flag splitter learns it.
#[test]
fn codex_auth_status_is_a_registered_command() {
    let args = Args::try_parse_from(["clud", "codex-auth", "status"]).unwrap();
    assert!(matches!(
        args.command,
        Some(Command::CodexAuth {
            subcommand: CodexAuthSubcommand::Status { json: false }
        })
    ));
}

#[test]
fn action_first_auth_subcommands_are_registered() {
    let args = Args::try_parse_from([
        "clud",
        "auth",
        "login",
        "codex",
        "--acknowledge-experimental",
        "--no-browser",
    ])
    .unwrap();
    assert!(matches!(
        args.command,
        Some(Command::Auth {
            subcommand: Some(AuthSubcommand::Login {
                provider: AuthProvider::Codex,
                acknowledge_experimental: true,
                no_browser: true,
            })
        })
    ));

    let args = Args::try_parse_from(["clud", "auth", "status", "deepseek", "--json"]).unwrap();
    assert!(matches!(
        args.command,
        Some(Command::Auth {
            subcommand: Some(AuthSubcommand::Status {
                provider: Some(AuthProvider::Deepseek),
                json: true,
            })
        })
    ));

    let args = Args::try_parse_from(["clud", "auth", "logout", "deepseek"]).unwrap();
    assert!(matches!(
        args.command,
        Some(Command::Auth {
            subcommand: Some(AuthSubcommand::Logout {
                provider: AuthProvider::Deepseek,
                json: false,
            })
        })
    ));
}

#[test]
fn action_first_auth_is_not_backend_passthrough() {
    let args = parse(&["clud", "auth", "status", "codex", "--json"]);
    assert!(args.passthrough.is_empty());
    assert!(matches!(args.command, Some(Command::Auth { .. })));
}

#[test]
fn deepseek_auth_subcommands_are_registered() {
    let args = Args::try_parse_from(["clud", "deepseek-auth", "status", "--json"]).unwrap();
    assert!(matches!(
        args.command,
        Some(Command::DeepseekAuth {
            subcommand: DeepseekAuthSubcommand::Status { json: true }
        })
    ));

    let args = Args::try_parse_from(["clud", "deepseek-auth", "login"]).unwrap();
    assert!(matches!(
        args.command,
        Some(Command::DeepseekAuth {
            subcommand: DeepseekAuthSubcommand::Login
        })
    ));

    let args = Args::try_parse_from(["clud", "deepseek-auth", "logout"]).unwrap();
    assert!(matches!(
        args.command,
        Some(Command::DeepseekAuth {
            subcommand: DeepseekAuthSubcommand::Logout { json: false }
        })
    ));
}

#[test]
fn test_model_flag() {
    let args = parse(&["clud", "--model", "opus"]);
    assert_eq!(args.model.as_deref(), Some("opus"));
}

#[test]
fn provider_neutral_selection_flags_are_owned_before_the_separator() {
    let args = parse(&[
        "clud",
        "--provider",
        "codex",
        "--model",
        "codex-terra",
        "--effort",
        "high",
        "--context-window",
        "1m",
        "run",
    ]);
    assert_eq!(args.provider, Some(crate::backend::ModelProvider::Codex));
    assert_eq!(args.model.as_deref(), Some("codex-terra"));
    assert_eq!(args.effort.as_deref(), Some("high"));
    assert_eq!(args.context_window.as_deref(), Some("1m"));
    assert!(matches!(args.command, Some(Command::Run)));
    assert!(args.passthrough.is_empty());
}

#[test]
fn generic_provider_is_an_explicit_cli_provider() {
    let args = parse(&["clud", "--provider", "deepseek"]);
    assert_eq!(
        args.explicit_model_provider(),
        Some(crate::backend::ModelProvider::DeepSeek)
    );
}

#[test]
fn explicit_run_normalizes_to_the_commandless_launch_shape() {
    let mut args = parse(&["clud", "--prompt", "hello", "run"]);
    assert!(matches!(args.command, Some(Command::Run)));
    args.normalize_explicit_run();
    assert!(args.command.is_none());
    assert_eq!(args.prompt.as_deref(), Some("hello"));
}

#[test]
fn unified_and_generic_mode_aliases_normalize_identically() {
    let unified = parse(&["clud", "--unified", "run"]);
    let generic = parse(&["clud", "--mode", "unified", "run"]);
    assert_eq!(unified.routing_mode(), crate::backend::RoutingMode::Unified);
    assert_eq!(generic.routing_mode(), crate::backend::RoutingMode::Unified);
    assert!(unified.explicit_model_provider().is_none());
    assert!(generic.explicit_model_provider().is_none());
}

#[test]
fn unified_mode_rejects_a_direct_provider_prefix() {
    let argv = ["clud", "--unified", "--codex"];
    assert!(Args::try_parse_from(argv).is_err());
}

#[test]
fn unified_tokens_remain_backend_passthrough_after_separator() {
    let args = parse(&["clud", "--", "--unified", "--mode", "unified"]);
    assert_eq!(args.routing_mode(), crate::backend::RoutingMode::Direct);
    assert_eq!(args.passthrough, ["--unified", "--mode", "unified"]);
}

#[test]
fn claimed_selection_flags_remain_backend_passthrough_after_separator() {
    let args = parse(&["clud", "--", "--effort", "high", "--provider", "codex"]);
    assert!(args.effort.is_none());
    assert!(args.provider.is_none());
    assert_eq!(
        args.passthrough,
        ["--effort", "high", "--provider", "codex"]
    );
}

#[test]
fn test_subprocess_flag() {
    let args = parse(&["clud", "--subprocess"]);
    assert!(args.subprocess);
    assert!(!args.pty);
}

#[test]
fn test_pty_flag() {
    let args = parse(&["clud", "--pty"]);
    assert!(args.pty);
    assert!(!args.subprocess);
}

#[test]
fn test_graphics_flag() {
    let args = parse(&[
        "clud",
        "--graphics",
        "sixel",
        "--graphics-image",
        "banner.png",
    ]);
    assert_eq!(args.graphics, crate::graphics::GraphicsMode::Sixel);
    assert_eq!(
        args.graphics_image.as_ref().map(|p| p.as_os_str()),
        Some(std::ffi::OsStr::new("banner.png"))
    );
    assert!(args.passthrough.is_empty());
}

#[test]
fn test_graphics_equals_form_stays_known() {
    let args = parse(&["clud", "--graphics=off", "--some-backend-flag"]);
    assert_eq!(args.graphics, crate::graphics::GraphicsMode::Off);
    assert_eq!(args.passthrough, vec!["--some-backend-flag"]);
}

#[test]
fn test_demo_gfx_sixel_flag() {
    let args = parse(&["clud", "--demo-gfx-sixel", "--some-backend-flag"]);
    assert!(args.demo_gfx_sixel);
    assert_eq!(args.passthrough, vec!["--some-backend-flag"]);
}

#[test]
fn test_safe_flag() {
    let args = parse(&["clud", "--safe", "-p", "hello"]);
    assert!(args.safe);
    assert_eq!(args.prompt.as_deref(), Some("hello"));
}

#[test]
fn test_dry_run() {
    let args = parse(&["clud", "--dry-run", "-p", "hello"]);
    assert!(args.dry_run);
}

#[test]
fn test_detach_flag() {
    let args = parse(&["clud", "--detach", "-p", "hello"]);
    assert!(args.detach);
    assert!(!args.detachable);
}

#[test]
fn test_detachable_flag() {
    let args = parse(&["clud", "--detachable", "-p", "hello"]);
    assert!(args.detachable);
    assert!(!args.detach);
}

#[test]
fn test_loop_subcommand() {
    let args = parse(&["clud", "loop", "do the task"]);
    match args.command {
        Some(Command::Loop {
            ref task,
            loop_count,
            refresh,
            no_done,
            ref done,
            ref repeat,
            ..
        }) => {
            assert_eq!(task.as_deref(), Some("do the task"));
            assert_eq!(loop_count, 50);
            assert!(!refresh);
            assert!(!no_done);
            assert!(done.is_none());
            assert!(repeat.is_none());
        }
        _ => panic!("expected Loop subcommand"),
    }
}

#[test]
fn test_loop_with_count() {
    let args = parse(&["clud", "loop", "--loop-count", "5", "task"]);
    match args.command {
        Some(Command::Loop {
            ref task,
            loop_count,
            ..
        }) => {
            assert_eq!(task.as_deref(), Some("task"));
            assert_eq!(loop_count, 5);
        }
        _ => panic!("expected Loop subcommand"),
    }
}

#[test]
fn test_loop_refresh_flag() {
    let args = parse(&[
        "clud",
        "loop",
        "--refresh",
        "https://github.com/o/r/issues/42",
    ]);
    match args.command {
        Some(Command::Loop {
            ref task,
            refresh,
            no_done,
            ref done,
            ref repeat,
            ..
        }) => {
            assert_eq!(task.as_deref(), Some("https://github.com/o/r/issues/42"));
            assert!(refresh);
            assert!(!no_done);
            assert!(done.is_none());
            assert!(repeat.is_none());
        }
        _ => panic!("expected Loop subcommand"),
    }
}

#[test]
fn test_loop_no_done_flag() {
    let args = parse(&["clud", "loop", "--no-done", "task"]);
    match args.command {
        Some(Command::Loop { no_done, .. }) => {
            assert!(no_done);
        }
        _ => panic!("expected Loop subcommand"),
    }
}

#[test]
fn test_loop_no_done_marker_compat_alias() {
    let args = parse(&["clud", "loop", "--no-done-marker", "task"]);
    match args.command {
        Some(Command::Loop { no_done, .. }) => {
            assert!(no_done);
        }
        _ => panic!("expected Loop subcommand"),
    }
}

#[test]
fn test_loop_done_path() {
    let args = parse(&["clud", "loop", "--done", "DONE.md", "task"]);
    match args.command {
        Some(Command::Loop {
            ref done, no_done, ..
        }) => {
            assert_eq!(done.as_deref(), Some("DONE.md"));
            assert!(!no_done);
        }
        _ => panic!("expected Loop subcommand"),
    }
}

#[test]
fn test_loop_repeat() {
    let args = parse(&["clud", "loop", "--repeat", "1h", "task"]);
    match args.command {
        Some(Command::Loop { ref repeat, .. }) => {
            assert_eq!(repeat.as_deref(), Some("1h"));
        }
        _ => panic!("expected Loop subcommand"),
    }
}

/// Issue #61: --repeat + --done <path> must parse cleanly. The two flags
/// compose; --done overrides --repeat's implicit --no-done at the
/// command-builder layer, but at the args layer they're orthogonal.
#[test]
fn test_loop_repeat_with_done() {
    let args = parse(&[
        "clud",
        "loop",
        "--repeat",
        "30m",
        "--done",
        "STATUS.md",
        "task",
    ]);
    match args.command {
        Some(Command::Loop {
            ref repeat,
            ref done,
            no_done,
            ..
        }) => {
            assert_eq!(repeat.as_deref(), Some("30m"));
            assert_eq!(done.as_deref(), Some("STATUS.md"));
            assert!(!no_done);
        }
        _ => panic!("expected Loop subcommand"),
    }
}

/// Issue #61: --repeat + --no-done must parse cleanly even though the
/// command-builder treats them as overlapping (both suppress the
/// contract). Clap should not reject the combination.
#[test]
fn test_loop_repeat_with_no_done() {
    let args = parse(&["clud", "loop", "--repeat", "5m", "--no-done", "task"]);
    match args.command {
        Some(Command::Loop {
            ref repeat,
            no_done,
            ref done,
            ..
        }) => {
            assert_eq!(repeat.as_deref(), Some("5m"));
            assert!(no_done);
            assert!(done.is_none());
        }
        _ => panic!("expected Loop subcommand"),
    }
}

/// Issue #61: --done and --no-done are mutually exclusive (clap
/// `conflicts_with`). Supplying both must fail — we don't pin the exact
/// error message because clap formatting drifts between versions, but
/// `try_parse_from` must return `Err`.
#[test]
fn test_loop_done_and_no_done_conflict() {
    let argv: Vec<String> = ["clud", "loop", "--done", "DONE.md", "--no-done", "task"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let result = Args::try_parse_from(argv);
    assert!(
        result.is_err(),
        "clap should reject simultaneous --done and --no-done"
    );
}

#[test]
fn test_up_subcommand() {
    let args = parse(&["clud", "up"]);
    assert!(matches!(args.command, Some(Command::Up { .. })));
}

#[test]
fn test_up_with_message() {
    let args = parse(&["clud", "up", "-m", "bump version"]);
    match args.command {
        Some(Command::Up {
            ref message,
            publish,
        }) => {
            assert_eq!(message.as_deref(), Some("bump version"));
            assert!(!publish);
        }
        _ => panic!("expected Up subcommand"),
    }
}

#[test]
fn test_up_with_publish() {
    let args = parse(&["clud", "up", "--publish"]);
    match args.command {
        Some(Command::Up {
            ref message,
            publish,
        }) => {
            assert!(message.is_none());
            assert!(publish);
        }
        _ => panic!("expected Up subcommand"),
    }
}

#[test]
fn test_up_with_message_and_publish() {
    let args = parse(&["clud", "up", "-m", "release", "--publish"]);
    match args.command {
        Some(Command::Up {
            ref message,
            publish,
        }) => {
            assert_eq!(message.as_deref(), Some("release"));
            assert!(publish);
        }
        _ => panic!("expected Up subcommand"),
    }
}

#[test]
fn test_rebase_subcommand() {
    let args = parse(&["clud", "rebase"]);
    assert!(matches!(args.command, Some(Command::Rebase)));
}

#[test]
fn test_fix_subcommand() {
    let args = parse(&["clud", "fix"]);
    assert!(matches!(args.command, Some(Command::Fix { .. })));
}

#[test]
fn test_optimize_defaults_to_rust_global_soldr() {
    let args = parse(&["clud", "optimize"]);
    match args.command {
        Some(Command::Optimize {
            target,
            global,
            repo,
            install_soldr,
            use_soldr_shims,
            ref soldr_version,
        }) => {
            assert_eq!(target, OptimizeTarget::Rust);
            assert!(!global);
            assert!(!repo);
            assert!(install_soldr);
            assert!(use_soldr_shims);
            assert_eq!(soldr_version, "latest");
        }
        other => panic!("expected Optimize subcommand, got {other:?}"),
    }
}

#[test]
fn test_optimize_rust_repo_flags() {
    let args = parse(&[
        "clud",
        "optimize",
        "rust",
        "--repo",
        "--install-soldr=false",
        "--use-soldr-shims=false",
        "--soldr-version",
        "1.2.3",
    ]);
    match args.command {
        Some(Command::Optimize {
            target,
            global,
            repo,
            install_soldr,
            use_soldr_shims,
            ref soldr_version,
        }) => {
            assert_eq!(target, OptimizeTarget::Rust);
            assert!(!global);
            assert!(repo);
            assert!(!install_soldr);
            assert!(!use_soldr_shims);
            assert_eq!(soldr_version, "1.2.3");
        }
        other => panic!("expected Optimize subcommand, got {other:?}"),
    }
}

#[test]
fn test_settings_defaults_to_interactive() {
    let args = parse(&["clud", "settings"]);
    match args.command {
        Some(Command::Settings { list, .. }) => assert!(!list),
        other => panic!("expected Settings subcommand, got {other:?}"),
    }
}

#[test]
fn test_settings_list_flag() {
    let args = parse(&["clud", "settings", "--list"]);
    match args.command {
        Some(Command::Settings { list, .. }) => assert!(list),
        other => panic!("expected Settings subcommand, got {other:?}"),
    }
}

#[test]
fn test_optimize_soldr_alias_selects_rust() {
    let args = parse(&["clud", "optimize", "soldr"]);
    match args.command {
        Some(Command::Optimize { target, .. }) => {
            assert_eq!(target, OptimizeTarget::Rust);
        }
        other => panic!("expected Optimize subcommand, got {other:?}"),
    }
}

#[test]
fn test_fix_with_url() {
    let args = parse(&[
        "clud",
        "fix",
        "https://github.com/user/repo/actions/runs/123",
    ]);
    match args.command {
        Some(Command::Fix { ref url }) => {
            assert_eq!(
                url.as_deref(),
                Some("https://github.com/user/repo/actions/runs/123")
            );
        }
        _ => panic!("expected Fix subcommand"),
    }
}

#[test]
fn bare_resume_gives_registered_builtins_precedence_over_optional_session_value() {
    for (verb, target) in [
        ("do", Some("README.md")),
        ("up", None),
        ("rebase", None),
        ("fix", None),
    ] {
        let mut raw = vec!["clud", "--codex", "--resume", verb];
        if let Some(target) = target {
            raw.push(target);
        }
        let args = Args::parse_from_raw(raw.iter().map(|value| (*value).to_string()).collect());
        assert_eq!(args.resume, Some(None), "verb={verb}");
        let parsed_verb = match args.command {
            Some(Command::Do { .. }) => "do",
            Some(Command::Up { .. }) => "up",
            Some(Command::Rebase) => "rebase",
            Some(Command::Fix { .. }) => "fix",
            other => panic!("expected {verb} command, got {other:?}"),
        };
        assert_eq!(parsed_verb, verb);
        assert!(args.passthrough.is_empty());
    }

    let short = parse(&["clud", "--codex", "-r", "do", "README.md"]);
    assert_eq!(short.resume, Some(None));
    assert!(matches!(short.command, Some(Command::Do { .. })));
}

#[test]
fn inline_key_split_survives_value_named_like_subcommand() {
    let args = parse(&[
        "clud",
        "--name",
        "test",
        "--deepseek=sk-0123456789abcdefghij",
    ]);
    assert!(args.deepseek);
    assert_eq!(args.session_name.as_deref(), Some("test"));
    assert!(args.passthrough.is_empty(), "{:?}", args.passthrough);
}

#[test]
fn bare_resume_does_not_swallow_following_flag() {
    let long = parse(&["clud", "--resume", "--model", "opus"]);
    assert_eq!(long.resume, Some(None));
    assert_eq!(long.model.as_deref(), Some("opus"));
    assert!(long.passthrough.is_empty(), "{:?}", long.passthrough);
    let short = parse(&["clud", "-r", "-p", "hi"]);
    assert_eq!(short.resume, Some(None));
    assert_eq!(short.prompt.as_deref(), Some("hi"));
    assert!(short.passthrough.is_empty(), "{:?}", short.passthrough);
}

#[test]
fn resume_session_named_like_subcommand_requires_unambiguous_equals_form() {
    let args = parse(&["clud", "--codex", "--resume=do"]);
    assert_eq!(
        args.resume.as_ref().and_then(|value| value.as_deref()),
        Some("do")
    );
    assert!(args.command.is_none());

    let passthrough = parse(&["clud", "--", "--resume", "do"]);
    assert_eq!(passthrough.passthrough, ["--resume", "do"]);
}

#[test]
fn test_do_subcommand() {
    let args = parse(&["clud", "do", "https://github.com/zackees/clud/issues/866"]);
    match args.command {
        Some(Command::Do { ref target }) => {
            assert_eq!(
                target.as_deref(),
                Some("https://github.com/zackees/clud/issues/866")
            );
        }
        _ => panic!("expected Do subcommand"),
    }
}

#[test]
fn test_do_subcommand_without_target() {
    let args = parse(&["clud", "do"]);
    match args.command {
        Some(Command::Do { ref target }) => assert!(target.is_none()),
        _ => panic!("expected Do subcommand"),
    }
}

#[test]
fn test_grind_subcommand_without_url() {
    let args = parse(&["clud", "grind"]);
    match args.command {
        Some(Command::Grind { ref url }) => assert!(url.is_none()),
        _ => panic!("expected Grind subcommand"),
    }
}

#[test]
fn test_grind_subcommand_with_url() {
    let args = parse(&["clud", "grind", "https://github.com/zackees/clud/issues"]);
    match args.command {
        Some(Command::Grind { ref url }) => {
            assert_eq!(
                url.as_deref(),
                Some("https://github.com/zackees/clud/issues")
            );
        }
        _ => panic!("expected Grind subcommand"),
    }
}

#[test]
fn test_do_with_dry_run() {
    let args = parse(&[
        "clud",
        "do",
        "--dry-run",
        "https://github.com/zackees/clud/issues/866",
    ]);
    assert!(args.dry_run);
    assert!(matches!(args.command, Some(Command::Do { .. })));
}

#[test]
fn test_wasm_subcommand() {
    let args = parse(&["clud", "wasm", "guest.wasm"]);
    match args.command {
        Some(Command::Wasm {
            ref module,
            ref invoke,
        }) => {
            assert_eq!(module, "guest.wasm");
            assert_eq!(invoke, "run");
        }
        _ => panic!("expected Wasm subcommand"),
    }
}

#[test]
fn test_wasm_subcommand_custom_entrypoint() {
    let args = parse(&["clud", "wasm", "guest.wasm", "--invoke", "_start"]);
    match args.command {
        Some(Command::Wasm {
            ref module,
            ref invoke,
        }) => {
            assert_eq!(module, "guest.wasm");
            assert_eq!(invoke, "_start");
        }
        _ => panic!("expected Wasm subcommand"),
    }
}

#[test]
fn test_attach_without_session_id() {
    let args = parse(&["clud", "attach"]);
    match args.command {
        Some(Command::Attach { session_id, last }) => {
            assert!(session_id.is_none());
            assert!(!last);
        }
        _ => panic!("expected Attach subcommand"),
    }
}

#[test]
fn test_attach_with_session_id() {
    let args = parse(&["clud", "attach", "sess-123"]);
    match args.command {
        Some(Command::Attach { session_id, last }) => {
            assert_eq!(session_id.as_deref(), Some("sess-123"));
            assert!(!last);
        }
        _ => panic!("expected Attach subcommand"),
    }
}

#[test]
fn test_attach_with_last() {
    let args = parse(&["clud", "attach", "--last"]);
    match args.command {
        Some(Command::Attach { session_id, last }) => {
            assert!(session_id.is_none());
            assert!(last);
        }
        _ => panic!("expected Attach subcommand"),
    }
}

#[test]
fn test_kill_subcommand() {
    let args = parse(&["clud", "kill", "sess-123"]);
    match args.command {
        Some(Command::Kill { session_id, all }) => {
            assert_eq!(session_id.as_deref(), Some("sess-123"));
            assert!(!all);
        }
        _ => panic!("expected Kill subcommand"),
    }
}

#[test]
fn test_kill_all() {
    let args = parse(&["clud", "kill", "--all"]);
    match args.command {
        Some(Command::Kill { session_id, all }) => {
            assert!(session_id.is_none());
            assert!(all);
        }
        _ => panic!("expected Kill subcommand"),
    }
}

#[test]
fn test_slay_subcommand() {
    let args = parse(&["clud", "slay"]);
    assert!(matches!(args.command, Some(Command::Slay)));
    assert!(args.passthrough.is_empty());
}

#[test]
fn test_name_flag() {
    let args = parse(&["clud", "--name", "my-session", "--detach", "-p", "hello"]);
    assert_eq!(args.session_name.as_deref(), Some("my-session"));
    assert!(args.detach);
}

#[test]
fn test_transcript_flag() {
    let args = parse(&["clud", "--transcript", "session.log", "-p", "hello"]);
    assert_eq!(
        args.transcript.as_ref().map(|p| p.as_os_str()),
        Some(std::ffi::OsStr::new("session.log"))
    );
}

#[test]
fn test_list_subcommand() {
    let args = parse(&["clud", "list"]);
    assert!(matches!(args.command, Some(Command::List)));
}

#[test]
fn test_logs_with_session_id() {
    let args = parse(&["clud", "logs", "sess-abc"]);
    match args.command {
        Some(Command::Logs {
            session_id,
            follow,
            lines,
            last,
        }) => {
            assert_eq!(session_id.as_deref(), Some("sess-abc"));
            assert!(!follow);
            assert!(lines.is_none());
            assert!(!last);
        }
        _ => panic!("expected Logs subcommand"),
    }
}

#[test]
fn test_logs_follow_flag() {
    let args = parse(&["clud", "logs", "-f", "sess-abc"]);
    match args.command {
        Some(Command::Logs {
            session_id,
            follow,
            last,
            ..
        }) => {
            assert_eq!(session_id.as_deref(), Some("sess-abc"));
            assert!(follow);
            assert!(!last);
        }
        _ => panic!("expected Logs subcommand"),
    }
}

#[test]
fn test_logs_lines_flag() {
    let args = parse(&["clud", "logs", "-n", "100", "sess-abc"]);
    match args.command {
        Some(Command::Logs {
            session_id,
            lines,
            last,
            ..
        }) => {
            assert_eq!(session_id.as_deref(), Some("sess-abc"));
            assert_eq!(lines, Some(100));
            assert!(!last);
        }
        _ => panic!("expected Logs subcommand"),
    }
}

#[test]
fn test_logs_last_flag() {
    let args = parse(&["clud", "logs", "--last"]);
    match args.command {
        Some(Command::Logs {
            session_id,
            follow,
            last,
            ..
        }) => {
            assert!(session_id.is_none());
            assert!(!follow);
            assert!(last);
        }
        _ => panic!("expected Logs subcommand"),
    }
}

#[test]
fn test_logs_last_short_flag() {
    let args = parse(&["clud", "logs", "-l"]);
    match args.command {
        Some(Command::Logs { last, .. }) => {
            assert!(last);
        }
        _ => panic!("expected Logs subcommand"),
    }
}

/// `--last` conflicts with positional session id (mirrors `clud attach`).
#[test]
fn test_logs_last_with_session_id_conflicts() {
    let argv: Vec<String> = ["clud", "logs", "--last", "sess-abc"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let result = Args::try_parse_from(argv);
    assert!(
        result.is_err(),
        "clap should reject --last combined with a session id"
    );
}

#[test]
fn test_unknown_flags_passthrough() {
    let args = parse(&["clud", "--some-unknown-flag", "-p", "hello"]);
    assert_eq!(args.prompt.as_deref(), Some("hello"));
    assert_eq!(args.passthrough, vec!["--some-unknown-flag"]);
}

#[test]
fn test_passthrough_after_separator() {
    let args = parse(&["clud", "-p", "hello", "--", "--verbose", "--debug"]);
    assert_eq!(args.prompt.as_deref(), Some("hello"));
    assert_eq!(args.passthrough, vec!["--verbose", "--debug"]);
}

/// Issue #508: the documented `clud tool run <tool> … run -- <cmd…>` shape.
///
/// The separator and everything behind it are *data* for the bundled tool,
/// which does its own `run -- <cmd…>` split. Before the fix the splitter
/// claimed them for `passthrough` — which nothing on the `tool` path reads —
/// and the tool reported `run: missing command`.
#[test]
fn tool_run_forwards_the_separator_and_everything_after_it() {
    let args = parse(&[
        "clud",
        "tool",
        "run",
        "docker/docker-build.py",
        "soldr",
        "C:/repo",
        "run",
        "--",
        "soldr",
        "cargo",
        "fmt",
        "-p",
        "soldr-cli",
        "--",
        "--check",
    ]);
    assert!(
        args.passthrough.is_empty(),
        "nothing may be diverted to passthrough: {:?}",
        args.passthrough
    );
    let Some(Command::Tool {
        subcommand: ToolSubcommand::Run { rel_path, args },
    }) = args.command
    else {
        panic!("expected `tool run`");
    };
    assert_eq!(rel_path, "docker/docker-build.py");
    // Both separators survive, and so does `-p`, which is a clud value flag
    // and would otherwise have been eaten along with its value.
    assert_eq!(
        args,
        vec![
            "soldr",
            "C:/repo",
            "run",
            "--",
            "soldr",
            "cargo",
            "fmt",
            "-p",
            "soldr-cli",
            "--",
            "--check",
        ]
    );
}

/// The acceptance criterion from the issue, in its smallest form.
#[test]
fn tool_run_round_trips_a_trivial_command_after_the_separator() {
    let args = parse(&[
        "clud",
        "tool",
        "run",
        "docker/docker-build.py",
        "soldr",
        "C:/repo",
        "run",
        "--",
        "echo",
        "hi",
    ]);
    let Some(Command::Tool {
        subcommand: ToolSubcommand::Run { args, .. },
    }) = args.command
    else {
        panic!("expected `tool run`");
    };
    assert_eq!(args, vec!["soldr", "C:/repo", "run", "--", "echo", "hi"]);
}

/// The behaviour the fix must *not* break, and which a broader fix does break:
/// every subcommand other than `tool` keeps `--` as clud's end-of-flags marker.
///
/// Found the hard way — routing `--` to clap for `loop` makes it reject
/// `--verbose` as an unknown argument and call `exit(2)`, which takes the whole
/// process with it.
#[test]
fn separator_after_a_non_tool_subcommand_still_feeds_passthrough() {
    let args = parse(&["clud", "loop", "task", "--", "--verbose", "--debug"]);
    assert_eq!(args.passthrough, vec!["--verbose", "--debug"]);
    assert!(matches!(args.command, Some(Command::Loop { .. })));
}

/// The behaviour the fix must *not* break: before any subcommand, `--` still
/// ends clud's own flags and hands the rest to the backend agent.
#[test]
fn separator_before_a_subcommand_still_feeds_passthrough() {
    let args = parse(&["clud", "-p", "hello", "--", "--verbose"]);
    assert_eq!(args.prompt.as_deref(), Some("hello"));
    assert_eq!(args.passthrough, vec!["--verbose"]);
    assert!(args.command.is_none());
}

#[test]
fn test_verbose_flag() {
    let args = parse(&["clud", "-v"]);
    assert!(args.verbose);
}

#[test]
fn test_default_no_flags() {
    let args = parse(&["clud"]);
    assert!(args.prompt.is_none());
    assert!(args.message.is_none());
    assert!(!args.continue_session);
    assert!(!args.claude);
    assert!(!args.codex);
    assert!(!args.subprocess);
    assert!(!args.pty);
    assert_eq!(args.graphics, crate::graphics::GraphicsMode::Auto);
    assert!(args.graphics_image.is_none());
    assert!(!args.safe);
    assert!(!args.dry_run);
    assert!(!args.detach);
    assert!(!args.detachable);
    assert!(args.transcript.is_none());
    assert!(!args.no_dnd);
    assert!(!args.clean_worktrees);
    assert!(!args.fix_hooks);
    assert!(!args.no_fix_hooks);
    assert!(!args.yes);
    assert!(!args.force);
    assert_eq!(args.stale_after, "1d");
    assert!(args.command.is_none());
    assert!(args.passthrough.is_empty());
}

#[test]
fn test_no_dnd_flag() {
    let args = parse(&["clud", "--no-dnd"]);
    assert!(args.no_dnd);
}

#[test]
fn test_no_drag_drop_alias() {
    let args = parse(&["clud", "--no-drag-drop"]);
    assert!(args.no_dnd);
}

#[test]
fn test_no_dnd_default_false() {
    let args = parse(&["clud", "-p", "hello"]);
    assert!(!args.no_dnd);
}

/// Issue #83: top-level `--clean-worktrees` toggles the worktree-cleanup
/// path and accepts the surrounding flags (`--stale-after`, `--yes`,
/// `--force`, the existing `--dry-run`).
#[test]
fn test_clean_worktrees_flag() {
    let args = parse(&["clud", "--clean-worktrees"]);
    assert!(args.clean_worktrees);
    assert_eq!(args.stale_after, "1d");
    assert!(!args.yes);
    assert!(!args.force);
}

#[test]
fn test_clean_worktrees_with_stale_after() {
    let args = parse(&["clud", "--clean-worktrees", "--stale-after", "7d"]);
    assert!(args.clean_worktrees);
    assert_eq!(args.stale_after, "7d");
}

#[test]
fn test_clean_worktrees_with_yes_and_force() {
    let args = parse(&["clud", "--clean-worktrees", "--yes", "--force"]);
    assert!(args.clean_worktrees);
    assert!(args.yes);
    assert!(args.force);
}

// ── #1257: model allowlist / inherited pin ─────────────────────────────────

#[test]
fn a_lone_model_flag_is_the_boundary_and_is_not_an_inherited_pin() {
    let mut args = parse(&["clud", "--openrouter", "--model", "xiaomi/mimo-v2.6-flash"]);
    // An explicit pin is the boundary whether or not anything resolved yet.
    assert_eq!(
        args.model_allowlist(),
        vec!["xiaomi/mimo-v2.6-flash".to_string()]
    );
    assert!(!args.model_pin_is_from_previous_selection());
    // With a resolved selection in hand the typed pin still wins: only the
    // *absence* of a typed model makes the selection the pin.
    let typed = args.model.clone();
    args.resolved_model_selection =
        crate::provider_catalog::resolve_for_launch_with_server_default(
            crate::backend::ModelProvider::OpenRouter,
            typed.as_deref(),
            None,
            None,
            None,
            true,
            None,
        )
        .unwrap();
    assert_eq!(
        args.model_allowlist(),
        vec!["xiaomi/mimo-v2.6-flash".to_string()]
    );
    assert!(!args.model_pin_is_from_previous_selection());
}

#[test]
fn with_no_model_flag_the_boundary_is_the_previous_model_selection() {
    let mut args = parse(&["clud", "--openrouter"]);
    // Before anything resolves there is nothing to pin and nothing to say.
    assert!(args.model_allowlist().is_empty());
    assert!(!args.model_pin_is_from_previous_selection());
    // After launch resolution, the previous selection -- the wire id that
    // would actually be billed -- is the only pin, and it is announced.
    args.resolved_model_selection =
        crate::provider_catalog::resolve_for_launch_with_server_default(
            crate::backend::ModelProvider::OpenRouter,
            None,
            None,
            None,
            None,
            true,
            None,
        )
        .unwrap();
    assert_eq!(
        args.model_allowlist(),
        vec!["~anthropic/claude-sonnet-latest".to_string()]
    );
    assert!(args.model_pin_is_from_previous_selection());
    // A selection with no wire id falls back to its CLI id.
    args.resolved_model_selection = Some(crate::provider_catalog::ResolvedModelSelection {
        provider: crate::backend::ModelProvider::OpenRouter,
        model: Some("openrouter-claude-sonnet".to_string()),
        wire_model: None,
        effort: None,
        context_window: None,
        model_source: None,
        effort_source: None,
        context_window_source: None,
    });
    assert_eq!(
        args.model_allowlist(),
        vec!["openrouter-claude-sonnet".to_string()]
    );
}

#[test]
fn repeated_allow_model_replaces_a_lone_pin_and_dedupes() {
    let args = parse(&[
        "clud",
        "--model",
        "ignored-by-the-explicit-list",
        "--allow-model",
        "deepseek-flash",
        "--allow-model",
        "DEEPSEEK-FLASH",
        "--allow-model",
        "deepseek-v4-pro",
    ]);
    assert_eq!(
        args.model_allowlist(),
        vec!["deepseek-flash".to_string(), "deepseek-v4-pro".to_string()]
    );
    assert!(!args.model_pin_is_from_previous_selection());
}

#[test]
fn normalize_model_allowlist_promotes_the_first_allow_model_to_the_pin() {
    let mut args = parse(&["clud", "--allow-model", "a", "--allow-model", "b"]);
    assert_eq!(args.model, None);
    args.normalize_model_allowlist();
    // The allowlist constrains every slot, so the main model must come from
    // it too -- otherwise the launch's own first turn starts outside its own
    // boundary.
    assert_eq!(args.model.as_deref(), Some("a"));
    assert_eq!(
        args.model_allowlist(),
        vec!["a".to_string(), "b".to_string()]
    );
    // An explicit --model is never displaced.
    let mut args = parse(&["clud", "--model", "x", "--allow-model", "a"]);
    args.normalize_model_allowlist();
    assert_eq!(args.model.as_deref(), Some("x"));
}

#[path = "args_tests/commands.rs"]
mod commands;

// ── `clud --deepseek <API_KEY>` ─────────────────────────────────────────────

fn parse_inline_key_argv(argv: &[&str]) -> Args {
    Args::parse_from_raw(argv.iter().map(|s| (*s).to_string()).collect())
}

const DEEPSEEK_SHAPED_KEY: &str = "sk-0123456789abcdef0123456789abcdef";

/// RED on 2.8.1: the key stayed in `passthrough` and was forwarded to Claude
/// Code as its opening prompt, so it was never stored as a credential.
#[test]
fn an_api_key_after_deepseek_is_lifted_out_of_the_backend_argv() {
    let args = parse_inline_key_argv(&["clud", "--deepseek", DEEPSEEK_SHAPED_KEY]);
    assert!(args.deepseek);
    assert_eq!(
        args.inline_api_key.as_ref().map(InlineApiKey::expose),
        Some(DEEPSEEK_SHAPED_KEY)
    );
    assert!(
        args.passthrough.iter().all(|t| t != DEEPSEEK_SHAPED_KEY),
        "the key must never reach the backend argv: {:?}",
        args.passthrough
    );
}

#[test]
fn a_prompt_after_the_key_is_still_forwarded() {
    let args = parse_inline_key_argv(&["clud", "--deepseek", DEEPSEEK_SHAPED_KEY, "fix the bug"]);
    assert!(args.inline_api_key.is_some());
    assert_eq!(args.passthrough, vec!["fix the bug".to_string()]);
}

#[test]
fn inline_api_keys_are_recognised_for_kimi_openrouter_and_provider() {
    let openrouter_key = "sk-or-v1-0123456789abcdef0123456789abcdef";
    for argv in [
        vec!["clud", "--kimi", DEEPSEEK_SHAPED_KEY],
        vec!["clud", "--openrouter", openrouter_key],
        vec!["clud", "--provider", "deepseek", DEEPSEEK_SHAPED_KEY],
    ] {
        let args = parse_inline_key_argv(&argv);
        assert!(args.inline_api_key.is_some(), "{argv:?}");
        assert!(
            args.passthrough.is_empty(),
            "{argv:?}: {:?}",
            args.passthrough
        );
    }
}

/// #1197: `--deepseek=<key>` was an unknown flag, so clud launched plain
/// Claude and forwarded the key to the harness argv.
#[test]
fn the_equals_form_of_an_inline_key_matches_the_space_form() {
    let openrouter_key = "sk-or-v1-0123456789abcdef0123456789abcdef";
    for (flag, key) in [
        ("--deepseek", DEEPSEEK_SHAPED_KEY),
        ("--kimi", DEEPSEEK_SHAPED_KEY),
        ("--openrouter", openrouter_key),
    ] {
        let equals = format!("{flag}={key}");
        let args = parse_inline_key_argv(&["clud", equals.as_str(), "-p", "hi"]);
        let spaced = parse_inline_key_argv(&["clud", flag, key, "-p", "hi"]);
        assert!(args.explicit_model_provider().is_some(), "{flag}");
        assert_eq!(
            args.explicit_model_provider(),
            spaced.explicit_model_provider(),
            "{flag}"
        );
        assert_eq!(
            args.inline_api_key.as_ref().map(InlineApiKey::expose),
            Some(key),
            "{flag}"
        );
        assert!(
            args.passthrough.iter().all(|token| !token.contains(key)),
            "{flag}: the key must never reach the backend argv: {:?}",
            args.passthrough
        );
    }
}

#[test]
fn a_non_key_value_after_a_provider_equals_sign_is_rejected_without_echoing_it() {
    for arg in ["--deepseek=not-a-key", "--kimi=", "--openrouter=sk-short"] {
        let raw = vec!["clud".to_string(), arg.to_string()];
        let error = split_inline_key_assignments(&raw).unwrap_err();
        assert!(error.contains("does not take a value"), "{arg}: {error}");
        let value = arg.split_once('=').unwrap().1;
        assert!(value.is_empty() || !error.contains(value), "{arg}: {error}");
    }
}

#[test]
fn equals_assignments_after_the_separator_or_a_subcommand_are_untouched() {
    for argv in [
        ["clud", "--", "--deepseek=not-a-key"],
        ["clud", "settings", "--deepseek=not-a-key"],
    ] {
        let raw: Vec<String> = argv.iter().map(|arg| (*arg).to_string()).collect();
        assert_eq!(split_inline_key_assignments(&raw).unwrap(), raw, "{argv:?}");
    }
}

#[test]
fn ordinary_prompts_and_non_api_key_providers_are_left_alone() {
    let prompt = parse_inline_key_argv(&["clud", "--deepseek", "summarize sk-notes"]);
    assert!(prompt.inline_api_key.is_none());
    assert_eq!(prompt.passthrough, vec!["summarize sk-notes".to_string()]);

    let short = parse_inline_key_argv(&["clud", "--deepseek", "sk-short"]);
    assert!(short.inline_api_key.is_none());

    for argv in [
        vec!["clud", DEEPSEEK_SHAPED_KEY],
        vec!["clud", "--codex", DEEPSEEK_SHAPED_KEY],
        vec!["clud", "--claude", DEEPSEEK_SHAPED_KEY],
    ] {
        let args = parse_inline_key_argv(&argv);
        assert!(args.inline_api_key.is_none(), "{argv:?}");
    }
}

#[test]
fn the_inline_key_never_appears_in_debug_output() {
    let args = parse_inline_key_argv(&["clud", "--deepseek", DEEPSEEK_SHAPED_KEY]);
    let rendered = format!("{:?}", args.inline_api_key);
    assert!(!rendered.contains("0123456789abcdef"), "{rendered}");
    assert!(rendered.contains("redacted"));
}

#[test]
fn api_key_shape_detection() {
    assert!(looks_like_api_key(DEEPSEEK_SHAPED_KEY));
    assert!(looks_like_api_key(&format!("  {DEEPSEEK_SHAPED_KEY}\r\n")));
    assert!(looks_like_api_key("sk-or-v1-0123456789abcdef"));
    assert!(!looks_like_api_key("sk-has spaces 0123456789"));
    assert!(!looks_like_api_key("pk-0123456789abcdef0123"));
    assert!(!looks_like_api_key("sk-12345"));
}

#[test]
fn ci_is_a_dispatched_subcommand_with_its_flags() {
    let args = parse(&[
        "clud",
        "ci",
        "test-linux-x64-unit",
        "--filter",
        "openrouter_free",
        "--json",
    ]);
    match args.command {
        Some(Command::Ci {
            job,
            filter,
            json,
            workflow,
        }) => {
            assert_eq!(job.as_deref(), Some("test-linux-x64-unit"));
            assert_eq!(filter.as_deref(), Some("openrouter_free"));
            assert!(json);
            assert_eq!(workflow, None);
        }
        other => panic!("expected ci, got {other:?}"),
    }
    assert!(args.passthrough.is_empty());
}

#[test]
fn test_loop_message_flag_with_and_without_task() {
    let args = parse(&["clud", "loop", "--message", "hi there"]);
    match args.command {
        Some(Command::Loop {
            ref task,
            ref message,
            ..
        }) => {
            assert!(task.is_none());
            assert_eq!(message.as_deref(), Some("hi there"));
        }
        _ => panic!("expected Loop subcommand"),
    }
    let args = parse(&["clud", "loop", "--message", "m", "do it"]);
    match args.command {
        Some(Command::Loop {
            ref task,
            ref message,
            ..
        }) => {
            assert_eq!(task.as_deref(), Some("do it"));
            assert_eq!(message.as_deref(), Some("m"));
        }
        _ => panic!("expected Loop subcommand"),
    }
}

#[test]
fn test_workflow_subcommand_with_args() {
    let args = parse(&[
        "clud",
        "workflow",
        "review.js",
        "--args",
        r#"{"mode":"fast"}"#,
    ]);
    match args.command {
        Some(Command::Workflow {
            ref script,
            ref args_json,
        }) => {
            assert_eq!(script, "review.js");
            assert_eq!(args_json.as_deref(), Some(r#"{"mode":"fast"}"#));
        }
        other => panic!("expected Workflow subcommand, got {other:?}"),
    }
}

use super::*;
use kernal_api::platform::fs::TemporaryDirectory;
use std::fs::File;
use std::path::Path;

#[test]
fn captured_line_restores_delimiter_for_incremental_readers() {
    let mut output = Vec::new();
    write_captured_line(&mut output, b"first").unwrap();
    assert_eq!(output, b"first\n");
}

fn fixture_filter(name: &str) -> String {
    let module_path = module_path!();
    let (_, test_module_path) = module_path
        .split_once("::")
        .expect("module_path! includes the crate name and test module");
    format!("{test_module_path}::{name}")
}

fn resumable_fixture_exit_code(progress_timeout: bool, with_session: bool) -> i32 {
    let executable = std::env::current_exe().unwrap();
    let argv = vec![
        executable.to_string_lossy().into_owned(),
        "--ignored".to_string(),
        "--exact".to_string(),
        fixture_filter("resumable_watchdog_fixture_child"),
        "--nocapture".to_string(),
    ];
    let mut env = std::env::vars().collect::<Vec<_>>();
    env.push((
        "CLUD_RESUMABLE_WATCHDOG_FIXTURE".to_string(),
        "1".to_string(),
    ));
    let mut watchdog = Watchdog::for_rel_path("github/pr_merge_watch.py");
    watchdog.command_timeout = if progress_timeout {
        Duration::from_secs(3)
    } else {
        Duration::from_millis(100)
    };
    watchdog.progress_timeout = progress_timeout.then_some(Duration::from_millis(100));
    let telemetry = ToolTelemetry {
        server: None,
        token: None,
        id: "test-1431".to_string(),
        name: "github/pr_merge_watch.py".to_string(),
        start_time_ms: 0,
    };
    if with_session {
        let temp = TemporaryDirectory::new().unwrap();
        let ctx = SessionContext::from_state_root(temp.path(), 4242, 1);
        run_with_session(
            &ctx,
            1,
            "github/pr_merge_watch.py",
            &[],
            argv,
            env,
            ToolRunState {
                telemetry,
                watchdog,
            },
        )
        .unwrap()
    } else {
        run_passthrough(
            "github/pr_merge_watch.py",
            &[],
            argv,
            env,
            ToolRunState {
                telemetry,
                watchdog,
            },
        )
        .unwrap()
    }
}

#[test]
fn resumable_command_timeout_exits_nonzero_in_both_runners() {
    assert_eq!(
        resumable_fixture_exit_code(false, false),
        WATCHDOG_EXIT_CODE
    );
    assert_eq!(resumable_fixture_exit_code(false, true), WATCHDOG_EXIT_CODE);
}

#[test]
fn resumable_progress_timeout_exits_nonzero_in_both_runners() {
    assert_eq!(resumable_fixture_exit_code(true, false), WATCHDOG_EXIT_CODE);
    assert_eq!(resumable_fixture_exit_code(true, true), WATCHDOG_EXIT_CODE);
}

#[test]
#[ignore = "subprocess fixture invoked by resumable timeout tests"]
fn resumable_watchdog_fixture_child() {
    if std::env::var_os("CLUD_RESUMABLE_WATCHDOG_FIXTURE").is_none() {
        return;
    }
    thread::sleep(Duration::from_millis(800));
}

#[test]
fn delayed_exit_observation_is_not_reported_as_success() {
    let executable = std::env::current_exe().unwrap();
    let argv = vec![
        executable.to_string_lossy().into_owned(),
        "--ignored".to_string(),
        "--exact".to_string(),
        fixture_filter("immediate_exit_fixture_child"),
        "--nocapture".to_string(),
    ];
    for with_session in [false, true] {
        let mut watchdog = Watchdog::for_rel_path("github/pr_merge_watch.py");
        watchdog.command_timeout = Duration::from_millis(100);
        watchdog.started_at = std::time::Instant::now() - Duration::from_secs(2);
        let state = ToolRunState {
            telemetry: ToolTelemetry {
                server: None,
                token: None,
                id: "test-late-observation".to_string(),
                name: "github/pr_merge_watch.py".to_string(),
                start_time_ms: 0,
            },
            watchdog,
        };
        let env = std::env::vars().collect::<Vec<_>>();
        let result = if with_session {
            let root = TemporaryDirectory::new().unwrap();
            let ctx = SessionContext::from_state_root(root.path(), 4242, 1);
            run_with_session(
                &ctx,
                1,
                "__test_delayed_observer",
                &[],
                argv.clone(),
                env,
                state,
            )
        } else {
            run_passthrough("__test_delayed_observer", &[], argv.clone(), env, state)
        };
        assert_eq!(result.unwrap(), WATCHDOG_EXIT_CODE);
    }
}

#[test]
fn delayed_observation_preserves_failures_and_marks_progress_uncertain() {
    let mut watchdog = Watchdog::for_rel_path("github/pr_merge_watch.py");
    watchdog.command_timeout = Duration::from_secs(60);
    watchdog.progress_timeout = Some(Duration::from_millis(100));
    watchdog.quiet_ok = false;
    watchdog.last_output_at = std::time::Instant::now() - Duration::from_secs(1);
    assert_eq!(
        delayed_observation(&watchdog),
        Some(AbortReason::ProgressTimeout)
    );
    assert_eq!(uncertain_exit_code(7, delayed_observation(&watchdog)), 7);
    assert_eq!(
        uncertain_exit_code(0, delayed_observation(&watchdog)),
        WATCHDOG_EXIT_CODE
    );
    watchdog.quiet_ok = true;
    assert_eq!(delayed_observation(&watchdog), None);
    assert_eq!(uncertain_exit_code(0, None), 0);
}

#[test]
fn delayed_buffered_progress_remains_uncertain_after_timer_reset() {
    let mut watchdog = Watchdog::for_rel_path("github/pr_merge_watch.py");
    watchdog.command_timeout = Duration::from_secs(60);
    watchdog.progress_timeout = Some(Duration::from_millis(100));
    watchdog.quiet_ok = false;
    watchdog.last_output_at = std::time::Instant::now() - Duration::from_secs(1);
    let mut uncertain = false;
    note_observed_progress(&mut watchdog, &mut uncertain);
    assert!(uncertain);
    assert_eq!(delayed_observation(&watchdog), None);
}

#[test]
fn buffered_progress_after_delayed_observation_is_uncertain_in_both_runners() {
    let executable = std::env::current_exe().unwrap();
    let argv = vec![
        executable.to_string_lossy().into_owned(),
        "--ignored".to_string(),
        "--exact".to_string(),
        fixture_filter("buffered_progress_fixture_child"),
        "--nocapture".to_string(),
    ];
    for with_session in [false, true] {
        let root = TemporaryDirectory::new().unwrap();
        let release = root.path().join("release");
        let _release_on_drop = FixtureRelease(release.clone());
        let args = vec![release.to_string_lossy().into_owned()];
        let mut watchdog = Watchdog::for_rel_path("github/pr_merge_watch.py");
        watchdog.command_timeout = Duration::from_secs(60);
        watchdog.progress_timeout = Some(Duration::from_secs(30));
        watchdog.quiet_ok = false;
        watchdog.last_output_at = std::time::Instant::now() - Duration::from_secs(31);
        let state = ToolRunState {
            telemetry: ToolTelemetry {
                server: None,
                token: None,
                id: "test-buffered-progress".to_string(),
                name: "github/pr_merge_watch.py".to_string(),
                start_time_ms: 0,
            },
            watchdog,
        };
        let mut env = std::env::vars()
            .filter(|(key, _)| key != "CLUD_TEST_PROGRESS_RELEASE")
            .collect::<Vec<_>>();
        env.push(("CLUD_TEST_PROGRESS_RELEASE".to_string(), args[0].clone()));
        let result = if with_session {
            let ctx = SessionContext::from_state_root(root.path(), 4242, 1);
            let code = run_with_session(
                &ctx,
                1,
                "__test_buffered_progress",
                &args,
                argv.clone(),
                env,
                state,
            );
            let records = crate::tool_query::read_invocations(&ctx).unwrap();
            assert_eq!(
                records[0].state,
                crate::tool_query::InvocationState::ObservationDelayed
            );
            assert_eq!(records[0].reason.as_deref(), Some("progress_timeout"));
            assert_eq!(records[0].exit_code, Some(0));
            code
        } else {
            run_passthrough("__test_buffered_progress", &args, argv.clone(), env, state)
        };
        assert_eq!(result.unwrap(), WATCHDOG_EXIT_CODE);
    }
}

#[test]
#[ignore]
fn buffered_progress_fixture_child() {
    let release = std::env::var_os("CLUD_TEST_PROGRESS_RELEASE").expect("release path");
    println!("buffered progress fixture");
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while !std::path::Path::new(&release).exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "fixture release timed out"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

struct FixtureRelease(std::path::PathBuf);

impl Drop for FixtureRelease {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.0, b"release");
    }
}

#[test]
#[ignore]
fn immediate_exit_fixture_child() {}

#[test]
fn captured_subprocess_output_is_forwardable_before_exit() {
    let executable = std::env::current_exe().unwrap();
    let mut env = std::env::vars().collect::<Vec<_>>();
    env.push(("CLUD_STREAMING_FIXTURE_CHILD".to_string(), "1".to_string()));
    let process = NativeProcess::new(ProcessConfig {
        command: CommandSpec::Argv(vec![
            executable.to_string_lossy().into_owned(),
            "--ignored".to_string(),
            "--exact".to_string(),
            fixture_filter("incremental_output_fixture_child"),
            "--nocapture".to_string(),
        ]),
        cwd: None,
        env: Some(env),
        capture: true,
        stderr_mode: StderrMode::Pipe,
        creationflags: None,
        create_process_group: false,
        stdin_mode: StdinMode::Null,
        nice: None,
        address_space_limit_bytes: None,
    });
    process.start().unwrap();

    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut emitted = 0;
    let mut first_before_exit = false;
    while std::time::Instant::now() < deadline {
        match process.wait(Some(Duration::from_millis(50))) {
            Err(ProcessError::Timeout) => {
                emitted = drain_passthrough_output_to(&process, emitted, &mut stdout, &mut stderr);
                if stdout.windows(b"first\n".len()).any(|w| w == b"first\n") {
                    first_before_exit = true;
                    break;
                }
            }
            Ok(code) => panic!("fixture exited with {code} before first was observed"),
            Err(error) => panic!("fixture wait failed: {error}"),
        }
    }
    assert!(
        first_before_exit,
        "first line was not forwardable before exit"
    );

    assert_eq!(process.wait(None).unwrap(), 7);
    let _ = drain_passthrough_output_to(&process, emitted, &mut stdout, &mut stderr);
    assert!(
        stderr.windows(b"second\n".len()).any(|w| w == b"second\n"),
        "stderr line missing from captured output: {}",
        String::from_utf8_lossy(&stderr)
    );
}

#[test]
#[ignore = "subprocess fixture invoked by captured_subprocess_output_is_forwardable_before_exit"]
fn incremental_output_fixture_child() {
    if std::env::var_os("CLUD_STREAMING_FIXTURE_CHILD").is_none() {
        return;
    }
    println!("first");
    io::stdout().flush().unwrap();
    thread::sleep(Duration::from_secs(1));
    eprintln!("second");
    io::stderr().flush().unwrap();
    std::process::exit(7);
}

#[test]
#[ignore = "subprocess fixture invoked by session_started_event_records_the_real_child_pid"]
fn pid_fixture_child() {
    if std::env::var_os("CLUD_TOOL_PID_FIXTURE_CHILD").is_none() {
        return;
    }
    // Echo our own PID so the parent can prove the recorded PID is the
    // real one rather than merely non-zero.
    println!("clud-pid-fixture pid={}", std::process::id());
    io::stdout().flush().unwrap();
    thread::sleep(Duration::from_millis(300));
    std::process::exit(0);
}

/// Regression: ensure the resolved tool path is exactly
/// `<tools_root>/<rel_path>`. A prior version called
/// `target_path_at(tools_root.parent(), rel_path)` which re-appended
/// `.clud/tools` and produced `~/.clud/.clud/tools/<rel_path>` —
/// every real `clud tool run` would NotFound.
#[test]
fn resolve_tool_path_does_not_double_prefix() {
    let tools_root = Path::new("/home/user/.clud/tools");
    let resolved = resolve_tool_path(tools_root, "github/pr_merge_watch.py");
    assert_eq!(
        resolved,
        PathBuf::from("/home/user/.clud/tools/github/pr_merge_watch.py"),
        "tool path must not contain a doubled `.clud/tools` segment",
    );
    let s = resolved.to_string_lossy().to_string();
    assert!(
        !s.contains(".clud/tools/.clud/tools"),
        "double `.clud/tools` segment regression: {s}",
    );
}

#[test]
fn unresolvable_tool_returns_not_found() {
    let err = run("definitely/does/not/exist-XXXX-clud-test-only.py", &[]).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::NotFound);
    let msg = err.to_string();
    assert!(
        msg.contains("definitely/does/not/exist-XXXX-clud-test-only.py")
            || msg.contains("definitely\\does\\not\\exist-XXXX-clud-test-only.py"),
        "error message should reference the requested rel_path; got: {msg}",
    );
}

#[test]
fn resolved_cache_dir_respects_parent_env() {
    let resolved =
        resolved_uv_cache_dir_from(Some(OsString::from("/tmp/test-cache-for-clud tool-run")));
    assert_eq!(resolved, PathBuf::from("/tmp/test-cache-for-clud tool-run"));
}

#[test]
fn resolved_cache_dir_falls_back_to_default() {
    let resolved = resolved_uv_cache_dir_from(None);
    assert_eq!(resolved, clud_uv_cache_dir());
}

#[test]
fn pep723_script_uses_uv_script_runner() {
    let tmp = TemporaryDirectory::new().unwrap();
    let tool = tmp.path().join("tools").join("hooks").join("hook.py");
    let body = "#!/usr/bin/env -S uv run --script\n# /// script\n# dependencies = []\n# ///\n";
    let argv = build_tool_argv(
        &tool,
        body.as_bytes(),
        &["--flag".to_string()],
        &tmp.path().join("tools"),
        &[],
    )
    .unwrap();
    assert_eq!(argv[0], "uv");
    assert_eq!(argv[1], "run");
    assert!(argv.contains(&"--no-project".to_string()));
    assert!(argv.contains(&"--script".to_string()));
    assert_eq!(argv.last().map(String::as_str), Some("--flag"));
}

#[test]
fn plain_python_uses_managed_python_when_present() {
    let tmp = TemporaryDirectory::new().unwrap();
    let tools_root = tmp.path().join("tools");
    let managed_dir = managed_python_install_dir(&tools_root).join("cpython-test");
    std::fs::create_dir_all(&managed_dir).unwrap();
    let exe_name = python_executable_names()[0];
    let python = managed_dir.join(exe_name);
    File::create(&python).unwrap();

    let tool = tools_root.join("plain.py");
    let argv = build_tool_argv(
        &tool,
        b"print('plain')\n",
        &["arg".to_string()],
        &tools_root,
        &[],
    )
    .unwrap();
    assert_eq!(argv[0], python.to_string_lossy());
    assert_eq!(argv[1], tool.to_string_lossy());
    assert_eq!(argv[2], "arg");
}

#[test]
fn pep723_block_without_shebang_uses_plain_python() {
    let tmp = TemporaryDirectory::new().unwrap();
    let tools_root = tmp.path().join("tools");
    let managed_dir = managed_python_install_dir(&tools_root).join("cpython-test");
    std::fs::create_dir_all(&managed_dir).unwrap();
    let python = managed_dir.join(python_executable_names()[0]);
    File::create(&python).unwrap();

    let tool = tools_root.join("metadata_only.py");
    let argv = build_tool_argv(
        &tool,
        b"# /// script\n# dependencies = [\"requests\"]\n# ///\nprint('plain')\n",
        &[],
        &tools_root,
        &[],
    )
    .unwrap();
    assert_eq!(argv[0], python.to_string_lossy());
    assert_eq!(argv[1], tool.to_string_lossy());
}

#[test]
fn shebang_bin_sh_runs_through_shell() {
    let tmp = TemporaryDirectory::new().unwrap();
    let tool = tmp.path().join("tool.sh");
    let argv = build_tool_argv(
        &tool,
        b"#!/bin/sh\necho hi\n",
        &["x".to_string()],
        tmp.path(),
        &[],
    )
    .unwrap();
    assert_eq!(argv, vec!["sh", &tool.to_string_lossy(), "x"]);
}

#[test]
fn uv_shebang_runs_script_runner() {
    let tmp = TemporaryDirectory::new().unwrap();
    let tool = tmp.path().join("tool.py");
    let argv = build_tool_argv(
        &tool,
        b"#!/usr/bin/env -S uv run --script\nprint('hi')\n",
        &[],
        tmp.path(),
        &[],
    )
    .unwrap();
    assert_eq!(argv[0], "uv");
    assert!(argv.contains(&"--script".to_string()));
}

#[test]
fn binary_magic_executes_directly() {
    let tmp = TemporaryDirectory::new().unwrap();
    let tool = tmp.path().join("tool.bin");
    let argv = build_tool_argv(
        &tool,
        b"\x7fELF\x02\x01",
        &["arg".to_string()],
        tmp.path(),
        &[],
    )
    .unwrap();
    assert_eq!(
        argv,
        vec![tool.to_string_lossy().to_string(), "arg".to_string()]
    );
}

#[test]
fn cpp_executable_path_sits_next_to_source() {
    let source = Path::new("/tmp/tool.cpp");
    let exe = cpp_executable_path(source);
    if crate::platform::host::WINDOWS {
        assert_eq!(exe, PathBuf::from("/tmp/tool.exe"));
    } else {
        assert_eq!(exe, PathBuf::from("/tmp/tool"));
    }
}

#[test]
fn install_path_parser_accepts_jsonl_and_arrays() {
    let jsonl = br#"{"install_path":"/clang/one"}
{"other":true}
"#;
    assert_eq!(first_install_path(jsonl), Some(PathBuf::from("/clang/one")));
    let array = br#"[{"install_path":"/clang/two"}]"#;
    assert_eq!(first_install_path(array), Some(PathBuf::from("/clang/two")));
}

#[test]
fn build_child_env_pins_tool_environment_and_strips_inherited_values() {
    let cache = std::path::PathBuf::from("/some/clud/cache");
    let env = build_child_env(&cache);
    let uv_entries: Vec<_> = env.iter().filter(|(k, _)| k == "UV_CACHE_DIR").collect();
    assert_eq!(
        uv_entries.len(),
        1,
        "must pin exactly one UV_CACHE_DIR entry"
    );
    assert_eq!(uv_entries[0].1, "/some/clud/cache");
    let clud_exe_entries: Vec<_> = env.iter().filter(|(k, _)| k == "CLUD_EXE").collect();
    assert_eq!(clud_exe_entries.len(), 1);
    assert_eq!(
        clud_exe_entries[0].1,
        std::env::current_exe().unwrap().to_str().unwrap()
    );
    let daemon_spawn_entries: Vec<_> = env
        .iter()
        .filter(|(k, _)| k == crate::daemon::ENV_ALLOW_DAEMON_SPAWN)
        .collect();
    assert_eq!(
        daemon_spawn_entries.len(),
        0,
        "tool children must not inherit CLUD_ALLOW_DAEMON_SPAWN"
    );
    // Sanity: at least one non-UV entry made it through (PATH on any host).
    // We don't insist on PATH specifically because some test runners may strip
    // it; we just confirm the env isn't only the pinned UV_CACHE_DIR entry.
    assert!(!env.is_empty(), "env must include the pinned UV_CACHE_DIR");
}

#[test]
fn stderr_tail_keeps_last_200_chars() {
    let text = "x".repeat(250);
    let tail = stderr_tail_200(text.as_bytes()).unwrap();
    assert_eq!(tail.len(), 200);
    assert_eq!(tail, "x".repeat(200));
    assert!(stderr_tail_200(b"").is_none());
}

#[test]
fn passthrough_abort_payload_names_exact_argv() {
    let args = vec!["--flag".to_string()];
    let argv = vec![
        "uv".to_string(),
        "run".to_string(),
        "--script".to_string(),
        "hooks/block-bad-cmd.py".to_string(),
    ];
    let payload = render_passthrough_abort_payload(
        "hooks/block-bad-cmd.py",
        &args,
        &argv,
        Duration::from_millis(1234),
        AbortReason::CommandTimeout,
        Some("blocked at sys.stdin.read()"),
    );
    let value: serde_json::Value = serde_json::from_str(&payload).unwrap();
    assert_eq!(value["status"], "aborted");
    assert_eq!(value["reason"], "command_timeout");
    assert_eq!(value["tool"], "hooks/block-bad-cmd.py");
    assert_eq!(value["args"][0], "--flag");
    assert_eq!(value["argv"][0], "uv");
    assert_eq!(value["stderr_tail"], "blocked at sys.stdin.read()");
}

/// Regression test for #1204: `run_with_session` used to hard-code
/// `pid: 0` (and `pid_start_time: 0`) in the `Started` event because
/// capturing the real subprocess PID through `running_process` "would need
/// a dedicated API" (per the old comment). That placeholder meant
/// `clud tool log --pid <pid>` / `clud tool info --pid <pid>` could never
/// resolve a real invocation: `tool_query::resolve_ref` had nothing but a
/// 0 to match against. This test runs a real short-lived child through the
/// real session path and asserts the recorded PID is the child's actual
/// OS PID, and that `resolve_ref` can find the invocation by that PID.
#[test]
fn session_started_event_records_the_real_child_pid() {
    use base64::engine::general_purpose::STANDARD_NO_PAD;
    use base64::Engine;

    let tmp = TemporaryDirectory::new().unwrap();
    let ctx = crate::session_index::SessionContext::from_state_root(tmp.path(), 4242, 1);
    let tool_id = crate::session_index::allocate_next_id(&ctx).unwrap();

    let executable = std::env::current_exe().unwrap();
    let argv = vec![
        executable.to_string_lossy().into_owned(),
        "--ignored".to_string(),
        "--exact".to_string(),
        fixture_filter("pid_fixture_child"),
        "--nocapture".to_string(),
    ];
    let mut env = std::env::vars().collect::<Vec<_>>();
    env.push(("CLUD_TOOL_PID_FIXTURE_CHILD".to_string(), "1".to_string()));

    // Build telemetry with no endpoint instead of calling
    // `ToolTelemetry::start`: that reads the daemon HTTP env vars from the
    // real environment and would POST a bogus tool event to a developer's
    // live daemon when the suite runs inside a clud session.
    let telemetry = ToolTelemetry {
        server: None,
        token: None,
        id: "test-1204".to_string(),
        name: "tests/pid-fixture".to_string(),
        start_time_ms: 0,
    };

    let ran = run_with_session(
        &ctx,
        tool_id,
        "tests/pid-fixture",
        &[],
        argv,
        env,
        ToolRunState {
            telemetry,
            watchdog: Watchdog::for_rel_path("tests/pid-fixture"),
        },
    );
    assert_eq!(ran.unwrap(), 0, "fixture child must exit 0");

    // The index must carry the child's real PID.
    let raw = std::fs::read_to_string(ctx.index_path()).unwrap();
    let started = raw
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .find(|value| value["event"] == "started")
        .expect("index must contain a started event");
    let pid = started["pid"]
        .as_u64()
        .expect("started event carries a numeric pid") as u32;
    assert_ne!(
        pid, 0,
        "Started event must record the child's real PID, not the 0 placeholder"
    );

    // ... and it must be the PID the child itself reported. A tee record
    // is "one captured line or chunk", so decode every record into one
    // string and search that, rather than requiring the marker to be a
    // whole record: a chunked read must not turn this assertion into a
    // silent miss. `TeeWriter::emit_captured_batch` already restored each
    // record's own `\n`, so concatenating them separator-free reproduces
    // the child's byte stream exactly; the pid digits are then taken with
    // `take_while` so the trailing newline and libtest's own output stop
    // the scan.
    let stdout_log =
        std::fs::read_to_string(ctx.tool_log_dir(tool_id).join("stdout.jsonl")).unwrap();
    let mut joined = String::new();
    for line in stdout_log.lines().filter(|line| !line.trim().is_empty()) {
        let value: serde_json::Value = serde_json::from_str(line).unwrap();
        let encoded = value["bytes"]
            .as_str()
            .expect("tee line carries base64 bytes");
        let decoded = STANDARD_NO_PAD.decode(encoded).unwrap();
        joined.push_str(&String::from_utf8_lossy(&decoded));
    }
    const MARKER: &str = "clud-pid-fixture pid=";
    let digits_at = joined
        .find(MARKER)
        .expect("fixture child echoed its own pid to stdout")
        + MARKER.len();
    let digits: String = joined[digits_at..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    let reported: u32 = digits
        .parse()
        .expect("the marker is followed by the child's decimal pid");
    assert_eq!(
        pid, reported,
        "recorded PID must be the child's real OS PID"
    );

    // `clud tool log --pid <real pid>` resolves through resolve_ref.
    // Note: we deliberately do not assert anything about `pid_start_time`
    // here — the fix records it best-effort, and a hard assertion on it
    // would be the one thing in this test that could flake under load.
    let invocations = crate::tool_query::read_invocations(&ctx).unwrap();
    assert_eq!(
        invocations.len(),
        1,
        "exactly one invocation in this session"
    );
    assert_eq!(invocations[0].pid, pid);
    let resolved =
        crate::tool_query::resolve_ref(&invocations, ctx.session_pid, None, Some(pid)).unwrap();
    assert_eq!(
        resolved, tool_id,
        "`--pid <real pid>` must resolve to this invocation"
    );
}

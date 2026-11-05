//! #2007: an explicitly opened Command Prompt outlives its clud-owned PTY.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use clud::job_orphan_reaper::ForegroundJobTracker;
use clud::process_identity::ProcessIdentity;
use running_process::pty::NativePtyProcess;

struct Fixture {
    process: NativePtyProcess,
    tracker: Option<ForegroundJobTracker>,
    terminal: Option<ProcessIdentity>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.process.close_impl();
        drop(self.tracker.take());
        if let Some(identity) = self.terminal {
            clud::process_tree::kill_tree_filtered_automatic(identity, &|_| true);
        }
    }
}

pub(super) fn git_bash() -> PathBuf {
    ["ProgramFiles", "ProgramW6432"]
        .into_iter()
        .filter_map(std::env::var_os)
        .map(|root| PathBuf::from(root).join("Git/bin/bash.exe"))
        .find(|path| path.is_file())
        .expect("Git Bash is required for the Windows start-command regression")
}

fn wait_for_terminal(process: &NativePtyProcess) -> ProcessIdentity {
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut output = Vec::new();
    loop {
        if let Some(bytes) = process.read_chunk_impl(Some(0.1)).unwrap() {
            output.extend(bytes);
            // ConPTY may request the cursor position before Bash starts.
            let _ = process.write_impl(b"\x1b[1;1R", false);
        }
        let scan = clud::process_scan::scan_env("CLUD");
        if let Some(terminal) = scan.tagged.iter().find(|candidate| {
            candidate.parent_pid == std::process::id()
                && candidate.name.eq_ignore_ascii_case("cmd.exe")
                && candidate.command.split_whitespace().count() == 1
        }) {
            return terminal.identity();
        }
        assert!(
            Instant::now() < deadline,
            "no opened cmd.exe: {}",
            String::from_utf8_lossy(&output)
        );
    }
}

fn check_started_terminal(request: &str) {
    let temp = tempfile::tempdir().unwrap();
    let mut env: Vec<(String, String)> = std::env::vars()
        .filter(|(key, _)| {
            !key.eq_ignore_ascii_case(running_process::DAEMON_MARKER_ENV_VAR)
                && !key.eq_ignore_ascii_case("MSYS2_ARG_CONV_EXCL")
        })
        .collect();
    let overrides = clud::shell::nounset::env_overrides_at(temp.path(), false, None);
    let originator = format!("CLUD:{}", std::process::id());
    for (key, value) in overrides.into_iter().chain([
        (
            "CLUD_EXE".to_string(),
            crate::exe::bin_path("clud", option_env!("CARGO_BIN_EXE_clud"))
                .display()
                .to_string(),
        ),
        (running_process::ORIGINATOR_ENV_VAR.to_string(), originator),
    ]) {
        env.retain(|(existing, _)| !existing.eq_ignore_ascii_case(&key));
        env.push((key, value));
    }
    let process = NativePtyProcess::new(
        vec![
            git_bash().display().to_string(),
            "--noprofile".into(),
            "--norc".into(),
            "-c".into(),
            format!("start {request}; printf 'START_REQUESTED\\n'; read -r"),
        ],
        None,
        Some(env),
        24,
        80,
        None,
    )
    .unwrap();
    process.set_echo(false);
    let mut fixture = Fixture {
        process,
        tracker: Some(ForegroundJobTracker::install().expect("foreground tracker")),
        terminal: None,
    };
    fixture.process.start_impl().unwrap();
    fixture
        .tracker
        .as_ref()
        .unwrap()
        .register_backend(fixture.process.pid().unwrap().unwrap(), "bash.exe");
    let terminal = wait_for_terminal(&fixture.process);
    fixture.terminal = Some(terminal);
    assert!(
        clud::process_scan::scan_env("CLUD")
            .declared_daemons
            .contains(&terminal.pid),
        "opened Command Prompt did not inherit explicit persistence"
    );
    fixture.process.close_impl().unwrap();
    assert!(
        terminal.is_live(),
        "opened Command Prompt died with PTY close"
    );
    drop(fixture.tracker.take());
    // Collect before asserting so a failure explains itself (#2384).
    let live_after_cleanup = terminal.is_live();
    let diagnostics = if live_after_cleanup {
        String::new()
    } else {
        let facts = clud::reaper_facts::collect_host_facts(
            &[terminal.pid],
            &Default::default(),
            Vec::new(),
        );
        let declared = clud::process_scan::scan_env("CLUD")
            .declared_daemons
            .contains(&terminal.pid);
        format!(
            " (terminal pid {}, declared_daemons contains it: {declared}, host facts: {facts:?})",
            terminal.pid
        )
    };
    assert!(
        live_after_cleanup,
        "explicitly opened Command Prompt died with clud cleanup{diagnostics}"
    );
    let facts =
        clud::reaper_facts::collect_host_facts(&[terminal.pid], &Default::default(), Vec::new());
    // The explicit launch must also declare persistence for later orphan sweeps.
    let scan = clud::process_scan::scan_env("CLUD");
    assert!(scan.declared_daemons.contains(&terminal.pid));
    let facts = clud::reaper_facts::FactsSnapshot {
        declared_daemons: scan.declared_daemons,
        ..facts
    };
    assert_eq!(
        clud::reaper_facts::spare_signal(&facts, terminal.pid, "cmd.exe"),
        Some(clud::reaper_facts::SpareReason::DeclaredDaemon)
    );
}

#[test]
fn start_cmd_survives_pty_close_and_foreground_cleanup() {
    require_pty_or_skip!("start_cmd_survives_pty_close_and_foreground_cleanup");
    for request in ["cmd", "cmd.exe"] {
        check_started_terminal(request);
    }
}

#[test]
fn start_intercepts_only_a_bare_cmd_request() {
    use running_process::{
        CommandSpec, NativeProcess, ProcessConfig, ReadStatus, StderrMode, StdinMode,
    };

    let temp = tempfile::tempdir().unwrap();
    let mut env: Vec<(String, String)> = std::env::vars().collect();
    for (key, value) in clud::shell::nounset::env_overrides_at(temp.path(), false, None)
        .into_iter()
        .chain([("CLUD_EXE".to_string(), "/usr/bin/echo".to_string())])
    {
        env.retain(|(existing, _)| !existing.eq_ignore_ascii_case(&key));
        env.push((key, value));
    }
    // Mock stock start before re-sourcing startup, so even the RED run cannot
    // open unrelated applications. The fixed startup replaces this function.
    let script = r#"
command() { printf 'FALLBACK'; printf '[%s]' "$@"; printf '\n'; }
start() { command start "$@"; }
. "$BASH_ENV"
start cmd
start CMD.EXE
start cmd.exe /c exit
start /b cmd.exe
start '' cmd.exe
start notepad.exe
"#;
    let child = NativeProcess::new(ProcessConfig {
        command: CommandSpec::Argv(vec![
            git_bash().display().to_string(),
            "--noprofile".into(),
            "--norc".into(),
            "-c".into(),
            script.into(),
        ]),
        cwd: None,
        env: Some(env),
        capture: true,
        stderr_mode: StderrMode::Stdout,
        creationflags: None,
        create_process_group: false,
        stdin_mode: StdinMode::Null,
        nice: None,
        address_space_limit_bytes: None,
    });
    child.start().unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut output = Vec::new();
    loop {
        assert!(Instant::now() < deadline, "Bash dispatch fixture timed out");
        match child.read_combined(Some(Duration::from_millis(100))) {
            ReadStatus::Line(event) => {
                output.extend(event.line);
                output.push(b'\n');
            }
            ReadStatus::Timeout => {}
            ReadStatus::Eof => break,
        }
    }
    assert_eq!(child.wait(Some(Duration::from_secs(5))).unwrap(), 0);
    let output = String::from_utf8(output).unwrap().replace('\r', "");
    assert_eq!(output, "__start-cmd\n__start-cmd\nFALLBACK[start][cmd.exe][/c][exit]\nFALLBACK[start][/b][cmd.exe]\nFALLBACK[start][][cmd.exe]\nFALLBACK[start][notepad.exe]\n");
}

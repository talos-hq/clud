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
    let log_start = reap_log_path()
        .ok()
        .and_then(|p| std::fs::metadata(p).ok())
        .map_or(0, |metadata| metadata.len());
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
    let selected_metadata = terminal_metadata(terminal.pid);
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
    assert_survives_cleanup(&mut fixture, terminal, selected_metadata, log_start);
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

// Observe the existing facts and decision log; never duplicate the spare table.
fn assert_survives_cleanup(
    fixture: &mut Fixture,
    terminal: ProcessIdentity,
    selected_metadata: String,
    log_start: u64,
) {
    let scan = clud::process_scan::scan_env("CLUD");
    let facts = clud::reaper_facts::collect_host_facts(
        &[terminal.pid],
        &scan.declared_daemons,
        clud::reaper_facts::configured_spare_images_from_env(),
    );
    let metadata = terminal_metadata(terminal.pid);
    // Sample last, immediately before drop, to distinguish an already-exited
    // start handoff from a process that disappears during foreground cleanup.
    let live_before_drop = terminal.is_live();
    drop(fixture.tracker.take());
    let live_after_cleanup = terminal.is_live();
    assert!(
        live_after_cleanup,
        "explicitly opened Command Prompt died with clud cleanup: \
         terminal={terminal:?}, selected_metadata={selected_metadata}, \
         metadata_before_drop={metadata}, \
         live_before_drop={live_before_drop}, host_facts_before_drop={facts:?}, \
         reaper_events={}",
        terminal_reap_events(terminal.pid, log_start)
    );
}

fn terminal_metadata(pid: u32) -> String {
    let system = sysinfo::System::new_with_specifics(
        sysinfo::RefreshKind::nothing().with_processes(sysinfo::ProcessRefreshKind::nothing()),
    );
    let metadata = system
        .process(sysinfo::Pid::from_u32(pid))
        .map(|p| (p.parent().map(|pid| pid.as_u32()), p.name().to_os_string()));
    format!("(parent_pid, image)={metadata:?}")
}

fn reap_log_path() -> std::io::Result<PathBuf> {
    Ok(clud::reap_log::session_reap_log_path(
        &clud::daemon::default_state_dir()?,
        std::process::id(),
        clud::process_identity::self_start_time(),
    ))
}

fn terminal_reap_events(pid: u32, log_start: u64) -> String {
    let path = match reap_log_path() {
        Ok(path) => path,
        Err(error) => return format!("state directory unavailable: {error}"),
    };
    // Tracker drop joins the listener and drops ReapLog, flushing its events.
    // Include live and exit phases: a live decision can also explain a death.
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) => return format!("{}: {error}", path.display()),
    };
    let mut matching = Vec::new();
    let mut errors = Vec::new();
    let offset = usize::try_from(log_start).unwrap_or(usize::MAX);
    let Some(content) = content.get(offset..) else {
        return format!("{}: log truncated since tracker install", path.display());
    };
    for line in content.lines() {
        match serde_json::from_str::<serde_json::Value>(line) {
            Ok(event) if event["pid"].as_u64() == Some(u64::from(pid)) => matching.push(event),
            Ok(_) => {}
            Err(error) => errors.push(error.to_string()),
        }
    }
    format!(
        "{}: {matching:?} (empty means no recorded decision); parse_errors={errors:?}",
        path.display()
    )
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

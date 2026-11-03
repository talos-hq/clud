//! Real terminal acceptance through the launching shell, including ConPTY.
use std::path::Path;
use std::time::{Duration, Instant};

use running_process::pty::NativePtyProcess;

fn launch(shell: &str, binary: &Path) -> Vec<String> {
    let binary = binary.to_string_lossy();
    let args = "--no-daemon --claude --subprocess loop --loop-count 1 test-dashboard -- --mock-sleep-ms 60000";
    match shell {
        "cmd" => vec![
            "cmd.exe".into(),
            "/d".into(),
            "/s".into(),
            "/c".into(),
            format!("\"\"{binary}\" {args}\""),
        ],
        "powershell" => vec![
            "powershell.exe".into(),
            "-NoLogo".into(),
            "-NoProfile".into(),
            "-Command".into(),
            format!(
                "& '{}' {args}; exit $LASTEXITCODE",
                binary.replace('\'', "''")
            ),
        ],
        _ => vec![
            "bash".into(),
            "--noprofile".into(),
            "--norc".into(),
            "-c".into(),
            format!(
                "exec '{}' {args}",
                binary.replace('\\', "/").replace('\'', "'\\''")
            ),
        ],
    }
}

fn wait_text(process: &NativePtyProcess, text: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(45);
    let mut bytes = Vec::new();
    let mut answered = 0;
    while Instant::now() < deadline {
        match process.read_chunk_impl(Some(0.1)) {
            Ok(Some(chunk)) => bytes.extend_from_slice(&chunk),
            Ok(None) => {}
            Err(error) => panic!(
                "terminal closed waiting for {text}: {error}: {}",
                String::from_utf8_lossy(&bytes)
            ),
        }
        let queries = bytes.windows(4).filter(|w| *w == b"\x1b[6n").count();
        while answered < queries {
            process.write_impl(b"\x1b[1;1R", false).unwrap();
            answered += 1;
        }
        let output = String::from_utf8_lossy(&bytes);
        if output.contains(text) {
            return output.into_owned();
        }
    }
    panic!(
        "timed out waiting for {text}: {}",
        String::from_utf8_lossy(&bytes)
    );
}

fn exercise(shell: &str) {
    let temp = tempfile::Builder::new()
        .prefix("loop console spaces ")
        .tempdir()
        .unwrap();
    let root = temp.path();
    let home = root.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let ext = if clud::platform::host::WINDOWS {
        ".exe"
    } else {
        ""
    };
    let mock = crate::common::mock_agent_path();
    for name in ["claude", "codex"] {
        std::fs::copy(&mock, root.join(format!("{name}{ext}"))).unwrap();
    }
    let mut env: Vec<(String, String)> = std::env::vars()
        .filter(|(k, _)| {
            !matches!(
                k.as_str(),
                "HOME" | "USERPROFILE" | "PATH" | "CLUD_PROVIDER" | "CLUD_SUBSCRIPTION_PROVIDER"
            )
        })
        .collect();
    let path = std::env::join_paths(
        std::iter::once(root.to_owned()).chain(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        )),
    )
    .unwrap();
    env.extend([
        ("PATH".into(), path.to_string_lossy().into_owned()),
        ("HOME".into(), home.to_string_lossy().into_owned()),
        ("USERPROFILE".into(), home.to_string_lossy().into_owned()),
        ("CLUD_HOOK_HOME".into(), home.to_string_lossy().into_owned()),
        ("CLUD_NO_UNLOCK".into(), "1".into()),
        ("SOLDR_BROKER_AUTOSPAWN".into(), "0".into()),
        ("TERM".into(), "xterm-256color".into()),
    ]);
    let binary = crate::exe::bin_path("clud", option_env!("CARGO_BIN_EXE_clud"));
    let process = NativePtyProcess::new(
        launch(shell, &binary),
        Some(root.to_string_lossy().into_owned()),
        Some(env),
        24,
        140,
        None,
    )
    .unwrap();
    process.set_echo(false);
    process.start_impl().unwrap();
    wait_text(&process, "Interrupt with message");
    process.write_impl(b"q", false).unwrap();
    wait_text(&process, "Enter: save");
    process
        .write_impl(b"literal $HOME %PATH% & message\r", false)
        .unwrap();
    wait_text(&process, "Message queued for next iteration");
    let inbox = root.join(".clud/loop/inbox");
    assert_eq!(std::fs::read_dir(&inbox).unwrap().count(), 1);
    process.write_impl(b"i", false).unwrap();
    wait_text(&process, "Enter: review");
    process.write_impl(b"restart message\r", false).unwrap();
    wait_text(&process, "Are you sure?");
    // Enter is safe; cancellation never queues another message.
    process.write_impl(b"\r", false).unwrap();
    wait_text(&process, "Enter: review");
    assert_eq!(std::fs::read_dir(&inbox).unwrap().count(), 1);
    process.write_impl(b"\r", false).unwrap();
    wait_text(&process, "Are you sure?");
    process.write_impl(b"y", false).unwrap();
    wait_text(&process, "Interrupt with message");
    let delivered = std::fs::read_to_string(root.join(".clud/loop/NEXT_MESSAGE.md")).unwrap();
    assert!(delivered.contains("literal $HOME %PATH% & message"));
    assert!(delivered.contains("restart message"));
    // A one-iteration budget still permits restarting that same iteration.
    let info: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join(".clud/loop/info.json")).unwrap()).unwrap();
    assert_eq!(info["current_iteration"], 1);
    process.write_impl(b"\x03", false).unwrap();
    let output = wait_text(&process, "\x1b[?1049l");
    assert!(
        output.contains("\x1b[?25h"),
        "cursor was not restored: {output:?}"
    );
    assert_eq!(
        process.wait_impl(Some(10.0)).unwrap(),
        130,
        "Ctrl+C must return to the shell without a restart picker"
    );
    process.close_impl().unwrap();
}

#[test]
fn bash_queue_confirm_restart_and_restore() {
    require_pty_or_skip!("bash_queue_confirm_restart_and_restore");
    exercise("bash");
}

#[test]
fn cmd_queue_confirm_restart_and_restore() {
    if !clud::platform::host::WINDOWS {
        return;
    }
    require_pty_or_skip!("cmd_queue_confirm_restart_and_restore");
    exercise("cmd");
}

#[test]
fn powershell_queue_confirm_restart_and_restore() {
    if !clud::platform::host::WINDOWS {
        return;
    }
    require_pty_or_skip!("powershell_queue_confirm_restart_and_restore");
    exercise("powershell");
}

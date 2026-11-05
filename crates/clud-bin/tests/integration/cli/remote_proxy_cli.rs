//! Native launch + mock harness HTTP probe, without Bosn or provider credentials.
use crate::{common, exe};
use running_process::{
    CommandSpec, NativeProcess, ProcessConfig, ReadStatus, StderrMode, StdinMode,
};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant};

#[test]
fn remote_proxy_launch_uses_session_bearer_without_local_provider_credentials() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("empty-home");
    let bin = root.path().join("bin");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    let name = if clud::platform::host::WINDOWS {
        "claude.exe"
    } else {
        "claude"
    };
    std::fs::copy(common::mock_agent_path(), bin.join(name)).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let server = proxy_server(listener);
    let report = root.path().join("probe.json");
    let mut env: Vec<_> = std::env::vars()
        .filter(|(key, _)| {
            // PATH is replaced, but Windows still needs PATHEXT to resolve
            // the fixture's claude.exe from an extensionless harness name.
            !["HOME", "USERPROFILE", "PATH"]
                .iter()
                .any(|name| key.eq_ignore_ascii_case(name))
                && ![
                    "ANTHROPIC",
                    "OPENAI",
                    "OPENROUTER",
                    "CLAUDE",
                    "CLUD",
                    "HERMES",
                ]
                .iter()
                .any(|prefix| key.to_ascii_uppercase().starts_with(prefix))
        })
        .collect();
    for (key, value) in [
        ("HOME", home.to_string_lossy().into_owned()),
        ("USERPROFILE", home.to_string_lossy().into_owned()),
        ("PATH", bin.to_string_lossy().into_owned()),
        ("CLUD_SERVER_SETTINGS", "0".into()),
        ("CLUD_REMOTE_PROXY_ORIGIN", origin),
        (
            "CLUD_REMOTE_PROXY_TOKEN",
            "ct_integration_session_only_123456".into(),
        ),
        ("CLUD_REMOTE_PROXY_MODEL", "openrouter/auto".into()),
        (
            "CLAUDE_CONFIG_DIR",
            root.path()
                .join("isolated/claude")
                .to_string_lossy()
                .into_owned(),
        ),
        ("OPENROUTER_API_KEY", "local-provider-canary".into()),
        ("ANTHROPIC_API_KEY", "local-provider-canary".into()),
    ] {
        env.push((key.into(), value));
    }
    run(&env, root.path(), &report);
    let value: serde_json::Value = serde_json::from_slice(&std::fs::read(report).unwrap()).unwrap();
    assert_eq!(value["status"], 200, "{value}");
    assert_eq!(value["bridged_reply_received"], true, "{value}");
    assert!(!home.join(".clud/credentials").exists());
    server.join().unwrap();
}

fn proxy_server(listener: TcpListener) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut reader = BufReader::new(&mut socket);
        let mut headers = String::new();
        let mut length = 0;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line == "\r\n" || line.is_empty() {
                break;
            }
            if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                length = value.trim().parse::<usize>().unwrap();
            }
            headers.push_str(&line);
        }
        assert!(length <= 128 * 1024);
        let mut body = vec![0; length];
        reader.read_exact(&mut body).unwrap();
        assert!(headers.starts_with("POST /api/proxy/v1/messages HTTP/1.1"));
        assert!(headers.contains("Authorization: Bearer ct_integration_session_only_123456"));
        assert!(!headers.contains("local-provider-canary"));
        let request: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(request["messages"].is_array());
        let body = r#"{"content":[{"type":"text","text":"bridged reply"}]}"#;
        write!(
            socket,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
    })
}

fn run(env: &[(String, String)], root: &std::path::Path, report: &std::path::Path) {
    let process = NativeProcess::new(ProcessConfig {
        command: CommandSpec::Argv(vec![
            exe::bin_path("clud", option_env!("CARGO_BIN_EXE_clud"))
                .to_string_lossy()
                .into_owned(),
            "--safe".into(),
            "--no-daemon".into(),
            "-p".into(),
            "hello".into(),
            "--".into(),
            "--mock-codex-bridge-probe".into(),
            report.to_string_lossy().into_owned(),
        ]),
        cwd: Some(root.to_path_buf()),
        env: Some(env.to_vec()),
        capture: true,
        stderr_mode: StderrMode::Stdout,
        creationflags: None,
        create_process_group: false,
        stdin_mode: StdinMode::Null,
        nice: None,
        address_space_limit_bytes: None,
    });
    process.start().unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut output = String::new();
    loop {
        match process.read_combined(Some(Duration::from_millis(100))) {
            ReadStatus::Line(event) => output.push_str(&String::from_utf8_lossy(&event.line)),
            ReadStatus::Eof => break,
            ReadStatus::Timeout if Instant::now() >= deadline => {
                let _ = process.kill();
                panic!("remote launch timed out: {output}");
            }
            ReadStatus::Timeout if process.returncode().is_some() => break,
            ReadStatus::Timeout => {}
        }
    }
    assert_eq!(
        process.wait(Some(Duration::from_secs(5))).unwrap(),
        0,
        "{output}"
    );
}

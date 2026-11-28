use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::json;

use super::*;
use crate::toast::{Severity, Toast, ToastEvent};

/// Shadows the production `render_into` so no test reads the host's real
/// `CLAUDE_EFFORT` (set whenever the suite runs under Claude Code).
fn render_into(out: &mut Vec<u8>, args: &RunArgs, stdin: &[u8], now_ms: u64) {
    render_into_with_effort_env(out, args, stdin, now_ms, None);
}

fn writer_in(dir: &Path, pid: u32) -> StatusStateWriter {
    StatusStateWriter::new(state_path(dir, pid))
}

#[test]
fn a_published_toast_round_trips_through_the_state_file() {
    let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let writer = writer_in(dir.path(), 11);
    writer.publish(ToastEvent::Show(Toast::new(
        "cpu",
        "cpu 200 %",
        Severity::Warn,
        Instant::now(),
    )));
    let toast = read_live_toast(writer.path(), now_ms()).unwrap();
    assert_eq!(toast.text, "cpu 200 %");
    assert_eq!(toast.severity, Severity::Warn);
    assert_eq!(toast.expires_ms, None);
    writer.publish(ToastEvent::Close { key: "cpu".into() });
    assert!(read_live_toast(writer.path(), now_ms()).is_none());
}

#[test]
fn an_expiring_toast_disappears_on_its_own() {
    let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let writer = writer_in(dir.path(), 12);
    writer.publish(ToastEvent::Show(
        Toast::new("cpu", "back to normal", Severity::Info, Instant::now())
            .expiring_after(Duration::from_secs(10)),
    ));
    let now = now_ms();
    assert!(read_live_toast(writer.path(), now).is_some());
    assert!(read_live_toast(writer.path(), now + 11_000).is_none());
}

#[test]
fn an_orphaned_live_toast_goes_stale() {
    let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let path = state_path(dir.path(), 13);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let state = StatusState {
        owner_identity: None,
        updated_ms: 1_000,
        launch_nonce: String::new(),
        provider_label: None,
        toast: Some(StatusToast {
            text: "cpu 400 %".into(),
            severity: Severity::Alert,
            expires_ms: None,
        }),
        usage: None,
        redaction: None,
        efforts: Vec::new(),
    };
    std::fs::write(&path, serde_json::to_vec(&state).unwrap()).unwrap();
    let stale = u64::try_from(STALE_AFTER.as_millis()).unwrap();
    assert!(read_live_toast(&path, 1_000 + stale).is_some());
    assert!(read_live_toast(&path, 1_000 + stale + 1).is_none());
}

#[test]
fn dropping_the_writer_removes_the_file_and_garbage_reads_as_none() {
    let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let path = {
        let writer = writer_in(dir.path(), 14);
        writer.publish(ToastEvent::Show(Toast::new(
            "cpu",
            "x",
            Severity::Info,
            Instant::now(),
        )));
        assert!(writer.path().is_file());
        writer.path().to_path_buf()
    };
    assert!(!path.exists());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"{not json").unwrap();
    assert!(read_live_toast(&path, now_ms()).is_none());
    assert!(read_live_toast(&dir.path().join("missing.json"), now_ms()).is_none());
}

#[test]
fn the_rendered_line_is_coloured_and_control_free() {
    let line = render_line(&StatusToast {
        text: "cpu\x1b[2J 300 %".into(),
        severity: Severity::Alert,
        expires_ms: None,
    });
    assert!(line.starts_with("\x1b[1;38;5;203mclud \u{b7} cpu"));
    assert!(line.ends_with("\x1b[0m"));
    assert!(
        !line.contains("\x1b[2J"),
        "control bytes in toast text are stripped"
    );
}

#[test]
fn provider_usage_is_persistent_distinct_and_stale_safe() {
    let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let writer = writer_in(dir.path(), 15);
    writer.publish_usage(StatusUsage {
        provider: "codex".into(),
        model: "gpt-5.6-terra".into(),
        request_count: 2,
        cached_input_tokens: 12_345,
        uncached_input_tokens: 67_890,
        output_tokens: 12,
        cache_health: "healthy".into(),
        generation: 0,
    });
    let usage = read_live_usage(writer.path(), now_ms()).expect("live usage");
    assert_eq!(usage.cached_input_tokens, 12_345);
    assert_eq!(usage.uncached_input_tokens, 67_890);
    let line = render_usage(&usage);
    assert!(
        line.contains("read 12.3K cached / 67.8K uncached"),
        "{line}"
    );
    assert!(line.contains("write 12"), "{line}");
    assert!(line.contains("gpt-5.6-terra"), "{line}");
    assert_eq!(
        line,
        format!("\x1b[38;5;75m{}\x1b[0m", usage_summary(&usage))
    );
    let stale = now_ms().saturating_add(u64::try_from(STALE_AFTER.as_millis()).unwrap() + 1);
    assert!(read_live_usage(writer.path(), stale).is_some());
}

#[test]
fn dead_owner_cannot_keep_bridge_usage_visible() {
    let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let writer = writer_in(dir.path(), 204);
    writer.publish_usage(StatusUsage {
        provider: "codex".into(),
        model: "fixture-model".into(),
        request_count: 1,
        cached_input_tokens: 0,
        uncached_input_tokens: 3,
        output_tokens: 2,
        cache_health: "unavailable".into(),
        generation: 0,
    });
    let mut state: StatusState =
        serde_json::from_slice(&std::fs::read(writer.path()).unwrap()).unwrap();
    let identity = state.owner_identity.unwrap();
    state.owner_identity = Some(crate::process_identity::ProcessIdentity::new(
        identity.pid,
        identity.start_time.saturating_add(100),
    ));
    std::fs::write(writer.path(), serde_json::to_vec(&state).unwrap()).unwrap();
    assert!(read_live_usage(writer.path(), now_ms()).is_none());
    let args = RunArgs {
        session_pid: 204,
        state_dir: dir.path().to_path_buf(),
        chain_b64: None,
    };
    let mut out = Vec::new();
    render_into(&mut out, &args, b"{}", now_ms());
    assert!(!String::from_utf8(out).unwrap().contains("read "));
}

#[test]
fn statusline_never_presents_a_last_call_as_cumulative_usage() {
    let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let args = RunArgs {
        session_pid: 17,
        state_dir: dir.path().to_path_buf(),
        chain_b64: None,
    };
    let stdin = br#"{
        "model": {"id": "claude-opus-5"},
        "context_window": {"current_usage": {
            "input_tokens": 1,
            "cache_creation_input_tokens": 2,
            "cache_read_input_tokens": 3,
            "output_tokens": 4
        }},
        "prompt_cache": {"warm": false}
    }"#;
    let mut out = Vec::new();
    render_into(&mut out, &args, stdin, now_ms());
    let text = String::from_utf8_lossy(&out);
    assert!(text.contains("claude-opus-5"), "{text}");
    assert!(!text.contains("read "), "{text}");
    assert!(!text.contains("last call"), "{text}");
    assert!(!text.contains("cumulative unavailable"), "{text}");
}

#[test]
fn exact_bridge_usage_wins_over_documented_claude_last_call() {
    let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let writer = writer_in(dir.path(), 18);
    writer.publish_usage(StatusUsage {
        provider: "codex".into(),
        model: "gpt-5.6-terra".into(),
        request_count: 1,
        cached_input_tokens: 8,
        uncached_input_tokens: 9,
        output_tokens: 10,
        cache_health: "healthy".into(),
        generation: 0,
    });
    let args = RunArgs {
        session_pid: 18,
        state_dir: dir.path().to_path_buf(),
        chain_b64: None,
    };
    let stdin = br#"{
        "model": {"display_name": "Opus"},
        "context_window": {"current_usage": {
            "input_tokens": 1,
            "cache_creation_input_tokens": 2,
            "cache_read_input_tokens": 3,
            "output_tokens": 4
        }}
    }"#;
    let mut out = Vec::new();
    render_into(
        &mut out,
        &args,
        stdin,
        now_ms() + u64::try_from(STALE_AFTER.as_millis()).unwrap() + 1,
    );
    let text = String::from_utf8_lossy(&out);
    assert!(text.contains("gpt-5.6-terra"), "{text}");
    assert!(!text.contains("Claude last call"), "{text}");
    assert!(!text.contains("cache healthy"), "{text}");
    assert!(!text.contains("codex gpt"), "{text}");
}

fn fixture_transcript(dir: &Path, subagents: bool) -> std::path::PathBuf {
    let main = dir.join("fixture.jsonl");
    std::fs::write(
        &main,
        include_str!("../../tests/fixtures/statusline/main.jsonl"),
    )
    .unwrap();
    if subagents {
        let agents = dir.join("fixture").join("subagents");
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::write(
            agents.join("agent-one.jsonl"),
            include_str!("../../tests/fixtures/statusline/agent.jsonl"),
        )
        .unwrap();
    }
    main
}

fn transcript_stdin(path: &Path) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "transcript_path": path,
        "model": {"id": "~deepseek/deepseek-flash-latest"},
    }))
    .unwrap()
}

#[test]
fn fixture_transcript_dedupes_main_and_subagents_into_one_cumulative_line() {
    let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let writer = StatusStateWriter::new_with_provider(state_path(dir.path(), 191), "openrouter");
    let path = fixture_transcript(dir.path(), true);
    let args = RunArgs {
        session_pid: 191,
        state_dir: dir.path().to_path_buf(),
        chain_b64: None,
    };
    let stdin = transcript_stdin(&path);
    let mut out = Vec::new();
    render_into(&mut out, &args, &stdin, now_ms());
    let text = String::from_utf8(out).unwrap();
    assert!(
        text.contains("read 800 cached / 62 uncached - write 16"),
        "{text}"
    );
    assert!(!text.contains("last call"), "{text}");
    assert!(!text.contains("cache "), "{text}");
    let usage = writer.effective_usage_snapshot().unwrap();
    assert_eq!(usage.request_count, 4);
    assert_eq!(usage.cached_input_tokens, 800);
    assert_eq!(usage.provider, "openrouter");
    let sidecar =
        std::fs::read_to_string(super::super::usage_ledger::snapshot_path(writer.path())).unwrap();
    assert!(!sidecar.contains("msg-main-one"));
    assert!(!sidecar.contains("msg-agent-one"));
    assert!(!sidecar.contains("fixture.jsonl"));
    assert!(!sidecar.contains("subagents"));
    assert!(!sidecar.contains("secret prompt"));
    let cursor = std::fs::read_to_string(writer.path().with_extension("usage.state.json")).unwrap();
    assert!(!cursor.contains("msg-main-one"));
    assert!(!cursor.contains("secret prompt"));
}

#[test]
fn fixture_without_subagent_directory_and_repeated_callbacks_stays_exact() {
    let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let writer = writer_in(dir.path(), 192);
    let path = fixture_transcript(dir.path(), false);
    let args = RunArgs {
        session_pid: 192,
        state_dir: dir.path().to_path_buf(),
        chain_b64: None,
    };
    let stdin = transcript_stdin(&path);
    for _ in 0..3 {
        let mut out = Vec::new();
        render_into(&mut out, &args, &stdin, now_ms());
        assert!(String::from_utf8(out)
            .unwrap()
            .contains("read 700 cached / 60 uncached - write 15"));
    }
    assert_eq!(writer.effective_usage_snapshot().unwrap().request_count, 3);
}

#[test]
fn malformed_transcript_suppresses_an_unprovable_total() {
    let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let writer = writer_in(dir.path(), 194);
    let path = fixture_transcript(dir.path(), false);
    let args = RunArgs {
        session_pid: 194,
        state_dir: dir.path().to_path_buf(),
        chain_b64: None,
    };
    let stdin = transcript_stdin(&path);
    let mut out = Vec::new();
    render_into(&mut out, &args, &stdin, now_ms());
    std::fs::write(&path, "not JSONL\n").unwrap();
    out.clear();
    render_into(&mut out, &args, &stdin, now_ms());
    assert!(!String::from_utf8(out).unwrap().contains("read "));
    assert!(writer.effective_usage_snapshot().is_none());
}

#[test]
fn usage_free_transcript_renders_only_the_model() {
    let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let _writer = writer_in(dir.path(), 199);
    let path = dir.path().join("empty-usage.jsonl");
    std::fs::write(&path, "{}\n").unwrap();
    let args = RunArgs {
        session_pid: 199,
        state_dir: dir.path().to_path_buf(),
        chain_b64: None,
    };
    let mut out = Vec::new();
    render_into(&mut out, &args, &transcript_stdin(&path), now_ms());
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("~deepseek/deepseek-flash-latest"));
    assert!(!text.contains("read "));
}

#[test]
fn replacement_transcript_adds_only_new_message_ids() {
    let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let writer = writer_in(dir.path(), 195);
    let path = fixture_transcript(dir.path(), false);
    let args = RunArgs {
        session_pid: 195,
        state_dir: dir.path().to_path_buf(),
        chain_b64: None,
    };
    let stdin = transcript_stdin(&path);
    let mut out = Vec::new();
    render_into(&mut out, &args, &stdin, now_ms());
    let next = json!({"message": {"id": "msg-new", "model": "new-model", "usage": {
        "input_tokens": 2, "cache_creation_input_tokens": 3,
        "cache_read_input_tokens": 4, "output_tokens": 5
    }}});
    std::fs::write(&path, format!("{next}\n")).unwrap();
    out.clear();
    render_into(&mut out, &args, &stdin, now_ms());
    assert!(String::from_utf8_lossy(&out).contains("read 704 cached / 65 uncached - write 20"));
    assert_eq!(writer.effective_usage_snapshot().unwrap().request_count, 4);
}

#[test]
fn clear_starts_a_fresh_direct_ledger_for_the_new_transcript() {
    let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let writer = writer_in(dir.path(), 198);
    let path = fixture_transcript(dir.path(), true);
    let args = RunArgs {
        session_pid: 198,
        state_dir: dir.path().to_path_buf(),
        chain_b64: None,
    };
    let mut out = Vec::new();
    render_into(&mut out, &args, &transcript_stdin(&path), now_ms());
    assert!(String::from_utf8_lossy(&out).contains("read 800 cached / 62 uncached - write 16"));
    // `/clear` hands the callback a new transcript that has no response yet
    // (#2045): neither the callback nor the HUD may show the old totals.
    let cleared = dir.path().join("cleared.jsonl");
    out.clear();
    render_into(&mut out, &args, &transcript_stdin(&cleared), now_ms());
    let text = String::from_utf8(out.clone()).unwrap();
    assert!(!text.contains("read "), "{text}");
    assert!(text.contains("deepseek-flash-latest"), "{text}");
    assert_eq!(writer.effective_usage_snapshot(), None);
    let next = json!({"message": {"id": "msg-after-clear", "model": "new-model", "usage": {
        "input_tokens": 2, "cache_creation_input_tokens": 3,
        "cache_read_input_tokens": 4, "output_tokens": 5
    }}});
    std::fs::write(&cleared, format!("{next}\n")).unwrap();
    out.clear();
    render_into(&mut out, &args, &transcript_stdin(&cleared), now_ms());
    let text = String::from_utf8(out).unwrap();
    assert!(
        text.contains("read 4 cached / 5 uncached - write 5"),
        "{text}"
    );
    let usage = writer.effective_usage_snapshot().unwrap();
    assert_eq!(usage.request_count, 1);
    assert_eq!(usage.model, "new-model");
}

#[test]
fn a_usage_reset_drops_the_snapshot_and_retires_older_generations() {
    let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let writer = writer_in(dir.path(), 17);
    let snapshot = |generation, request_count| StatusUsage {
        provider: "deepseek".into(),
        model: "deepseek-v4.1-flash".into(),
        request_count,
        cached_input_tokens: 0,
        uncached_input_tokens: request_count * 100,
        output_tokens: request_count,
        cache_health: "unavailable".into(),
        generation,
    };
    writer.publish_usage(snapshot(0, 5));
    writer.reset_usage(1);
    assert_eq!(writer.usage_snapshot(), None);
    assert_eq!(read_live_usage(writer.path(), now_ms()), None);
    // A worker that finished before the clear cannot bring the old totals back.
    writer.publish_usage(snapshot(0, 6));
    assert_eq!(writer.usage_snapshot(), None);
    writer.publish_usage(snapshot(1, 1));
    assert_eq!(
        read_live_usage(writer.path(), now_ms()),
        Some(snapshot(1, 1))
    );
    // A stale or repeated reset is ignored.
    writer.reset_usage(1);
    assert_eq!(writer.usage_snapshot(), Some(snapshot(1, 1)));
}

#[test]
fn direct_ledger_survives_a_quiet_parent_past_toast_staleness() {
    let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let writer = writer_in(dir.path(), 196);
    let path = fixture_transcript(dir.path(), false);
    let args = RunArgs {
        session_pid: 196,
        state_dir: dir.path().to_path_buf(),
        chain_b64: None,
    };
    let future = now_ms() + u64::try_from(STALE_AFTER.as_millis()).unwrap() + 1;
    let mut out = Vec::new();
    render_into(&mut out, &args, &transcript_stdin(&path), future);
    assert!(String::from_utf8(out)
        .unwrap()
        .contains("read 700 cached / 60 uncached - write 15"));
    assert!(read_live_usage(writer.path(), future).is_some());
}

#[test]
fn overlapping_statusline_callbacks_commit_each_response_once() {
    let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let writer = writer_in(dir.path(), 197);
    let path = fixture_transcript(dir.path(), true);
    let args = RunArgs {
        session_pid: 197,
        state_dir: dir.path().to_path_buf(),
        chain_b64: None,
    };
    let stdin = transcript_stdin(&path);
    std::thread::scope(|scope| {
        for _ in 0..8 {
            let args = args.clone();
            let stdin = stdin.clone();
            scope.spawn(move || {
                let mut out = Vec::new();
                render_into(&mut out, &args, &stdin, now_ms());
            });
        }
    });
    let mut out = Vec::new();
    render_into(&mut out, &args, &stdin, now_ms());
    assert!(String::from_utf8(out)
        .unwrap()
        .contains("read 800 cached / 62 uncached - write 16"));
    assert_eq!(writer.effective_usage_snapshot().unwrap().request_count, 4);
}

// The callback runs on a two-second cadence, so it must not rescan the whole
// transcript each time. Asserted through the persisted cursor, not elapsed
// time: a wall-clock budget fails on an oversubscribed runner (#2153).
#[test]
fn incremental_callback_resumes_from_its_cursor_for_thousands_of_records() {
    use std::io::Write;
    let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let _writer = writer_in(dir.path(), 198);
    let path = dir.path().join("large.jsonl");
    let file = std::fs::File::create(&path).unwrap();
    let mut file = std::io::BufWriter::new(file);
    for index in 0..3000 {
        let record = json!({"message": {"id": format!("msg-{index}"), "model": "fixture-model", "usage": {
            "input_tokens": 1, "cache_creation_input_tokens": 0,
            "cache_read_input_tokens": 2, "output_tokens": 3
        }}});
        writeln!(file, "{record}").unwrap();
    }
    file.flush().unwrap();
    let args = RunArgs {
        session_pid: 198,
        state_dir: dir.path().to_path_buf(),
        chain_b64: None,
    };
    let stdin = transcript_stdin(&path);
    let len = std::fs::metadata(&path).unwrap().len();
    let ledger = state_path(dir.path(), 198).with_extension("usage.state.json");
    let cursor = || -> u64 {
        let state: Value = serde_json::from_slice(&std::fs::read(&ledger).unwrap()).unwrap();
        let files = state["files"].as_object().unwrap();
        assert_eq!(files.len(), 1, "{state}");
        files.values().next().unwrap()["offset"].as_u64().unwrap()
    };
    render_into(&mut Vec::new(), &args, &stdin, now_ms());
    assert_eq!(
        cursor(),
        len,
        "the first callback consumes the whole transcript"
    );
    let mut out = Vec::new();
    render_into(&mut out, &args, &stdin, now_ms());
    assert_eq!(
        cursor(),
        len,
        "the next callback resumes at the end, not from zero"
    );
    assert!(String::from_utf8(out)
        .unwrap()
        .contains("read 6K cached / 3K uncached - write 9K"));
}

#[test]
fn a_delayed_older_usage_snapshot_cannot_replace_newer_totals() {
    let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let writer = writer_in(dir.path(), 16);
    let snapshot = |request_count, uncached_input_tokens| StatusUsage {
        provider: "codex".into(),
        model: "gpt-5.6-terra".into(),
        request_count,
        cached_input_tokens: 0,
        uncached_input_tokens,
        output_tokens: request_count,
        cache_health: "healthy".into(),
        generation: 0,
    };
    writer.publish_usage(snapshot(2, 200));
    // This simulates an older worker acquiring the status writer after the
    // second terminal response already committed its aggregate snapshot.
    writer.publish_usage(snapshot(1, 100));
    assert_eq!(
        read_live_usage(writer.path(), now_ms()),
        Some(snapshot(2, 200)),
        "the visible ledger must remain monotonic"
    );
}

#[test]
fn the_command_quotes_paths_and_carries_the_chain_losslessly() {
    let chain = "bash ~/.claude/statusline.sh | tr 'a' \"b\"";
    let cmd = statusline_command(
        Path::new("/opt/my tools/clud"),
        4242,
        Path::new("/home/u/.clud/state"),
        Some(chain),
        false,
    )
    .unwrap();
    assert!(cmd.starts_with(
        "\"/opt/my tools/clud\" statusline --session-pid 4242 --state-dir \"/home/u/.clud/state\""
    ));
    let encoded = cmd.rsplit(' ').next().unwrap();
    assert_eq!(decode_chain(encoded).as_deref(), Some(chain));
    assert!(
        !encoded.contains(['+', '/', '=']),
        "URL-safe, unpadded, shell-inert"
    );
}

#[test]
fn windows_commands_use_forward_slashes() {
    let cmd = statusline_command(
        Path::new(r"C:\Users\me\AppData\Roaming\Python\Scripts\clud.exe"),
        7,
        Path::new(r"C:\Users\me\.clud\state"),
        None,
        true,
    )
    .unwrap();
    assert_eq!(
        cmd,
        "\"C:/Users/me/AppData/Roaming/Python/Scripts/clud.exe\" statusline --session-pid 7 --state-dir \"C:/Users/me/.clud/state\""
    );
}

#[test]
fn paths_that_would_need_shell_escaping_are_refused() {
    for bad in ["/tmp/a\"b/clud", "/tmp/$HOME/clud", "/tmp/`x`/clud"] {
        assert!(
            statusline_command(Path::new(bad), 1, Path::new("/s"), None, false).is_none(),
            "{bad}"
        );
    }
}

#[test]
fn only_real_user_commands_are_chained() {
    assert_eq!(chained_command(None), None);
    assert_eq!(
        chained_command(Some(&json!({"type": "command", "command": " echo hi "}))),
        Some("echo hi".into())
    );
    assert_eq!(
        chained_command(Some(&json!({"command": "echo hi"}))),
        Some("echo hi".into())
    );
    assert_eq!(
        chained_command(Some(&json!({"type": "static", "command": "x"}))),
        None
    );
    assert_eq!(
        chained_command(Some(&json!({"type": "command", "command": "  "}))),
        None
    );
    let ours = statusline_command(Path::new("/c"), 1, Path::new("/s"), None, false).unwrap();
    assert_eq!(
        chained_command(Some(&json!({"type": "command", "command": ours}))),
        None,
        "clud's own command is never chained into itself"
    );
}

#[test]
fn the_composed_setting_keeps_user_fields_and_never_slows_refresh() {
    let fresh = compose_setting(None, "cmd".into());
    assert_eq!(
        fresh,
        json!({"type": "command", "command": "cmd", "refreshInterval": REFRESH_INTERVAL_SECS})
    );
    let user = json!({"type": "command", "command": "theirs", "padding": 2, "refreshInterval": 30});
    let merged = compose_setting(Some(&user), "ours".into());
    assert_eq!(merged["padding"], 2);
    assert_eq!(merged["command"], "ours");
    assert_eq!(merged["refreshInterval"], REFRESH_INTERVAL_SECS);
    let faster = compose_setting(Some(&json!({"refreshInterval": 1})), "ours".into());
    assert_eq!(faster["refreshInterval"], 1);
}

#[test]
fn discovery_follows_claude_codes_precedence() {
    let root = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let home = root.path().join("home");
    let project = root.path().join("work").join("repo");
    let nested = project.join("src").join("deep");
    std::fs::create_dir_all(home.join(".claude")).unwrap();
    std::fs::create_dir_all(project.join(".claude")).unwrap();
    std::fs::create_dir_all(&nested).unwrap();
    let write = |path: std::path::PathBuf, cmd: &str| {
        std::fs::write(
            path,
            json!({"statusLine": {"type": "command", "command": cmd}}).to_string(),
        )
        .unwrap();
    };

    assert_eq!(discover_user_statusline(None, &nested, Some(&home)), None);
    write(home.join(".claude/settings.json"), "home");
    let found = |explicit: Option<&Value>| {
        discover_user_statusline(explicit, &nested, Some(&home))
            .map(|v| v["command"].as_str().unwrap().to_string())
    };
    assert_eq!(found(None).as_deref(), Some("home"));
    write(project.join(".claude/settings.json"), "project");
    assert_eq!(found(None).as_deref(), Some("project"));
    write(project.join(".claude/settings.local.json"), "local");
    assert_eq!(found(None).as_deref(), Some("local"));
    let explicit = json!({"statusLine": {"type": "command", "command": "explicit"}});
    assert_eq!(found(Some(&explicit)).as_deref(), Some("explicit"));
}

#[test]
fn a_project_settings_file_without_a_status_line_falls_back_to_home() {
    let root = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let home = root.path().join("home");
    let project = root.path().join("repo");
    std::fs::create_dir_all(home.join(".claude")).unwrap();
    std::fs::create_dir_all(project.join(".claude")).unwrap();
    std::fs::write(project.join(".claude/settings.json"), r#"{"model": "x"}"#).unwrap();
    std::fs::write(
        home.join(".claude/settings.json"),
        r#"{"statusLine": {"type": "command", "command": "home"}}"#,
    )
    .unwrap();
    let found = discover_user_statusline(None, &project, Some(&home)).unwrap();
    assert_eq!(found["command"], "home");
}

/// Runs a real shell on every CI lane: `sh -c` on Linux and macOS, Git Bash
/// or `cmd` on Windows.
#[test]
fn the_chain_runs_through_the_platform_shell_with_claudes_stdin() {
    let out = run_chain("echo chained-status", b"{\"model\":{}}").expect("chain ran");
    assert!(
        String::from_utf8_lossy(&out).contains("chained-status"),
        "stdout: {:?}",
        String::from_utf8_lossy(&out)
    );
}

fn argv_of(spec: Option<CommandSpec>) -> Vec<String> {
    match spec {
        Some(CommandSpec::Argv(argv)) => argv,
        _ => panic!("expected CommandSpec::Argv"),
    }
}

#[test]
fn windows_chain_spec_uses_valid_git_bash_env_path() {
    let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let bash = dir.path().join("bash.exe");
    std::fs::write(&bash, b"").unwrap();
    let spec = windows_chain_spec("echo hi", Some(bash.clone().into_os_string()), || {
        panic!("which must not run when the env path is a file")
    });
    assert_eq!(
        argv_of(spec),
        vec![
            bash.to_string_lossy().into_owned(),
            "-c".to_string(),
            "echo hi".to_string()
        ]
    );
}

#[test]
fn windows_chain_spec_ignores_missing_env_path_and_locates_bash() {
    let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let missing = dir.path().join("no-such-bash.exe");
    let spec = windows_chain_spec("echo hi", Some(missing.into_os_string()), || {
        Some(std::path::PathBuf::from("/x/bash"))
    });
    assert_eq!(argv_of(spec), vec!["/x/bash", "-c", "echo hi"]);
}

#[test]
fn windows_chain_spec_ignores_directory_env_path() {
    let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let spec = windows_chain_spec(
        "echo hi",
        Some(dir.path().as_os_str().to_os_string()),
        || Some(std::path::PathBuf::from("/x/bash")),
    );
    assert_eq!(argv_of(spec), vec!["/x/bash", "-c", "echo hi"]);
}

/// #1371: a `statusLine.command` is authored for Git Bash, so without one it
/// must not be handed to `cmd.exe`, whose quoting and `$VAR` rules differ.
#[test]
fn windows_chain_spec_never_hands_a_bash_command_to_cmd() {
    let spec = windows_chain_spec("echo \"$CLAUDE_MODEL\" | cut -d/ -f2", None, || None);
    assert!(
        spec.is_none(),
        "a bash-authored status line must not fall back to cmd.exe"
    );
}

/// #1371: without Git Bash the footer says why the user's line is missing
/// instead of rendering nothing, and no process is started.
#[test]
fn a_missing_git_bash_renders_a_visible_notice_not_an_empty_line() {
    let mut out = Vec::new();
    render_chain(&mut out, None, b"{}");
    let text = String::from_utf8(out).unwrap();
    assert_eq!(text, format!("{NO_GIT_BASH_NOTICE}\n"));
    assert!(text.contains("Git Bash not found"), "{text:?}");
    assert!(text.contains("CLAUDE_CODE_GIT_BASH_PATH"), "{text:?}");
}

/// Git for Windows puts only `Git\cmd` on PATH by default, so `bash` is
/// usually not on PATH even when Git Bash is installed; it is found beside
/// `git.exe` instead, then under `%ProgramFiles%`.
#[test]
fn git_bash_is_found_beside_git_and_in_program_files() {
    let git = Path::new("C:/Tools/Git/cmd/git.exe");
    let program_files = Path::new("C:/Program Files");
    assert_eq!(
        git_bash_candidates(Some(git), Some(program_files)),
        vec![
            Path::new("C:/Tools/Git").join("bin").join("bash.exe"),
            program_files.join("Git").join("bin").join("bash.exe"),
        ]
    );
    assert_eq!(
        git_bash_candidates(Some(Path::new("C:/Git/bin/git.exe")), None),
        vec![Path::new("C:/Git").join("bin").join("bash.exe")]
    );
    assert!(git_bash_candidates(None, None).is_empty());
}

/// Runs on the Windows CI lane, whose runner (like any machine that runs
/// Claude Code on Windows) has Git for Windows installed.
#[test]
fn chain_spec_finds_the_installed_git_bash_on_windows() {
    if !crate::platform::host::WINDOWS {
        return;
    }
    let bash = locate_git_bash().expect("Git Bash is installed on the Windows runner");
    assert!(bash.is_file(), "{}", bash.display());
    let argv = argv_of(chain_spec("echo hi"));
    assert_eq!(&argv[1..], ["-c", "echo hi"]);
}

#[test]
fn chain_spec_non_windows_uses_sh() {
    if crate::platform::host::WINDOWS {
        return;
    }
    assert_eq!(argv_of(chain_spec("echo hi")), vec!["sh", "-c", "echo hi"]);
}

#[test]
fn the_chain_receives_the_session_json_on_stdin() {
    if !crate::platform::host::UNIX {
        return;
    }
    let out = run_chain("cat", b"{\"session_id\":\"abc\"}").expect("chain ran");
    assert!(String::from_utf8_lossy(&out).contains("\"session_id\":\"abc\""));
}

#[test]
fn render_prints_the_user_line_then_the_live_toast() {
    let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let writer = writer_in(dir.path(), 21);
    writer.publish(ToastEvent::Show(Toast::new(
        "cpu",
        "cpu 180 %",
        Severity::Warn,
        Instant::now(),
    )));
    let args = RunArgs {
        session_pid: 21,
        state_dir: dir.path().to_path_buf(),
        chain_b64: Some(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode("echo user-line")),
    };
    let mut out = Vec::new();
    render_into(&mut out, &args, b"{}", now_ms());
    let text = String::from_utf8_lossy(&out);
    let user = text.find("user-line").expect("user line first");
    let toast = text.find("cpu 180 %").expect("toast second");
    assert!(user < toast, "{text:?}");

    let quiet = RunArgs {
        session_pid: 99,
        state_dir: dir.path().to_path_buf(),
        chain_b64: None,
    };
    let mut out = Vec::new();
    render_into(&mut out, &quiet, b"{}", now_ms());
    assert!(out.is_empty(), "no user line and no toast prints nothing");
}

#[test]
fn render_prints_usage_between_the_user_line_and_toast() {
    let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let writer = writer_in(dir.path(), 22);
    writer.publish_usage(StatusUsage {
        provider: "codex".into(),
        model: "gpt-5.6-terra".into(),
        request_count: 1,
        cached_input_tokens: 5,
        uncached_input_tokens: 6,
        output_tokens: 7,
        cache_health: "cold".into(),
        generation: 0,
    });
    writer.publish(ToastEvent::Show(Toast::new(
        "cpu",
        "cpu 180 %",
        Severity::Warn,
        Instant::now(),
    )));
    let args = RunArgs {
        session_pid: 22,
        state_dir: dir.path().to_path_buf(),
        chain_b64: Some(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode("echo user-line")),
    };
    let mut out = Vec::new();
    render_into(&mut out, &args, b"{}", now_ms());
    let text = String::from_utf8_lossy(&out);
    assert!(text.find("user-line").unwrap() < text.find("gpt-5.6-terra").unwrap());
    assert!(text.find("gpt-5.6-terra").unwrap() < text.find("cpu 180 %").unwrap());
}

#[test]
fn revised_response_counters_replace_instead_of_double_counting() {
    let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let writer = writer_in(dir.path(), 201);
    let transcript = dir.path().join("revisions.jsonl");
    let first = json!({"message": {"id": "same-response", "model": "fixture-model", "usage": {
        "input_tokens": 2, "cache_creation_input_tokens": 3,
        "cache_read_input_tokens": 4, "output_tokens": 5
    }}});
    std::fs::write(&transcript, format!("{first}\n")).unwrap();
    let args = RunArgs {
        session_pid: 201,
        state_dir: dir.path().to_path_buf(),
        chain_b64: None,
    };
    let stdin = transcript_stdin(&transcript);
    render_into(&mut Vec::new(), &args, &stdin, now_ms());
    let revised = json!({"message": {"id": "same-response", "model": "fixture-model", "usage": {
        "input_tokens": 2, "cache_creation_input_tokens": 3,
        "cache_read_input_tokens": 4, "output_tokens": 7
    }}});
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&transcript)
        .unwrap();
    use std::io::Write;
    writeln!(file, "{revised}").unwrap();
    let mut out = Vec::new();
    render_into(&mut out, &args, &stdin, now_ms());
    assert!(String::from_utf8_lossy(&out).contains("read 4 cached / 5 uncached - write 7"));
    assert_eq!(writer.effective_usage_snapshot().unwrap().request_count, 1);
    let changed_tuple = json!({"message": {"id": "same-response", "model": "fixture-model", "usage": {
        "input_tokens": 1, "cache_creation_input_tokens": 3,
        "cache_read_input_tokens": 10, "output_tokens": 9
    }}});
    writeln!(file, "{changed_tuple}").unwrap();
    out.clear();
    render_into(&mut out, &args, &stdin, now_ms());
    assert!(String::from_utf8(out)
        .unwrap()
        .contains("read 10 cached / 4 uncached - write 9"));
}

#[test]
fn malformed_complete_line_is_checkpointed_before_later_usage() {
    use std::io::Write;
    let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let writer = writer_in(dir.path(), 202);
    let transcript = dir.path().join("malformed.jsonl");
    std::fs::write(&transcript, "bad json\n").unwrap();
    let args = RunArgs {
        session_pid: 202,
        state_dir: dir.path().to_path_buf(),
        chain_b64: None,
    };
    let stdin = transcript_stdin(&transcript);
    render_into(&mut Vec::new(), &args, &stdin, now_ms());
    let record = json!({"message": {"id": "after-bad", "model": "fixture-model", "usage": {
        "input_tokens": 2, "cache_creation_input_tokens": 3,
        "cache_read_input_tokens": 4, "output_tokens": 5
    }}});
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&transcript)
        .unwrap();
    writeln!(file, "{record}").unwrap();
    render_into(&mut Vec::new(), &args, &stdin, now_ms());
    assert!(writer.effective_usage_snapshot().is_none());
}

#[test]
fn incomplete_usage_record_suppresses_an_otherwise_valid_total() {
    let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let writer = writer_in(dir.path(), 205);
    let transcript = dir.path().join("incomplete-usage.jsonl");
    let valid = json!({"message": {"id": "valid-response", "model": "fixture-model", "usage": {
        "input_tokens": 2, "cache_creation_input_tokens": 3,
        "cache_read_input_tokens": 4, "output_tokens": 5
    }}});
    let incomplete = json!({"message": {"id": "incomplete-response", "usage": {
        "input_tokens": 7, "cache_creation_input_tokens": 8,
        "cache_read_input_tokens": 9
    }}});
    std::fs::write(&transcript, format!("{valid}\n{incomplete}\n")).unwrap();
    let args = RunArgs {
        session_pid: 205,
        state_dir: dir.path().to_path_buf(),
        chain_b64: None,
    };
    let mut out = Vec::new();
    render_into(&mut out, &args, &transcript_stdin(&transcript), now_ms());
    assert!(!String::from_utf8_lossy(&out).contains("read "));
    assert!(writer.effective_usage_snapshot().is_none());
}

#[test]
fn empty_response_id_suppresses_an_otherwise_valid_total() {
    let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let writer = writer_in(dir.path(), 206);
    let transcript = dir.path().join("empty-id.jsonl");
    let valid = json!({"message": {"id": "valid-response", "model": "fixture-model", "usage": {
        "input_tokens": 2, "cache_creation_input_tokens": 3,
        "cache_read_input_tokens": 4, "output_tokens": 5
    }}});
    let empty_id = json!({"message": {"id": "  ", "usage": {
        "input_tokens": 10, "cache_creation_input_tokens": 11,
        "cache_read_input_tokens": 12, "output_tokens": 13
    }}});
    std::fs::write(&transcript, format!("{valid}\n{empty_id}\n")).unwrap();
    let args = RunArgs {
        session_pid: 206,
        state_dir: dir.path().to_path_buf(),
        chain_b64: None,
    };
    let mut out = Vec::new();
    render_into(&mut out, &args, &transcript_stdin(&transcript), now_ms());
    assert!(!String::from_utf8_lossy(&out).contains("read "));
    assert!(writer.effective_usage_snapshot().is_none());
}

#[test]
fn unchanged_callback_does_not_rewrite_cursor_state() {
    let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let _writer = writer_in(dir.path(), 203);
    let transcript = fixture_transcript(dir.path(), false);
    let args = RunArgs {
        session_pid: 203,
        state_dir: dir.path().to_path_buf(),
        chain_b64: None,
    };
    let stdin = transcript_stdin(&transcript);
    render_into(&mut Vec::new(), &args, &stdin, now_ms());
    let cursor = state_path(dir.path(), 203).with_extension("usage.state.json");
    let before = std::fs::metadata(&cursor).unwrap().modified().unwrap();
    render_into(&mut Vec::new(), &args, &stdin, now_ms() + 1_000);
    assert_eq!(
        std::fs::metadata(&cursor).unwrap().modified().unwrap(),
        before
    );
}

fn effort_args(dir: &Path, pid: u32) -> RunArgs {
    RunArgs {
        session_pid: pid,
        state_dir: dir.to_path_buf(),
        chain_b64: None,
    }
}

#[test]
fn claude_effort_env_appends_a_highlighted_effort_segment_after_the_user_line() {
    use base64::Engine as _;
    let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let args = RunArgs {
        chain_b64: Some(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode("echo user-line")),
        ..effort_args(dir.path(), 301)
    };
    let mut out = Vec::new();
    render_into_with_effort_env(&mut out, &args, b"{}", now_ms(), Some("high"));
    let text = String::from_utf8(out).unwrap();
    let user = text.find("user-line").expect("user line printed");
    let effort = text.find("effort: high").expect("effort segment");
    assert!(user < effort, "{text}");
    assert!(text.contains("\x1b[1;38;5;214meffort: high"), "{text}");
}

#[test]
fn stdin_effort_level_wins_over_claude_effort_env() {
    let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let args = effort_args(dir.path(), 302);
    let stdin = br#"{"model":{"id":"claude-opus-5"},"effort":{"level":"medium"}}"#;
    let mut out = Vec::new();
    render_into_with_effort_env(&mut out, &args, stdin, now_ms(), Some("max"));
    let text = String::from_utf8(out).unwrap();
    assert!(
        text.contains("\x1b[38;5;75meffort: medium\x1b[0m"),
        "{text}"
    );
    assert!(!text.contains("max"), "{text}");
}

#[test]
fn no_effort_source_leaves_the_output_unchanged() {
    let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let args = effort_args(dir.path(), 303);
    let stdin = br#"{"model":{"id":"claude-opus-5"}}"#;
    let mut out = Vec::new();
    render_into_with_effort_env(&mut out, &args, stdin, now_ms(), None);
    assert_eq!(
        String::from_utf8(out).unwrap(),
        "\x1b[38;5;75mclaude-opus-5\x1b[0m\n"
    );
}

#[test]
fn gateway_agent_requests_above_the_session_effort_are_counted() {
    let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let writer = writer_in(dir.path(), 304);
    writer.record_effort("medium", false);
    for _ in 0..3 {
        writer.record_effort("high", true);
    }
    writer.record_effort("low", true);
    let args = effort_args(dir.path(), 304);
    let mut out = Vec::new();
    render_into_with_effort_env(&mut out, &args, b"{}", now_ms(), None);
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("effort: medium"), "{text}");
    assert!(text.contains("agents: 3\u{d7}high"), "{text}");
    assert!(!text.contains("low"), "{text}");
}

#[test]
fn effort_events_outside_the_window_are_ignored() {
    let events = vec![EffortEvent {
        ms: 1_000,
        level: "max".into(),
        agent: true,
    }];
    let window = u64::try_from(EFFORT_WINDOW.as_millis()).unwrap();
    assert!(render_effort(b"{}", None, &events, 1_000 + window + 1).is_none());
    assert!(render_effort(b"{}", None, &events, 1_000 + window)
        .unwrap()
        .contains("agents: 1\u{d7}max"));
}

#[test]
fn gateway_redaction_statistics_have_an_explicit_empty_status_slot() {
    let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let writer = writer_in(dir.path(), 293);
    let value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(writer.path()).unwrap()).unwrap();
    assert_eq!(value.get("redaction"), Some(&serde_json::Value::Null));
}

#[test]
fn delayed_redaction_snapshots_cannot_decrease_counters() {
    let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let writer = writer_in(dir.path(), 294);
    for count in [3, 1] {
        writer.publish_redaction(crate::secret_redaction::reversible::Counts {
            kinds: std::collections::BTreeMap::from([("vault".to_string(), count)]),
            unknown_tokens: count,
        });
    }
    let value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(writer.path()).unwrap()).unwrap();
    assert_eq!(value["redaction"]["kinds"]["vault"], 3);
    assert_eq!(value["redaction"]["unknown_tokens"], 3);
}

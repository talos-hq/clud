use std::time::Instant;

use super::*;

/// stdout of a real `voxtype 1.1.1 transcribe` run on speech_hello.wav
/// (parakeet, exit 0), with some log lines removed.
const REAL_STDOUT: &str = "Loading audio file: \"speech_hello.wav\"\n\
Audio format: 16000 Hz, 1 channel(s), Int\n\
Processing 16454 samples (1.03s)...\n\
\x1b[2m2026-11-07T01:53:29.793888Z\x1b[0m \x1b[32m INFO\x1b[0m Using Energy VAD backend\n\
VAD: 0.74s speech (71.2% of audio)\n\
\x1b[2m2026-11-07T01:53:34.517530Z\x1b[0m \x1b[32m INFO\x1b[0m Parakeet Tdt transcription completed in 0.51s: \"Hello world.\"\n\
\n\
Hello world.\n";

#[test]
fn the_real_voxtype_stdout_yields_only_the_transcript() {
    assert_eq!(
        parse_transcript(REAL_STDOUT).as_deref(),
        Some("Hello world.")
    );
}

#[test]
fn no_speech_and_log_only_output_have_no_transcript() {
    let no_speech = "Loading audio file: \"a.wav\"\nVAD: 0.00s speech (0.0% of audio)\n\
                     No speech detected, skipping transcription.\n";
    assert_eq!(parse_transcript(no_speech), None);
    assert_eq!(parse_transcript("\n\n"), None);
    assert_eq!(parse_transcript("unrecognized output\n"), None);
}

#[test]
fn a_multi_line_transcript_stays_on_one_line() {
    // A newline typed into the PTY would submit the prompt.
    assert_eq!(
        parse_transcript("VAD: 1s\n\nfirst part\nsecond part\n").as_deref(),
        Some("first part second part")
    );
}

#[test]
fn a_transcript_from_a_crashed_run_is_discarded() {
    let err = interpret(
        "voxtype",
        134,
        REAL_STDOUT,
        "corrupted double-linked list\n",
    )
    .unwrap_err();
    assert!(err.contains("crashed (signal 6, exit code 134)"), "{err}");
    assert!(err.contains("discarded"), "{err}");
    assert!(err.contains("corrupted double-linked list"), "{err}");
}

#[test]
fn crash_codes_are_distinct_from_ordinary_failures() {
    assert_eq!(crash_description(1), None);
    assert_eq!(crash_description(2), None);
    assert_eq!(crash_description(128), None);
    assert_eq!(
        crash_description(139).as_deref(),
        Some("signal 11, exit code 139")
    );
    assert_eq!(crash_description(-6).as_deref(), Some("signal 6"));
    assert_eq!(
        crash_description(0xC000_0005_u32 as i32).as_deref(),
        Some("status 0xC0000005")
    );
}

#[test]
fn an_empty_stderr_falls_back_to_the_stdout_log() {
    let stdout = "VAD: 1s\n\x1b[31mERROR\x1b[0m init failed\n";
    let err = interpret("voxtype", 1, stdout, "").unwrap_err();
    assert!(err.contains("exit code 1"), "{err}");
    assert!(err.contains("no stderr"), "{err}");
    assert!(err.contains("ERROR init failed"), "{err}");
    assert!(!err.contains('\x1b'), "{err}");
}

#[test]
fn a_stderr_tail_is_bounded() {
    let stderr = "x".repeat(10_000);
    let err = interpret("voxtype", 1, "", &stderr).unwrap_err();
    assert!(err.len() < 800, "{}", err.len());
}

#[test]
fn locating_prefers_override_then_talos_vox_then_voxtype() {
    assert_eq!(
        locate_with(None, &|name| Some(PathBuf::from(name))).unwrap(),
        PathBuf::from("talos-vox")
    );
    let voxtype_only =
        |name: &OsStr| (name == OsStr::new("voxtype")).then(|| PathBuf::from("voxtype"));
    assert_eq!(
        locate_with(None, &voxtype_only).unwrap(),
        PathBuf::from("voxtype")
    );
    assert_eq!(
        locate_with(Some(Path::new("mine")), &|name| Some(PathBuf::from(name))).unwrap(),
        PathBuf::from("mine")
    );
}

#[test]
fn a_missing_binary_says_how_to_get_it() {
    let err = locate_with(None, &|_| None).unwrap_err();
    assert!(err.contains("talos-hq/talos-vox"), "{err}");
    assert!(err.contains("CLUD_VOICE_TRANSCRIBER"), "{err}");
}

#[test]
fn a_bad_override_is_an_error_not_a_fallback() {
    let lookup = |name: &OsStr| (name == OsStr::new("voxtype")).then(|| PathBuf::from("voxtype"));
    let err = locate_with(Some(Path::new("/nope")), &lookup).unwrap_err();
    assert!(err.contains("/nope"), "{err}");
}

#[test]
fn language_goes_before_the_subcommand() {
    let argv = transcriber_argv(Path::new("vox"), Some("fr"), Path::new("a.wav"));
    assert_eq!(argv, ["vox", "--language", "fr", "transcribe", "a.wav"]);
    let argv = transcriber_argv(Path::new("vox"), None, Path::new("a.wav"));
    assert_eq!(argv, ["vox", "transcribe", "a.wav"]);
}

/// Fake transcribers are shell scripts; the pure tests above cover Windows.
fn fake(dir: &Path, body: &str) -> PathBuf {
    let path = dir.join("fake-transcriber");
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    crate::platform::shim_fs::set_mode(&path, 0o755).unwrap();
    path
}

fn run_fake(body: &str, deadline: Duration) -> Result<String, String> {
    let dir = tempfile::tempdir().unwrap();
    let script = fake(dir.path(), body);
    transcribe_with(&script, None, &[0.25; 160], deadline)
}

const LONG: Duration = Duration::from_secs(30);

#[test]
fn a_fake_transcriber_succeeds_and_receives_a_wav_that_is_then_removed() {
    if crate::platform::host::WINDOWS {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let seen = dir.path().join("seen");
    let body = format!(
        "echo \"$2\" > '{seen}'\nhead -c 4 \"$2\" >> '{seen}'\necho 'VAD: 1s'\necho\necho 'hello there'",
        seen = seen.display()
    );
    let script = fake(dir.path(), &body);
    let result = transcribe_with(&script, None, &[0.25; 160], LONG);
    assert_eq!(result.as_deref(), Ok("hello there"));
    let seen = std::fs::read_to_string(seen).unwrap();
    let (wav, magic) = seen.split_once('\n').unwrap();
    assert_eq!(magic, "RIFF");
    assert!(!Path::new(wav).exists(), "temp WAV must be removed");
}

#[test]
fn a_fake_nonzero_exit_reports_the_code_and_stderr() {
    if crate::platform::host::WINDOWS {
        return;
    }
    let err = run_fake("echo 'model missing' >&2\nexit 3", LONG).unwrap_err();
    assert!(err.contains("exit code 3"), "{err}");
    assert!(err.contains("model missing"), "{err}");
}

#[test]
fn a_fake_abort_is_reported_as_a_crash_and_its_transcript_dropped() {
    if crate::platform::host::WINDOWS {
        return;
    }
    let err = run_fake("echo\necho 'hello there'\nkill -ABRT $$", LONG).unwrap_err();
    assert!(err.contains("crashed (signal 6"), "{err}");
    assert!(err.contains("discarded"), "{err}");
}

#[test]
fn a_fake_empty_transcript_is_an_error() {
    if crate::platform::host::WINDOWS {
        return;
    }
    let err = run_fake("echo 'VAD: 0s'", LONG).unwrap_err();
    assert!(err.contains("no transcript"), "{err}");
}

#[test]
fn a_hung_transcriber_is_stopped_at_the_deadline() {
    if crate::platform::host::WINDOWS {
        return;
    }
    let started = Instant::now();
    let err = run_fake("exec sleep 60", Duration::from_millis(300)).unwrap_err();
    assert!(err.contains("did not finish"), "{err}");
    assert!(started.elapsed() < Duration::from_secs(20));
}

#[test]
fn a_missing_binary_path_fails_to_start() {
    let err = transcribe_with(Path::new("/nonexistent/vox"), None, &[0.0], LONG).unwrap_err();
    assert!(err.contains("/nonexistent/vox"), "{err}");
}

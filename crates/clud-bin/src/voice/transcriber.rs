//! Speech-to-text by delegating to the external `talos-vox` / `voxtype` CLI
//! (DD-206). clud links no ML runtime; this module finds the binary, hands it
//! a temporary WAV and turns what comes back into a transcript or an error.
//!
//! The transcript goes straight into the user's PTY, so a run only yields text
//! when it exited 0. voxtype can print a correct transcript and then abort in
//! native teardown; that output is discarded and the error says so, because
//! nothing distinguishes it from a truncated result.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use running_process::{
    NativeProcess, ProcessConfig, ProcessError, StderrMode, StdinMode, StreamKind,
};

use super::audio::TARGET_SAMPLE_RATE;
use super::config::VoiceConfig;
use super::wav;

const CANDIDATES: [&str; 2] = ["talos-vox", "voxtype"];
const DEADLINE: Duration = Duration::from_secs(120);
const TAIL_LINES: usize = 5;
const TAIL_CHARS: usize = 600;
/// voxtype's own progress lines (`src/app/transcribe_file.rs`), which share
/// stdout with the transcript.
const STATUS_PREFIXES: [&str; 6] = [
    "Loading audio file:",
    "Audio format:",
    "Resampling from ",
    "Processing ",
    "VAD: ",
    "No speech detected",
];

pub(super) fn locate(config: &VoiceConfig) -> Result<PathBuf, String> {
    locate_with(config.transcriber.as_deref(), &|name| {
        which::which(name).ok()
    })
}

fn locate_with(
    override_path: Option<&Path>,
    lookup: &dyn Fn(&OsStr) -> Option<PathBuf>,
) -> Result<PathBuf, String> {
    if let Some(path) = override_path {
        return lookup(path.as_os_str()).ok_or_else(|| {
            format!(
                "CLUD_VOICE_TRANSCRIBER is set to {} but no executable is there; \
                 fix or unset it",
                path.display()
            )
        });
    }
    CANDIDATES
        .iter()
        .find_map(|name| lookup(OsStr::new(name)))
        .ok_or_else(not_installed_message)
}

fn not_installed_message() -> String {
    "voice transcription needs the talos-vox or voxtype CLI, and neither is on PATH. \
     talos-vox has no releases yet: build it from https://github.com/talos-hq/talos-vox, \
     install voxtype from your package manager, or set CLUD_VOICE_TRANSCRIBER to the binary."
        .to_string()
}

pub(super) fn transcribe(config: &VoiceConfig, audio: &[f32]) -> Result<String, String> {
    let binary = locate(config)?;
    transcribe_with(&binary, config.language.as_deref(), audio, DEADLINE)
}

fn transcribe_with(
    binary: &Path,
    language: Option<&str>,
    audio: &[f32],
    deadline: Duration,
) -> Result<String, String> {
    // NamedTempFile gives a unique name and removes it on drop, on every path.
    let wav_file = tempfile::Builder::new()
        .prefix("clud-voice-")
        .suffix(".wav")
        .tempfile()
        .map_err(|error| format!("could not create a temporary WAV: {error}"))?;
    std::fs::write(wav_file.path(), wav::encode(audio, TARGET_SAMPLE_RATE))
        .map_err(|error| format!("could not write the temporary WAV: {error}"))?;

    let argv = transcriber_argv(binary, language, wav_file.path());
    let process = NativeProcess::new(ProcessConfig {
        command: crate::subprocess::command_spec_for_subprocess(argv),
        cwd: None,
        env: None,
        capture: true,
        stderr_mode: StderrMode::Pipe,
        creationflags: crate::win_creation_flags::invisible_helper_creationflags(),
        create_process_group: false,
        stdin_mode: StdinMode::Null,
        nice: None,
        address_space_limit_bytes: None,
    });
    let name = binary.display();
    process
        .start()
        .map_err(|error| format!("could not start {name}: {error}"))?;
    let code = match process.wait(Some(deadline)) {
        Ok(code) => code,
        Err(ProcessError::Timeout) => {
            let _ = process.kill();
            return Err(format!(
                "{name} did not finish within {}s and was stopped",
                deadline.as_secs()
            ));
        }
        Err(error) => {
            let _ = process.kill();
            return Err(format!("waiting for {name}: {error}"));
        }
    };
    let stdout = String::from_utf8_lossy(&process.drain_stream_raw(StreamKind::Stdout)).to_string();
    let stderr = String::from_utf8_lossy(&process.drain_stream_raw(StreamKind::Stderr)).to_string();
    interpret(&name.to_string(), code, &stdout, &stderr)
}

/// `--language` is a top-level voxtype option: clap refuses it after
/// `transcribe`. voxtype applies it to the Whisper engine only.
fn transcriber_argv(binary: &Path, language: Option<&str>, wav: &Path) -> Vec<String> {
    let mut argv = vec![binary.to_string_lossy().to_string()];
    if let Some(language) = language {
        argv.extend(["--language".to_string(), language.to_string()]);
    }
    argv.push("transcribe".to_string());
    argv.push(wav.to_string_lossy().to_string());
    argv
}

fn interpret(name: &str, code: i32, stdout: &str, stderr: &str) -> Result<String, String> {
    let transcript = parse_transcript(stdout);
    if code == 0 {
        return transcript.ok_or_else(|| {
            format!("{name} returned no transcript; no speech was detected in the recording")
        });
    }
    let mut message = match crash_description(code) {
        Some(crash) => format!("{name} crashed ({crash})"),
        None => format!("{name} failed with exit code {code}"),
    };
    if transcript.is_some() {
        message.push_str("; it printed a transcript first, which was discarded");
    }
    message.push_str(&failure_tail(stdout, stderr));
    Err(message)
}

/// Exit codes that mean a signal or abort rather than a reported failure: a
/// negative code (signal number, or a Windows NTSTATUS such as 0xC0000005) or
/// a shell-style 128+N.
fn crash_description(code: i32) -> Option<String> {
    match code {
        -64..=-1 => Some(format!("signal {}", -code)),
        129..=192 => Some(format!("signal {}, exit code {code}", code - 128)),
        i32::MIN..=-65 => Some(format!("status {:#010X}", code as u32)),
        _ => None,
    }
}

/// The last lines of stderr. voxtype logs its errors to stdout and can exit
/// with stderr empty, so fall back to the last stdout log lines in that case.
fn failure_tail(stdout: &str, stderr: &str) -> String {
    let stderr = crate::ci_local::strip_ansi(stderr);
    let stdout = crate::ci_local::strip_ansi(stdout);
    let (label, text) = if stderr.trim().is_empty() {
        ("no stderr; stdout log", stdout.as_str())
    } else {
        ("stderr", stderr.as_str())
    };
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.is_empty() {
        return "; it printed no output".to_string();
    }
    let joined = lines[lines.len().saturating_sub(TAIL_LINES)..].join(" | ");
    let chars: Vec<char> = joined.chars().collect();
    let tail: String = chars[chars.len().saturating_sub(TAIL_CHARS)..]
        .iter()
        .collect();
    format!(" [{label}: {tail}]")
}

/// voxtype writes `"\n{transcript}\n"` as its final stdout, after progress
/// lines and tracing logs that also land on stdout with ANSI colors. The
/// transcript is therefore the last block preceded by a blank line once log
/// lines are removed. Output without that blank line has no transcript: text
/// we cannot identify must never be typed into the PTY.
fn parse_transcript(stdout: &str) -> Option<String> {
    let cleaned = crate::ci_local::strip_ansi(stdout);
    let lines: Vec<&str> = cleaned
        .lines()
        .map(str::trim)
        .filter(|line| !is_log_line(line))
        .collect();
    let end = lines.iter().rposition(|line| !line.is_empty())? + 1;
    let start = lines[..end].iter().rposition(|line| line.is_empty())? + 1;
    Some(lines[start..end].join(" "))
}

fn is_log_line(line: &str) -> bool {
    STATUS_PREFIXES.iter().any(|p| line.starts_with(p)) || is_tracing_line(line)
}

/// `2026-11-07T01:53:29.793888Z  INFO ...`
fn is_tracing_line(line: &str) -> bool {
    let bytes = line.as_bytes();
    bytes.len() > 20
        && bytes[..4].iter().all(u8::is_ascii_digit)
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[10] == b'T'
        && bytes[13] == b':'
}

#[cfg(test)]
#[path = "transcriber_tests.rs"]
mod tests;

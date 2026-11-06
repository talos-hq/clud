//! Sequential A/B measurements. Invalid samples never enter successful counts.
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::Duration;

use clap::Args;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::subprocess::{capture_separate, CapturedOutput};

#[derive(Args, Debug, Clone)]
pub struct AbArgs {
    #[arg(long, value_delimiter = ',', required = true)]
    pub backends: Vec<String>,
    #[arg(long)]
    pub prompt_file: PathBuf,
    #[arg(long, default_value = "1", value_parser = clap::value_parser!(u32).range(1..))]
    pub runs: u32,
    #[arg(long, value_delimiter = ',')]
    pub model_map: Vec<String>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Serialize)]
struct Sample {
    backend: String,
    run: u32,
    model: String,
    exit: i32,
    ms: f64,
    stdout_bytes: usize,
    stderr_bytes: usize,
    stdout_sha256: String,
    valid: bool,
    error: Option<String>,
}

impl Sample {
    fn from_output(backend: &str, run: u32, model: &str, output: CapturedOutput) -> Self {
        Self {
            backend: backend.into(),
            run,
            model: model.into(),
            exit: output.exit,
            ms: output.elapsed.as_secs_f64() * 1000.0,
            stdout_bytes: output.stdout.len(),
            stderr_bytes: output.stderr.len(),
            stdout_sha256: format!("{:x}", Sha256::digest(&output.stdout)),
            valid: output.exit == 0 && !output.stdout.is_empty() && output.error.is_none(),
            error: output.error,
        }
    }
}

fn models(options: &AbArgs) -> Result<HashMap<String, String>, String> {
    let mut models = HashMap::from([
        ("claude".into(), "claude-sonnet".into()),
        ("codex".into(), "gpt-6.1-sol".into()),
    ]);
    let mut seen = HashSet::new();
    for backend in &options.backends {
        if !models.contains_key(backend) || !seen.insert(backend) {
            return Err(format!("unsupported or duplicate backend: {backend}"));
        }
    }
    let mut overrides = HashSet::new();
    for mapping in &options.model_map {
        let (backend, model) = mapping
            .split_once('=')
            .ok_or("model-map requires backend=model")?;
        if !models.contains_key(backend) || model.trim().is_empty() || !overrides.insert(backend) {
            return Err(format!("invalid or duplicate model-map: {mapping}"));
        }
        models.insert(backend.into(), model.into());
    }
    Ok(models)
}

fn argv(executable: &str, backend: &str, model: &str, prompt: &str) -> Vec<String> {
    let mut args = vec![executable.into()];
    if backend == "codex" {
        args.extend(["--provider".into(), "codex".into()]);
    }
    args.extend(["--model".into(), model.into(), "-p".into(), prompt.into()]);
    args
}

fn measure(options: &AbArgs) -> Result<Vec<Sample>, String> {
    let models = models(options)?;
    let prompt =
        std::fs::read_to_string(&options.prompt_file).map_err(|error| error.to_string())?;
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    let executable = executable
        .to_str()
        .ok_or("clud executable path is not UTF-8")?;
    let mut samples = Vec::new();
    for backend in &options.backends {
        let model = &models[backend];
        for run in 1..=options.runs {
            let started = std::time::Instant::now();
            let output = capture_separate(
                argv(executable, backend, model, &prompt),
                Duration::from_secs(600),
            )
            .unwrap_or_else(|error| CapturedOutput {
                exit: -1,
                elapsed: started.elapsed(),
                stdout: Vec::new(),
                stderr: Vec::new(),
                error: Some(error),
            });
            samples.push(Sample::from_output(backend, run, model, output));
        }
    }
    Ok(samples)
}

fn table(samples: &[Sample]) -> String {
    let mut text =
        String::from("BACKEND RUN MODEL STATUS EXIT MS STDOUT_BYTES STDERR_BYTES STDOUT_SHA256\n");
    for sample in samples {
        let _ = writeln!(
            text,
            "{} {} {} {} {} {:.3} {} {} {}",
            sample.backend,
            sample.run,
            sample.model,
            if sample.valid { "VALID" } else { "INVALID" },
            sample.exit,
            sample.ms,
            sample.stdout_bytes,
            sample.stderr_bytes,
            sample.stdout_sha256
        );
    }
    let valid = samples.iter().filter(|sample| sample.valid).count();
    let _ = writeln!(
        text,
        "Valid: {valid}/{} (invalid samples excluded)",
        samples.len()
    );
    text
}

pub fn run(options: &AbArgs) -> i32 {
    match measure(options) {
        Ok(samples) => {
            if options.json {
                println!(
                    "{}",
                    serde_json::to_string(&samples).expect("samples serialize")
                );
            } else {
                print!("{}", table(&samples));
            }
            for sample in &samples {
                if let Some(error) = &sample.error {
                    eprintln!("ab {} run {}: {error}", sample.backend, sample.run);
                }
            }
            i32::from(samples.iter().any(|sample| !sample.valid))
        }
        Err(error) => {
            eprintln!("ab: {error}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(exit: i32, stdout: &[u8], stderr: &[u8]) -> Sample {
        Sample::from_output(
            "claude",
            1,
            "claude-sonnet",
            CapturedOutput {
                exit,
                elapsed: Duration::from_micros(12345),
                stdout: stdout.into(),
                stderr: stderr.into(),
                error: None,
            },
        )
    }

    #[test]
    fn ab_invalid_samples() {
        assert!(!sample(1, b"answer", b"error").valid);
        assert!(!sample(0, b"", b"version skew").valid);
        assert!(!sample(0, b"", b"").valid);
    }

    #[test]
    fn ab_valid_with_stderr_and_byte_timing_capture() {
        let result = sample(0, "é".as_bytes(), b"diagnostics");
        assert!(result.valid);
        assert_eq!((result.stdout_bytes, result.stderr_bytes), (2, 11));
        assert!((result.ms - 12.345).abs() < 0.000001);
        assert_eq!(
            sample(0, b"abc", b"").stdout_sha256,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn ab_table_renderer_excludes_invalid_from_count() {
        let rendered = table(&[
            sample(0, b"secret answer", b"diagnostics"),
            sample(0, b"", b"skew"),
        ]);
        assert!(rendered.contains("claude 1 claude-sonnet VALID 0 12.345 13 11"));
        assert!(rendered.contains("INVALID 0 12.345 0 4"));
        assert!(rendered.contains("Valid: 1/2"));
        assert!(!rendered.contains("secret answer"));
    }

    #[test]
    fn ab_cli_wiring_and_model_overrides() {
        let args = crate::args::Args::parse_from_raw(
            vec![
                "clud",
                "ab",
                "--backends",
                "claude,codex",
                "--prompt-file",
                "prompt.md",
                "--runs",
                "2",
                "--model-map",
                "claude=x,codex=y",
                "--json",
            ]
            .into_iter()
            .map(String::from)
            .collect(),
        );
        let Some(crate::args::Command::Ab(options)) = args.command else {
            panic!("not ab")
        };
        assert_eq!(models(&options).unwrap()["codex"], "y");
        assert_eq!(options.runs, 2);
        assert!(options.json);
        assert_eq!(
            argv("clud", "codex", "y", "hello"),
            ["clud", "--provider", "codex", "--model", "y", "-p", "hello"]
        );
        assert!(crate::args::Args::try_parse_from([
            "clud",
            "ab",
            "--backends",
            "claude",
            "--prompt-file",
            "f",
            "--runs",
            "0"
        ])
        .is_err());
    }

    #[cfg(unix)]
    #[test]
    fn ab_subprocess_captures_raw_streams_and_wall_clock() {
        let output = capture_separate(
            vec![
                "sh".into(),
                "-c".into(),
                "printf 'abc'; printf 'diagnostics' >&2; sleep 0.02; exit 7".into(),
            ],
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(output.stdout, b"abc");
        assert_eq!(output.stderr, b"diagnostics");
        assert_eq!(output.exit, 7);
        assert!(output.elapsed >= Duration::from_millis(20));
        assert!(!Sample::from_output("claude", 1, "x", output).valid);
    }
}

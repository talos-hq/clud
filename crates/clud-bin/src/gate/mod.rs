//! Compact delegated lint/test gate (#2601). Tool output belongs in logs, never stdout.
use running_process::ReadStatus;
use serde_json::{json, Value};
use std::{
    fs::{self, File},
    io::{self, Write},
    path::Path,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
pub enum Lane {
    Lint,
    Test,
    Both,
}
impl Lane {
    fn name(self) -> &'static str {
        match self {
            Self::Lint => "lint",
            Self::Test => "test",
            Self::Both => "both",
        }
    }
}

#[derive(Debug)]
struct Element {
    id: &'static str,
    kind: &'static str,
    argv: Vec<String>,
}
fn element(id: &'static str, kind: &'static str, argv: &[&str]) -> Element {
    Element {
        id,
        kind,
        argv: argv.iter().map(|s| (*s).to_owned()).collect(),
    }
}

fn detect(repo: &Path) -> Vec<Element> {
    let mut items = Vec::new();
    if repo.join("Cargo.toml").is_file() {
        items.push(element(
            "fmt",
            "lint",
            &["soldr", "cargo", "fmt", "--all", "--", "--check"],
        ));
        items.push(element(
            "clippy",
            "lint",
            &[
                "soldr",
                "cargo",
                "clippy",
                "--workspace",
                "--all-targets",
                "--",
                "-D",
                "warnings",
            ],
        ));
        items.push(if which::which("cargo-nextest").is_ok() {
            element(
                "nextest",
                "test",
                &["soldr", "cargo", "nextest", "run", "--workspace"],
            )
        } else {
            element(
                "cargo-test",
                "test",
                &["soldr", "cargo", "test", "--workspace", "--quiet"],
            )
        });
    }
    if repo.join("pyproject.toml").is_file() {
        items.push(element("ruff", "lint", &["ruff", "check", "."]));
        items.push(element("pytest", "test", &["pytest", "-q"]));
    }
    if let Ok(text) = fs::read_to_string(repo.join("package.json")) {
        if let Ok(package) = serde_json::from_str::<Value>(&text) {
            for (id, kind, args) in [
                ("eslint", "lint", vec!["."]),
                ("vitest", "test", vec!["run"]),
                ("jest", "test", vec!["--runInBand"]),
            ] {
                if ["dependencies", "devDependencies"]
                    .iter()
                    .any(|key| package[key].get(id).is_some())
                {
                    let local = repo.join("node_modules/.bin");
                    let program = which::which_in(id, Some(local.as_os_str()), repo).map_or_else(
                        |_| id.to_owned(),
                        |path| path.to_string_lossy().into_owned(),
                    );
                    let mut item = element(id, kind, &args);
                    item.argv.insert(0, program);
                    items.push(item);
                }
            }
        }
    }
    // Complete the lint lane before running any tests, including mixed-language repos.
    items.sort_by_key(|item| item.kind != "lint");
    items
}

/// Rank all candidates, so log headers and early pointers cannot hide a compiler error.
fn first_error(text: &str) -> Option<String> {
    let patterns = [
        r"^error\[E\d+\]",
        r"^error:",
        r"^-->\s+.+:\d+:\d+",
        r"^(?:[A-Za-z]:)?[^\s:]+\.[A-Za-z0-9]+:\d+:\d+(?:\s|:|$)",
        r"^(?:[A-Za-z]:)?[^\s:]+\.py:\d+:\s+.+",
    ];
    for pattern in patterns {
        let re = regex::Regex::new(pattern).expect("constant diagnostic regex");
        if let Some(line) = text.lines().map(str::trim).find(|line| re.is_match(line)) {
            return Some(line.chars().take(240).collect());
        }
    }
    None
}

fn execute(item: &Element, repo: &Path, log: &Path) -> io::Result<(i32, String)> {
    let mut file = File::create(log)?;
    let result = crate::subprocess::ManagedSubprocess::start_inheriting_env(
        item.argv.clone(),
        Some(repo.to_owned()),
        true,
        None,
    );
    let process = match result {
        Ok(process) => process,
        Err(error) => {
            writeln!(file, "{error}")?;
            return Ok((1, error));
        }
    };
    let mut output = String::new();
    loop {
        match process.read_stdout(Some(std::time::Duration::from_millis(100))) {
            ReadStatus::Line(bytes) => {
                file.write_all(&bytes)?;
                output.push_str(&String::from_utf8_lossy(&bytes));
            }
            ReadStatus::Eof => break,
            ReadStatus::Timeout => {
                process.poll().map_err(io::Error::other)?;
            }
        }
    }
    let code = process.wait(None).map_err(io::Error::other)?;
    Ok((code, output))
}

fn evaluate(
    lane: Lane,
    no_fast_fail: bool,
    repo: &Path,
    log_dir: &Path,
    run_id: &str,
    selected: &[Element],
) -> io::Result<Value> {
    fs::create_dir_all(log_dir)?;
    let started = Instant::now();
    let fast_fail = matches!(lane, Lane::Both) && !no_fast_fail;
    let mut elements = Vec::new();
    let mut passed = Vec::new();
    let mut skipped = Vec::new();
    let mut failed = false;
    for item in selected
        .iter()
        .filter(|item| matches!(lane, Lane::Both) || item.kind == lane.name())
    {
        if failed && fast_fail {
            skipped.push(json!({"id": item.id, "reason": "fast_fail: previous element failed"}));
            continue;
        }
        let log_path = log_dir.join(format!("{}.log", item.id));
        let begin = Instant::now();
        let (code, output) = execute(item, repo, &log_path)?;
        let mut record = json!({"id": item.id, "kind": item.kind, "status": if code == 0 { "pass" } else { "fail" }, "ms": begin.elapsed().as_millis(), "log_path": log_path});
        if code == 0 {
            passed.push(item.id);
        } else {
            failed = true;
            if let Some(error) = first_error(&output) {
                record["first_error"] = json!(error);
            }
        }
        elements.push(record);
    }
    if !failed {
        elements.clear();
    }
    Ok(
        json!({"lane": lane.name(), "fast_fail": fast_fail, "run_id": run_id, "status": if failed { "fail" } else { "pass" }, "duration_ms": started.elapsed().as_millis(), "elements": elements, "passed_elements": passed, "skipped": skipped}),
    )
}

pub fn run(lane: Lane, repo: &Path, no_fast_fail: bool) -> i32 {
    let result = (|| {
        let repo = repo.canonicalize()?;
        let home = crate::home::user_home()
            .ok_or_else(|| io::Error::other("home directory unavailable"))?;
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_nanos();
        let run_id = format!("{stamp}-{}", std::process::id());
        evaluate(
            lane,
            no_fast_fail,
            &repo,
            &home.join(".clud/gate").join(&run_id),
            &run_id,
            &detect(&repo),
        )
    })();
    match result {
        Ok(payload) => {
            println!("{payload}");
            i32::from(payload["status"] == "fail")
        }
        Err(error) => {
            eprintln!("clud gate: {error}");
            2
        }
    }
}

#[cfg(test)]
mod tests;

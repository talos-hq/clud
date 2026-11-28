//! `clud test run` / `clud test stats` (#407).
//!
//! v1 is deliberately the dumbest useful surface: the wrapper records what a
//! command cost, and `stats` reports it. No bucket heuristics, no CPU
//! normalization formula, no run-all-vs-targeted threshold — those are #405
//! Q2/Q3/Q6 and are explicitly deferred. The *agent* makes the decision; this
//! reports the facts it needs.

use std::path::{Path, PathBuf};

use running_process::{CommandSpec, NativeProcess, ProcessConfig, StderrMode, StdinMode};

use super::store::{self, BucketStats, RunRecord};

/// Locate the repo root by walking up for a `.git`, so `clud test` works from
/// a subdirectory the way every other repo-scoped tool does.
pub fn repo_root_from(start: &Path) -> PathBuf {
    let mut cursor = Some(start);
    while let Some(dir) = cursor {
        if dir.join(".git").exists() {
            return dir.to_path_buf();
        }
        cursor = dir.parent();
    }
    start.to_path_buf()
}

/// Sample total CPU utilization percent, for the record.
///
/// Two refreshes separated by `sysinfo`'s minimum interval: the first
/// establishes a baseline, and a single refresh would report 0 on every
/// platform. The design stores this raw and normalizes at query time
/// (#405 Q3), so a slightly noisy sample is acceptable — what matters is that
/// it is *recorded* rather than discarded.
fn sample_cpu_pct() -> u8 {
    let mut system = sysinfo::System::new();
    system.refresh_cpu_usage();
    std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
    system.refresh_cpu_usage();
    let usage = system.global_cpu_usage();
    usage.clamp(0.0, 100.0).round() as u8
}

/// `clud test run --bucket <b> [--target <t>] -- <command…>`.
///
/// Returns the child's exit code **unchanged**. That is what makes the wrapper
/// safe to adopt: anything running tests today keeps working when it is
/// prefixed on, and a recording failure never turns a green run red.
pub fn run(bucket: &str, target: Option<String>, command: &[String]) -> i32 {
    if command.is_empty() {
        eprintln!("clud test run: no command given (use `-- <command…>`)");
        return 2;
    }

    let cpu = sample_cpu_pct();
    let started = std::time::Instant::now();

    let process = NativeProcess::new(ProcessConfig {
        command: CommandSpec::Argv(command.to_vec()),
        cwd: None,
        env: None,
        // Inherit stdio: the point is to wrap a test run, not to swallow its
        // output. A wrapper that hid test output would not be used twice.
        capture: false,
        stderr_mode: StderrMode::Stdout,
        creationflags: None,
        create_process_group: false,
        stdin_mode: StdinMode::Inherit,
        nice: None,
        address_space_limit_bytes: None,
    });
    if process.start().is_err() {
        eprintln!("clud test run: failed to start {:?}", command[0]);
        return 127;
    }
    let exit_code = process.wait(None).unwrap_or(1);
    let elapsed_ms = started.elapsed().as_millis() as u64;

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let root = repo_root_from(&cwd);
    store::record(
        &root,
        &RunRecord::new(bucket, target, elapsed_ms, cpu, exit_code),
    );

    eprintln!(
        "[clud] test {}: {} (cpu {}% at start, exit {})",
        store::normalize_bucket(bucket),
        store::human_ms(elapsed_ms),
        cpu,
        exit_code
    );
    exit_code
}

/// `clud test stats [--bucket <b>] [--json]`.
pub fn stats(bucket: Option<&str>, json: bool) -> i32 {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let root = repo_root_from(&cwd);
    let records = store::load(&root);
    let mut summary = store::summarize(&records);
    if let Some(bucket) = bucket {
        let wanted = store::normalize_bucket(bucket);
        summary.retain(|entry| entry.bucket == wanted);
    }

    if json {
        let body = serde_json::to_string_pretty(&summary).unwrap_or_else(|_| "[]".into());
        println!("{body}");
        return 0;
    }

    if summary.is_empty() {
        println!("no test-runtime history yet — run `clud test run --bucket <b> -- <cmd>`");
        return 0;
    }
    for line in format_summary(&summary) {
        println!("{line}");
    }
    0
}

/// Render the human table.
///
/// Pure so the output contract is testable. The trailing hint is deliberately
/// descriptive rather than prescriptive — v1 does not own the decision policy
/// (#405 Q6), so it reports cost and lets the agent weigh it against blast
/// radius, which only the agent knows.
pub fn format_summary(summary: &[BucketStats]) -> Vec<String> {
    summary
        .iter()
        .map(|entry| {
            let hint = if entry.n < 5 {
                "little history, prefer run-all"
            } else if entry.p90_ms <= 30_000 {
                "cheap"
            } else {
                "expensive"
            };
            format!(
                "{:<12} p50={:<9} p90={:<9} n={:<4} → {hint}",
                entry.bucket,
                store::human_ms(entry.p50_ms),
                store::human_ms(entry.p90_ms),
                entry.n,
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use kernal_api::platform::fs::TemporaryDirectory;

    fn stats_of(bucket: &str, n: usize, p50: u64, p90: u64) -> BucketStats {
        BucketStats {
            bucket: bucket.to_string(),
            n,
            p50_ms: p50,
            p90_ms: p90,
        }
    }

    #[test]
    fn the_repo_root_is_found_from_a_subdirectory() {
        let tmp = TemporaryDirectory::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let nested = root.join("crates").join("clud-bin").join("src");
        std::fs::create_dir_all(&nested).unwrap();

        assert_eq!(repo_root_from(&nested), root);
    }

    /// Outside a repo the walk must terminate rather than climbing to `/` and
    /// writing a store in someone's home directory.
    #[test]
    fn a_path_with_no_git_root_falls_back_to_itself() {
        let tmp = TemporaryDirectory::new().unwrap();
        assert_eq!(repo_root_from(tmp.path()), tmp.path());
    }

    #[test]
    fn the_summary_marks_cheap_and_expensive_buckets() {
        let lines = format_summary(&[
            stats_of("unit", 43, 1_900, 6_400),
            stats_of("integration", 12, 232_000, 250_000),
        ]);
        assert!(lines[0].contains("unit"), "{lines:?}");
        assert!(lines[0].contains("cheap"), "{lines:?}");
        assert!(lines[1].contains("expensive"), "{lines:?}");
        assert!(lines[1].contains("3m52s"), "{lines:?}");
    }

    /// #405's cold-start rule: below five samples, percentiles are noise and
    /// the honest answer is "prefer run-all" regardless of how fast it looks.
    #[test]
    fn a_thin_sample_reports_little_history_even_when_fast() {
        let lines = format_summary(&[stats_of("e2e", 2, 500, 900)]);
        assert!(lines[0].contains("little history"), "{lines:?}");
        assert!(!lines[0].contains("cheap"), "{lines:?}");
    }

    #[test]
    fn an_empty_summary_renders_nothing_rather_than_a_header() {
        assert!(format_summary(&[]).is_empty());
    }
}

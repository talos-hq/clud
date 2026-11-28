//! Append-only test-runtime store (#407, per the #405 design).
//!
//! # Storage deviation from #407's text
//!
//! #407 specifies SQLite via `rusqlite`, and its acceptance criteria allow a
//! deviation "with one-line rationale". The rationale: **`rusqlite` is not a
//! dependency of this repo and was removed on purpose** — `Cargo.toml` records
//! the swap to `redb` "to cut cold-build time and drop the C-toolchain pressure
//! on CI". The accepted design
//! ([`docs/architecture/test-runtime-memory.md`](../../../../docs/architecture/test-runtime-memory.md))
//! works through the alternatives and picks append-only JSONL: redb takes an
//! exclusive per-process file lock, which is the problem DD-006 needed a daemon
//! and an advisory lockfile to work around, and two `clud test` invocations from
//! two shells is an ordinary case here.
//!
//! #407's own note applies unchanged — "this PR's scope shrinks but the
//! read/write/GC interfaces stay the same."
//!
//! # Why a lost sample is not an error
//!
//! Every fallible path here degrades to "no data" rather than propagating.
//! This is a histogram feeding an advisory recommendation: one skipped line out
//! of hundreds changes no decision, whereas a tool that can fail a test run to
//! record a statistic *about* the test run gets removed.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Schema version. A reader skips rows whose version it does not know rather
/// than misreading them; there is no migration script, because the file is a
/// cache and the correct response to an unreadable one is to start over.
pub const SCHEMA_VERSION: u32 = 1;

/// Rows retained per bucket after compaction (#407: "keep last 500 rows per
/// bucket").
pub const MAX_ROWS_PER_BUCKET: usize = 500;

/// Rows the read path considers (#407: "last 200 rows per bucket").
pub const STATS_WINDOW: usize = 200;

/// Buckets are a closed set; anything unrecognized lands in `unknown` rather
/// than being dropped, so a growing `unknown` count is a visible signal.
pub const BUCKETS: &[&str] = &["unit", "integration", "e2e", "smoke", "unknown"];

pub fn normalize_bucket(raw: &str) -> String {
    let lowered = raw.trim().to_ascii_lowercase();
    if BUCKETS.contains(&lowered.as_str()) {
        lowered
    } else {
        "unknown".to_string()
    }
}

/// One observation. Field names are short because they repeat every row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunRecord {
    pub v: u32,
    pub b: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub t: Option<String>,
    pub ms: u64,
    pub cpu: u8,
    pub at: u64,
    pub rc: i32,
    pub os: String,
}

impl RunRecord {
    pub fn new(bucket: &str, target: Option<String>, ms: u64, cpu: u8, rc: i32) -> Self {
        Self {
            v: SCHEMA_VERSION,
            b: normalize_bucket(bucket),
            t: target,
            ms,
            cpu: cpu.min(100),
            at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            rc,
            os: host_os().to_string(),
        }
    }
}

pub fn host_os() -> &'static str {
    if crate::platform::host::WINDOWS {
        "windows"
    } else if crate::platform::host::MACOS {
        "macos"
    } else {
        "linux"
    }
}

/// `<repo>/.clud/test-runtime/runs.jsonl`.
pub fn store_path(repo_root: &Path) -> PathBuf {
    repo_root
        .join(".clud")
        .join("test-runtime")
        .join("runs.jsonl")
}

/// Append one record, then compact if the file has grown past the retention
/// bound. Best-effort: an unwritable `.clud/` is silently accepted.
pub fn record(repo_root: &Path, run: &RunRecord) {
    let path = store_path(repo_root);
    if let Some(parent) = path.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
    }
    let Ok(line) = serde_json::to_string(run) else {
        return;
    };
    // `append(true)` plus **one** `write_all` is what makes concurrent writers
    // safe without a lock: a ~120-byte record is far below the size at which
    // append atomicity stops holding, so two `clud test` invocations interleave
    // whole records rather than corrupting one.
    //
    // The newline must be part of that single write. `writeln!` issues the
    // content and the newline as *separate* writes, which lets another appender
    // land between them and merges two records into one unparseable line —
    // measured at 43 of 100 records surviving with four concurrent writers.
    let mut payload = line;
    payload.push('\n');
    let appended = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut file| file.write_all(payload.as_bytes()));
    if appended.is_err() {
        return;
    }
    compact_if_needed(&path);
}

/// Read every parseable record. Unparseable lines and unknown schema versions
/// are skipped — see the module note on why that is not an error.
pub fn load(repo_root: &Path) -> Vec<RunRecord> {
    let Ok(text) = std::fs::read_to_string(store_path(repo_root)) else {
        return Vec::new();
    };
    parse_records(&text)
}

pub fn parse_records(text: &str) -> Vec<RunRecord> {
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<RunRecord>(line).ok())
        .filter(|run| run.v == SCHEMA_VERSION)
        .collect()
}

/// Retain the newest [`MAX_ROWS_PER_BUCKET`] per bucket.
///
/// Pure so the retention bound is asserted without touching the filesystem.
/// Input order is append order (oldest first); output preserves it, so the file
/// stays chronological and a human can still `tail` it.
pub fn compact(records: Vec<RunRecord>) -> Vec<RunRecord> {
    let mut per_bucket: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for run in &records {
        *per_bucket.entry(run.b.clone()).or_insert(0) += 1;
    }
    if per_bucket.values().all(|&n| n <= MAX_ROWS_PER_BUCKET) {
        return records;
    }
    // Walk newest-first, keep the first MAX per bucket, then restore order.
    let mut kept: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut keep_flags = vec![false; records.len()];
    for (index, run) in records.iter().enumerate().rev() {
        let seen = kept.entry(run.b.clone()).or_insert(0);
        if *seen < MAX_ROWS_PER_BUCKET {
            *seen += 1;
            keep_flags[index] = true;
        }
    }
    records
        .into_iter()
        .zip(keep_flags)
        .filter_map(|(run, keep)| keep.then_some(run))
        .collect()
}

fn compact_if_needed(path: &Path) {
    // Compaction is a read-modify-write, which appends are not — so unlike the
    // append path it *does* need exclusive access. Without it, one process can
    // read the file, another can append, and the first can then rename its
    // now-stale copy over the top, silently dropping the appended record.
    //
    // The lock is only taken once some bucket is over the bound, so the common
    // write pays a read and nothing else.
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    if compact(parse_records(&text)).len() == parse_records(&text).len() {
        return;
    }
    let lock_path = path.with_extension("jsonl.lock");
    let Ok(lock) = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
    else {
        return;
    };
    if crate::file_lock::FileExt::lock_exclusive(&lock).is_err() {
        return;
    }
    // Re-read under the lock: the file may have grown since the check above.
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    let records = parse_records(&text);
    let compacted = compact(records.clone());
    if compacted.len() == records.len() {
        return;
    }
    let mut body = String::new();
    for run in &compacted {
        if let Ok(line) = serde_json::to_string(run) {
            body.push_str(&line);
            body.push('\n');
        }
    }
    // Write a sibling and rename, so a concurrent reader sees the old file or
    // the new one, never a half-written one.
    let temp = path.with_extension(format!("jsonl.tmp{}", std::process::id()));
    if std::fs::write(&temp, body).is_ok() {
        let _ = std::fs::rename(&temp, path);
    }
    let _ = std::fs::remove_file(&temp);
}

/// Per-bucket summary over the most recent [`STATS_WINDOW`] passing runs.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BucketStats {
    pub bucket: String,
    pub n: usize,
    pub p50_ms: u64,
    pub p90_ms: u64,
}

/// Summarize, newest-window-first, passing runs only.
///
/// Failed runs are excluded because a suite that died 4 seconds in is not
/// evidence that the suite takes 4 seconds. `rc` exists for this exclusion, not
/// to build a flaky-test tracker (an explicit non-goal of #405).
pub fn summarize(records: &[RunRecord]) -> Vec<BucketStats> {
    let mut by_bucket: std::collections::BTreeMap<String, Vec<u64>> =
        std::collections::BTreeMap::new();
    for run in records.iter().rev() {
        if run.rc != 0 {
            continue;
        }
        let entry = by_bucket.entry(run.b.clone()).or_default();
        if entry.len() < STATS_WINDOW {
            entry.push(run.ms);
        }
    }
    by_bucket
        .into_iter()
        .map(|(bucket, mut samples)| {
            samples.sort_unstable();
            BucketStats {
                bucket,
                n: samples.len(),
                p50_ms: percentile(&samples, 50),
                p90_ms: percentile(&samples, 90),
            }
        })
        .collect()
}

/// Nearest-rank percentile. Exact and obvious at these sample counts;
/// interpolation would add a rounding argument for no decision-relevant gain.
pub fn percentile(sorted: &[u64], pct: u64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = ((pct * sorted.len() as u64).div_ceil(100)).max(1) as usize;
    sorted[rank.min(sorted.len()) - 1]
}

/// Human-readable duration. Test runtimes span milliseconds to many minutes, so
/// a single unit would be unreadable at one end or the other.
pub fn human_ms(ms: u64) -> String {
    if ms < 1_000 {
        return format!("{ms}ms");
    }
    let secs = ms as f64 / 1000.0;
    if secs < 60.0 {
        return format!("{secs:.1}s");
    }
    let minutes = ms / 60_000;
    let rest = (ms % 60_000) as f64 / 1000.0;
    format!("{minutes}m{rest:.0}s")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn run(bucket: &str, ms: u64, rc: i32) -> RunRecord {
        RunRecord {
            v: SCHEMA_VERSION,
            b: bucket.to_string(),
            t: None,
            ms,
            cpu: 10,
            at: 1_700_000_000,
            rc,
            os: "linux".to_string(),
        }
    }

    #[test]
    fn a_record_round_trips_through_the_store() {
        let tmp = TempDir::new().unwrap();
        let record_in = RunRecord::new("unit", Some("skills::".into()), 1913, 11, 0);
        record(tmp.path(), &record_in);

        let loaded = load(tmp.path());
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0], record_in);
        assert!(store_path(tmp.path()).is_file());
    }

    /// #407's acceptance criterion, verbatim: 600 writes to one bucket must
    /// leave exactly 500.
    #[test]
    fn six_hundred_writes_compact_to_exactly_five_hundred() {
        let tmp = TempDir::new().unwrap();
        for i in 0..600u64 {
            record(tmp.path(), &run("unit", i, 0));
        }
        let loaded = load(tmp.path());
        assert_eq!(loaded.len(), MAX_ROWS_PER_BUCKET);
        // The *newest* rows survive: retention that dropped the recent end
        // would make the histogram describe the past rather than the present.
        assert_eq!(loaded.first().unwrap().ms, 100);
        assert_eq!(loaded.last().unwrap().ms, 599);
    }

    /// Retention is per bucket, so a chatty bucket cannot evict a quiet one.
    #[test]
    fn compaction_is_per_bucket_not_global() {
        let mut records: Vec<RunRecord> = (0..600).map(|i| run("unit", i, 0)).collect();
        records.push(run("e2e", 42, 0));
        let compacted = compact(records);

        assert_eq!(compacted.iter().filter(|r| r.b == "unit").count(), 500);
        assert_eq!(
            compacted.iter().filter(|r| r.b == "e2e").count(),
            1,
            "a quiet bucket must survive a chatty neighbour"
        );
    }

    #[test]
    fn compaction_preserves_append_order() {
        let records: Vec<RunRecord> = (0..600).map(|i| run("unit", i, 0)).collect();
        let compacted = compact(records);
        let times: Vec<u64> = compacted.iter().map(|r| r.ms).collect();
        let mut sorted = times.clone();
        sorted.sort_unstable();
        assert_eq!(times, sorted, "the file must stay chronological");
    }

    #[test]
    fn stats_report_percentiles_per_bucket() {
        let records: Vec<RunRecord> = (1..=10).map(|i| run("unit", i * 1000, 0)).collect();
        let stats = summarize(&records);
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].n, 10);
        assert_eq!(stats[0].p50_ms, 5_000);
        assert_eq!(stats[0].p90_ms, 9_000);
    }

    /// A suite that died 4 seconds in is not evidence that the suite takes 4
    /// seconds.
    #[test]
    fn failed_runs_are_excluded_from_timing_stats() {
        let records = vec![
            run("unit", 10_000, 0),
            run("unit", 12_000, 0),
            run("unit", 40, 1),
        ];
        let stats = summarize(&records);
        assert_eq!(stats[0].n, 2, "the failed run must not be a sample");
        assert!(stats[0].p50_ms >= 10_000);
    }

    /// The read path is windowed, so an old burst cannot dominate today's view.
    #[test]
    fn the_stats_window_takes_the_newest_samples() {
        let mut records: Vec<RunRecord> = (0..STATS_WINDOW).map(|_| run("unit", 1, 0)).collect();
        records.extend((0..STATS_WINDOW).map(|_| run("unit", 9_999, 0)));
        let stats = summarize(&records);
        assert_eq!(stats[0].n, STATS_WINDOW);
        assert_eq!(stats[0].p50_ms, 9_999, "the window must be the newest rows");
    }

    /// A malformed line is skipped, not fatal — one bad write must not disable
    /// the feature permanently.
    #[test]
    fn a_corrupt_line_is_skipped_rather_than_poisoning_the_store() {
        let text = concat!(
            "{\"v\":1,\"b\":\"unit\",\"ms\":10,\"cpu\":5,\"at\":1,\"rc\":0,\"os\":\"linux\"}\n",
            "{ this is not json\n",
            "\n",
            "{\"v\":1,\"b\":\"unit\",\"ms\":20,\"cpu\":5,\"at\":2,\"rc\":0,\"os\":\"linux\"}\n",
        );
        let records = parse_records(text);
        assert_eq!(records.len(), 2);
    }

    /// A future writer's rows are skipped rather than misread as v1.
    #[test]
    fn rows_from_an_unknown_schema_version_are_ignored() {
        let text =
            "{\"v\":99,\"b\":\"unit\",\"ms\":10,\"cpu\":5,\"at\":1,\"rc\":0,\"os\":\"linux\"}\n";
        assert!(parse_records(text).is_empty());
    }

    #[test]
    fn an_absent_store_reads_as_no_history_rather_than_failing() {
        let tmp = TempDir::new().unwrap();
        assert!(load(tmp.path()).is_empty());
        assert!(summarize(&[]).is_empty());
    }

    #[test]
    fn an_unknown_bucket_is_recorded_as_unknown_not_dropped() {
        assert_eq!(normalize_bucket("Unit"), "unit");
        assert_eq!(normalize_bucket(" integration "), "integration");
        assert_eq!(normalize_bucket("bench"), "unknown");
    }

    /// Two writers appending concurrently must not corrupt or lose records.
    #[test]
    fn concurrent_writers_do_not_corrupt_the_store() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let handles: Vec<_> = (0..4)
            .map(|worker| {
                let root = root.clone();
                std::thread::spawn(move || {
                    for i in 0..25u64 {
                        record(&root, &run("unit", worker * 100 + i, 0));
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        let loaded = load(&root);
        assert_eq!(
            loaded.len(),
            100,
            "4 writers x 25 records must all survive intact"
        );
    }

    #[test]
    fn percentiles_handle_the_degenerate_sizes() {
        assert_eq!(percentile(&[], 50), 0);
        assert_eq!(percentile(&[7], 50), 7);
        assert_eq!(percentile(&[7], 90), 7);
        assert_eq!(percentile(&[1, 2], 100), 2);
    }

    #[test]
    fn durations_render_readably_across_the_whole_range() {
        assert_eq!(human_ms(0), "0ms");
        assert_eq!(human_ms(940), "940ms");
        assert_eq!(human_ms(1_900), "1.9s");
        assert_eq!(human_ms(232_000), "3m52s");
    }
}

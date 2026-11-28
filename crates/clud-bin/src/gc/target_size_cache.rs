//! #1687: bounded background measurements, read-only cached target reports.
//! Only the daemon calls `refresh`; the CLI reads one bounded snapshot file.

use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const CACHE_NAME: &str = "target-sizes.json";
const MAX_CACHE_BYTES: u64 = 4 * 1024 * 1024;
const ENTRY_BUDGET: usize = 200_000;
const ROW_BUDGET: usize = 2000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TargetRow {
    pub repo_root: String,
    pub worktree: String,
    pub target: String,
    pub bytes: Option<u64>,
    pub status: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Snapshot {
    pub checked_unix: i64,
    #[serde(default)]
    pub truncated: bool,
    pub rows: Vec<TargetRow>,
}

#[derive(Debug, Serialize)]
struct Report {
    checked_unix: i64,
    stale: bool,
    truncated: bool,
    rows: Vec<TargetRow>,
}

fn cache_path() -> io::Result<PathBuf> {
    Ok(crate::daemon::default_state_dir()?.join(CACHE_NAME))
}

/// Called on the existing repo-worktree probe thread, after discovery. A
/// single entry budget covers the entire snapshot, not every checkout.
/// Paths are metadata, never purge candidates. No git or provider calls.
pub(crate) fn refresh(checkouts: impl IntoIterator<Item = (String, String)>) -> io::Result<()> {
    // Registry-worker unit fixtures do not own the user's state directory.
    // Exercise persistence through an explicit temporary destination instead.
    if cfg!(test) {
        return Ok(());
    }
    refresh_at(&cache_path()?, checkouts)
}

fn refresh_at(
    path: &Path,
    checkouts: impl IntoIterator<Item = (String, String)>,
) -> io::Result<()> {
    let snapshot = measure(
        checkouts,
        super::worktree_size_cache::now_unix(),
        ENTRY_BUDGET,
    );
    crate::fs_private::write_private_atomic(path, &bounded_snapshot_bytes(snapshot)?)
}

fn bounded_snapshot_bytes(mut snapshot: Snapshot) -> io::Result<Vec<u8>> {
    let mut used = 128_u64;
    let mut retained = Vec::new();
    for row in snapshot.rows {
        used = used.saturating_add(serde_json::to_vec(&row)?.len() as u64 + 1);
        if used > MAX_CACHE_BYTES {
            snapshot.truncated = true;
            break;
        }
        retained.push(row);
    }
    snapshot.rows = retained;
    serde_json::to_vec(&snapshot).map_err(io::Error::other)
}

fn measure(
    checkouts: impl IntoIterator<Item = (String, String)>,
    now: i64,
    mut budget: usize,
) -> Snapshot {
    let unique: BTreeSet<_> = checkouts.into_iter().collect();
    let truncated = unique.len() > ROW_BUDGET;
    let rows = unique
        .into_iter()
        .take(ROW_BUDGET)
        .map(|(repo_root, worktree)| {
            let target = Path::new(&worktree).join("target");
            let (bytes, status) = measure_tree(&target, &mut budget);
            TargetRow {
                repo_root,
                worktree,
                target: target.to_string_lossy().into_owned(),
                bytes,
                status: status.to_string(),
            }
        })
        .collect();
    Snapshot {
        checked_unix: now,
        truncated,
        rows,
    }
}

fn directory_ready(root: &Path) -> Result<(), &'static str> {
    let metadata = match root.symlink_metadata() {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Err("missing"),
        Err(_) => return Err("unreadable"),
    };
    if metadata.file_type().is_symlink() {
        return Err("symlink-skipped");
    }
    if !metadata.is_dir() {
        return Err("not-directory");
    }
    Ok(())
}

fn measure_tree(root: &Path, budget: &mut usize) -> (Option<u64>, &'static str) {
    if let Err(status) = directory_ready(root) {
        return (None, status);
    }
    let mut bytes = 0_u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        if let Err(status) = scan_directory(&directory, &mut stack, &mut bytes, budget) {
            return (None, status);
        }
    }
    (Some(bytes), "complete")
}

fn scan_directory(
    directory: &Path,
    stack: &mut Vec<PathBuf>,
    bytes: &mut u64,
    budget: &mut usize,
) -> Result<(), &'static str> {
    let entries = fs::read_dir(directory).map_err(|_| "unreadable")?;
    for entry in entries {
        *budget = budget.checked_sub(1).ok_or("budget-exhausted")?;
        let entry = entry.map_err(|_| "unreadable")?;
        let metadata = entry.path().symlink_metadata().map_err(|_| "unreadable")?;
        if metadata.is_file() {
            *bytes = bytes.saturating_add(metadata.len());
        } else if metadata.is_dir() {
            stack.push(entry.path());
        }
    }
    Ok(())
}

fn load(path: &Path) -> io::Result<Snapshot> {
    let file = fs::File::open(path)?;
    let mut bytes = Vec::new();
    file.take(MAX_CACHE_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_CACHE_BYTES {
        return Err(io::Error::other("target size cache exceeds its size limit"));
    }
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}

fn report(snapshot: Snapshot, now: i64) -> Report {
    let age = now.saturating_sub(snapshot.checked_unix);
    Report {
        checked_unix: snapshot.checked_unix,
        stale: !(-super::worktree_size_cache::FUTURE_SKEW_SECS
            ..=super::worktree_size_cache::MAX_CACHE_AGE_SECS)
            .contains(&age),
        rows: snapshot.rows,
        truncated: snapshot.truncated,
    }
}

/// No daemon startup, directory measurement, cleanup or network request.
/// Reading stale data is useful, but it is always explicitly marked stale.
pub(crate) fn run_report(json: bool) -> i32 {
    let snapshot = match cache_path().and_then(|path| load(&path)) {
        Ok(snapshot) => snapshot,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if !json {
                println!(
                    "No cached target sizes; the daemon's next repository probe populates them."
                );
                return 0;
            }
            Snapshot::default()
        }
        Err(error) => {
            eprintln!("error: cannot read target size cache: {error}");
            return 1;
        }
    };
    let report = report(snapshot, super::worktree_size_cache::now_unix());
    if json {
        match serde_json::to_string(&report) {
            Ok(text) => println!("{text}"),
            Err(error) => {
                eprintln!("error: cannot serialize target sizes: {error}");
                return 1;
            }
        }
    } else {
        print_table(&report);
    }
    0
}

fn print_table(report: &Report) {
    println!(
        "Rust target directories (cached at {}{}):",
        report.checked_unix,
        if report.stale { "; stale" } else { "" }
    );
    if report.truncated {
        println!("  Snapshot truncated by its {ROW_BUDGET}-worktree or 4 MiB size limit.");
    }
    let mut previous_repo = None;
    for row in &report.rows {
        if previous_repo != Some(&row.repo_root) {
            println!("\n{}", row.repo_root);
            previous_repo = Some(&row.repo_root);
        }
        let size = row
            .bytes
            .map_or_else(|| "unknown".to_string(), |bytes| format!("{bytes} bytes"));
        println!("  {size:>18}  {:<18} {}", row.status, row.target);
    }
    if report.rows.is_empty() {
        println!("  No known repository worktrees in the last probe.");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_and_deduplicates_worktrees_and_measures_nested_files() {
        let temp = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let one = temp.path().join("one");
        let two = temp.path().join("two");
        fs::create_dir_all(one.join("target/nested")).unwrap();
        fs::create_dir_all(two.join("target")).unwrap();
        fs::write(one.join("target/nested/output"), b"1234567").unwrap();
        fs::write(two.join("target/output"), b"123").unwrap();
        let entry = |path: &Path| ("repo".to_string(), path.to_string_lossy().into_owned());
        let snapshot = measure([entry(&two), entry(&one), entry(&one)], 100, 100);
        assert_eq!(snapshot.rows.len(), 2);
        assert_eq!(snapshot.rows[0].bytes, Some(7));
        assert_eq!(snapshot.rows[1].bytes, Some(3));
        assert_eq!(
            fs::read(one.join("target/nested/output")).unwrap(),
            b"1234567"
        );
    }

    #[test]
    fn missing_and_budget_exhaustion_never_claim_zero_size() {
        let temp = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        assert_eq!(
            measure_tree(&temp.path().join("absent"), &mut 10),
            (None, "missing")
        );
        fs::write(temp.path().join("output"), b"abc").unwrap();
        assert_eq!(
            measure_tree(temp.path(), &mut 0),
            (None, "budget-exhausted")
        );
        assert_eq!(measure_tree(temp.path(), &mut 10), (Some(3), "complete"));
    }

    #[test]
    fn cache_read_does_not_remeasure_and_marks_stale_and_future_data() {
        let temp = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let path = temp.path().join("snapshot");
        fs::write(&path, br#"{"checked_unix":100,"rows":[{"repo_root":"repo","worktree":"gone","target":"gone/target","bytes":42,"status":"complete"}]}"#).unwrap();
        let fresh = report(load(&path).unwrap(), 101);
        assert!(!fresh.stale);
        assert_eq!(fresh.rows[0].bytes, Some(42));
        assert!(report(load(&path).unwrap(), 100_000).stale);
        assert!(report(load(&path).unwrap(), -1000).stale);
        fs::write(&path, "corrupt").unwrap();
        assert!(load(&path).is_err());
    }

    #[test]
    fn oversized_paths_produce_a_readable_explicitly_truncated_snapshot() {
        let snapshot = Snapshot {
            checked_unix: 100,
            truncated: false,
            rows: vec![TargetRow {
                repo_root: "repo".to_string(),
                worktree: "worktree".to_string(),
                target: "x".repeat(MAX_CACHE_BYTES as usize),
                bytes: None,
                status: "missing".to_string(),
            }],
        };
        let bytes = bounded_snapshot_bytes(snapshot).unwrap();
        assert!(bytes.len() as u64 <= MAX_CACHE_BYTES);
        let decoded: Snapshot = serde_json::from_slice(&bytes).unwrap();
        assert!(decoded.truncated);
        assert!(decoded.rows.is_empty());
    }

    #[test]
    fn background_snapshot_persistence_uses_an_explicit_isolated_destination() {
        let temp = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let checkout = temp.path().join("checkout");
        fs::create_dir_all(checkout.join("target")).unwrap();
        fs::write(checkout.join("target/output"), b"12345").unwrap();
        let cache = temp.path().join("state/target-sizes.json");
        refresh_at(
            &cache,
            [("repo".to_string(), checkout.to_string_lossy().into_owned())],
        )
        .unwrap();
        assert_eq!(load(&cache).unwrap().rows[0].bytes, Some(5));
        fs::write(checkout.join("target/output"), b"changed").unwrap();
        assert_eq!(load(&cache).unwrap().rows[0].bytes, Some(5));
    }
}

//! Rust `target/` reclamation — issue #510.
//!
//! `~/dev/**/target` is the single largest disk sink on a machine that
//! builds a lot of Rust, and clud's redb GC never sees it (it only tracks
//! worktrees / clones / extern-repos / trash / uv-cache). This module adds a
//! bounded, opt-in sweep: given a list of dev roots, find `target/` dirs that
//! sit next to a `Cargo.toml` and remove the ones whose mtime is older than a
//! threshold.
//!
//! Safety posture (deliberately conservative — this deletes regenerable build
//! output, but a mistaken delete still costs a rebuild):
//! - **Opt-in.** With no roots configured the sweep is a no-op. Enabled via
//!   `CLUD_GC_TARGET_ROOTS` (see `daemon/target_sweep.rs`).
//! - **Bounded discovery.** The walk is depth-limited and never crosses into
//!   `.git` / `node_modules` / a `target/` it already found. It never runs
//!   over the whole filesystem — only the explicitly configured roots.
//! - **mtime gate.** A `target/` touched within the threshold (default 3d)
//!   is left alone. A dir untouched that long is not part of an active build,
//!   which is the cheap stand-in for "no live session owns this".

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use super::delete_audit;

/// Default staleness gate.
///
/// Was 14 days, on the reasoning that rebuilding `target/` is expensive so we
/// should err toward keeping it. That reasoning is right and the number was
/// still wrong: a retention window only reclaims anything if it is shorter
/// than the time the machine takes to fill its disk. On the workstation this
/// was measured on, a four-day-old filesystem held 1.07 TiB, 206 GB of it in
/// `target/` dirs — every one of them younger than 14 days, so the sweep ran
/// on schedule and correctly declined to delete a single byte while the disk
/// filled. A gate longer than the fill time is not a conservative policy, it
/// is an inactive one.
///
/// Three days is the shared floor: it matches `gc::session_tmp` and
/// `gc::uv_cache`, and 72h is what a Friday-evening-to-Monday-morning absence
/// (~63h) needs so a returning user still has their build output. Raise it
/// per-machine with `CLUD_GC_TARGET_STALE_DAYS` where rebuilds are the
/// scarcer resource.
pub const DEFAULT_STALE_DAYS: u64 = 3;

/// How deep the discovery walk descends from each configured root before
/// giving up. Deep enough for `root/<repo>/<crate>/target` layouts, shallow
/// enough to stay cheap.
pub const MAX_DEPTH: usize = 6;

/// Directory names we never descend into during discovery.
const SKIP_DIRS: &[&str] = &[".git", "node_modules", ".claude"];

/// Outcome of a sweep. `bytes_freed` is best-effort (summed before removal).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SweepReport {
    pub targets_removed: usize,
    pub bytes_freed: u64,
    pub skipped: usize,
    pub dry_run: bool,
}

/// Discover `target/` directories under `root` (bounded by [`MAX_DEPTH`]).
/// A directory qualifies when it is named `target` and has a sibling
/// `Cargo.toml`. The walk does not descend into a qualifying `target/`.
pub fn find_target_dirs(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    walk(root, 0, &mut found);
    found
}

fn walk(dir: &Path, depth: usize, found: &mut Vec<PathBuf>) {
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        // Symlinks are never followed: a linked `target/` or tree could point
        // outside the worktree (#2302 review).
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if SKIP_DIRS.contains(&name.as_ref()) {
            continue;
        }
        if name == "target" && path.with_file_name("Cargo.toml").exists() {
            // `path.with_file_name("Cargo.toml")` resolves to the sibling of
            // `target/` — i.e. `<crate>/Cargo.toml`. Qualifies; don't recurse
            // into build output.
            found.push(path);
            continue;
        }
        walk(&path, depth + 1, found);
    }
}

/// Defense-in-depth over-match guard (issue #893): refuse to remove anything
/// that is not verifiably Rust build output — a directory named exactly
/// `target` with a sibling `Cargo.toml`. The discovery walk already enforces
/// both conditions, so this re-checks them independently at the deletion
/// boundary: a future edit to [`walk`] that loosens matching must also get
/// past this guard before it can delete something that is not build output.
pub fn ensure_removable_build_output(path: &Path) -> Result<(), String> {
    if path.file_name() != Some(std::ffi::OsStr::new("target")) {
        return Err(format!(
            "refusing non-build-output path: final component is not `target`: {}",
            path.display()
        ));
    }
    if !path.with_file_name("Cargo.toml").is_file() {
        return Err(format!(
            "refusing `target/` without a sibling Cargo.toml: {}",
            path.display()
        ));
    }
    Ok(())
}

/// Sweep every configured root, removing stale `target/` dirs. `threshold`
/// is the age gate; `now` is injectable for tests. No roots ⇒ empty report.
pub fn sweep_roots_at(
    roots: &[PathBuf],
    now: SystemTime,
    threshold: Duration,
    dry_run: bool,
) -> SweepReport {
    let mut report = SweepReport {
        dry_run,
        ..Default::default()
    };
    let rule = format!("target-sweep stale>{}d", threshold.as_secs() / 86_400);
    for root in roots {
        for target in find_target_dirs(root) {
            let Some(mtime) = newest_build_mtime(&target) else {
                continue;
            };
            // Clock skew (future mtime) → skip; treat as fresh.
            let Ok(age) = now.duration_since(mtime) else {
                continue;
            };
            if age <= threshold {
                continue;
            }
            if let Err(reason) = ensure_removable_build_output(&target) {
                eprintln!("[clud] target sweep: {reason}");
                report.skipped += 1;
                continue;
            }
            let bytes = dir_size(&target);
            if dry_run {
                report.targets_removed += 1;
                report.bytes_freed = report.bytes_freed.saturating_add(bytes);
                continue;
            }
            // Audit *before* acting (#893) so even a crash mid-deletion
            // leaves the line that explains it.
            delete_audit::record("gc.target-sweep", &target, &rule);
            match fs::remove_dir_all(&target) {
                Ok(()) => {
                    report.targets_removed += 1;
                    report.bytes_freed = report.bytes_freed.saturating_add(bytes);
                }
                // Non-fatal (Windows lock, races). Retried next sweep.
                Err(_) => report.skipped += 1,
            }
        }
    }
    report
}

/// Newest mtime among `target/` and two levels below it. A directory's own
/// mtime moves only when a direct child is added or removed, so an incremental
/// build under `target/debug/...` leaves `target/` looking old (#1911). The
/// profile dirs and their `deps/`, `.fingerprint/` and `build/` children do
/// change on every build that produces output.
pub(crate) fn newest_build_mtime(target: &Path) -> Option<SystemTime> {
    let mut newest = fs::metadata(target).ok()?.modified().ok()?;
    let mut consider = |path: &Path| {
        if let Ok(mtime) = fs::symlink_metadata(path).and_then(|meta| meta.modified()) {
            newest = newest.max(mtime);
        }
    };
    for child in fs::read_dir(target).into_iter().flatten().flatten() {
        let path = child.path();
        consider(&path);
        if child.file_type().is_ok_and(|kind| kind.is_dir()) {
            for grandchild in fs::read_dir(&path).into_iter().flatten().flatten() {
                consider(&grandchild.path());
            }
        }
    }
    Some(newest)
}

pub(crate) fn dir_size(path: &Path) -> u64 {
    let mut budget = usize::MAX;
    dir_size_bounded(path, &mut budget)
}

/// [`dir_size`] that stops (undercounting) after `budget` entries.
pub(crate) fn dir_size_bounded(path: &Path, budget: &mut usize) -> u64 {
    let mut total = 0u64;
    let Ok(entries) = fs::read_dir(path) else {
        return 0;
    };
    for entry in entries.flatten() {
        if *budget == 0 {
            break;
        }
        *budget -= 1;
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_dir() {
            total = total.saturating_add(dir_size_bounded(&entry.path(), budget));
        } else {
            total = total.saturating_add(meta.len());
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use kernal_api::platform::fs::TemporaryDirectory;
    use std::fs::File;
    use std::io::Write;
    use std::time::Duration as StdDuration;

    #[test]
    fn recent_build_output_below_target_keeps_it_fresh() {
        let tmp = TemporaryDirectory::new().unwrap();
        let target = make_crate_with_target(tmp.path(), "c");
        let now = SystemTime::now() + StdDuration::from_secs(4 * 24 * 60 * 60);
        let debug = target.join("debug");
        fs::create_dir_all(&debug).unwrap();
        File::create(debug.join("foo")).unwrap();
        let recent = filetime::FileTime::from_system_time(now - StdDuration::from_secs(3600));
        filetime::set_file_mtime(debug.join("foo"), recent).unwrap();

        let report = sweep_roots_at(
            &[tmp.path().to_path_buf()],
            now,
            StdDuration::from_secs(3 * 24 * 60 * 60),
            false,
        );

        assert_eq!(report.targets_removed, 0);
        assert!(target.exists());
    }

    /// Build `<root>/<crate_name>/{Cargo.toml, target/payload.o}` and return
    /// the target dir path.
    fn make_crate_with_target(root: &Path, crate_name: &str) -> PathBuf {
        let crate_dir = root.join(crate_name);
        fs::create_dir_all(&crate_dir).unwrap();
        File::create(crate_dir.join("Cargo.toml"))
            .unwrap()
            .write_all(b"[package]\nname = \"x\"\n")
            .unwrap();
        let target = crate_dir.join("target");
        fs::create_dir_all(&target).unwrap();
        File::create(target.join("payload.o"))
            .unwrap()
            .write_all(b"0123456789")
            .unwrap();
        target
    }

    #[test]
    fn find_discovers_target_next_to_cargo_toml() {
        let tmp = TemporaryDirectory::new().unwrap();
        let target = make_crate_with_target(tmp.path(), "mycrate");
        let found = find_target_dirs(tmp.path());
        assert_eq!(found, vec![target]);
    }

    #[test]
    fn find_ignores_target_without_sibling_cargo_toml() {
        let tmp = TemporaryDirectory::new().unwrap();
        // A bare `target/` with no Cargo.toml sibling must not match.
        fs::create_dir_all(tmp.path().join("weird").join("target")).unwrap();
        let found = find_target_dirs(tmp.path());
        assert!(found.is_empty());
    }

    #[test]
    fn find_does_not_descend_into_skip_dirs() {
        let tmp = TemporaryDirectory::new().unwrap();
        // A target buried under node_modules should be skipped entirely.
        let nm = tmp.path().join("node_modules").join("pkg");
        fs::create_dir_all(&nm).unwrap();
        File::create(nm.join("Cargo.toml")).unwrap();
        fs::create_dir_all(nm.join("target")).unwrap();
        let found = find_target_dirs(tmp.path());
        assert!(found.is_empty());
    }

    #[test]
    fn sweep_no_roots_is_empty() {
        let report = sweep_roots_at(&[], SystemTime::now(), Duration::from_secs(1), false);
        assert_eq!(report, SweepReport::default());
    }

    #[test]
    fn sweep_leaves_fresh_targets() {
        let tmp = TemporaryDirectory::new().unwrap();
        let target = make_crate_with_target(tmp.path(), "c");
        let report = sweep_roots_at(
            &[tmp.path().to_path_buf()],
            SystemTime::now(),
            Duration::from_secs(3 * 24 * 60 * 60),
            false,
        );
        assert_eq!(report.targets_removed, 0);
        assert!(target.exists());
    }

    #[test]
    fn sweep_removes_stale_targets_and_counts_bytes() {
        let tmp = TemporaryDirectory::new().unwrap();
        let target = make_crate_with_target(tmp.path(), "c");
        let future_now = SystemTime::now() + StdDuration::from_secs(15 * 24 * 60 * 60);
        let report = sweep_roots_at(
            &[tmp.path().to_path_buf()],
            future_now,
            Duration::from_secs(3 * 24 * 60 * 60),
            false,
        );
        assert_eq!(report.targets_removed, 1);
        assert!(report.bytes_freed >= 10, "should count the 10-byte payload");
        assert!(!target.exists());
    }

    #[test]
    fn sweep_dry_run_does_not_delete() {
        let tmp = TemporaryDirectory::new().unwrap();
        let target = make_crate_with_target(tmp.path(), "c");
        let future_now = SystemTime::now() + StdDuration::from_secs(15 * 24 * 60 * 60);
        let report = sweep_roots_at(
            &[tmp.path().to_path_buf()],
            future_now,
            Duration::from_secs(3 * 24 * 60 * 60),
            true,
        );
        assert_eq!(report.targets_removed, 1);
        assert!(target.exists(), "dry run must not delete");
    }

    #[test]
    fn default_stale_days_is_14() {
        assert_eq!(DEFAULT_STALE_DAYS, 3);
    }

    // ---- #893 defense-in-depth guard ----

    #[test]
    fn guard_accepts_a_qualified_target_dir() {
        let tmp = TemporaryDirectory::new().unwrap();
        let target = make_crate_with_target(tmp.path(), "c");
        ensure_removable_build_output(&target).unwrap();
    }

    #[test]
    fn guard_refuses_a_directory_not_named_target() {
        let tmp = TemporaryDirectory::new().unwrap();
        // A live repo root: named like a repo, has a Cargo.toml. Exactly the
        // shape #893 reported vanishing — the guard must refuse it even if a
        // future discovery-walk regression ever hands it to the sweeper.
        let repo = tmp.path().join("text2infographic");
        fs::create_dir_all(&repo).unwrap();
        File::create(repo.join("Cargo.toml")).unwrap();
        let err = ensure_removable_build_output(&repo).unwrap_err();
        assert!(err.contains("not `target`"), "{err}");
    }

    #[test]
    fn guard_refuses_target_without_sibling_cargo_toml() {
        let tmp = TemporaryDirectory::new().unwrap();
        let bare = tmp.path().join("weird").join("target");
        fs::create_dir_all(&bare).unwrap();
        let err = ensure_removable_build_output(&bare).unwrap_err();
        assert!(err.contains("Cargo.toml"), "{err}");
    }

    /// End-to-end: a real sweep writes exactly one pre-deletion JSONL line
    /// naming site, path, and rule into the injected state dir (#893).
    /// Tolerates unrelated concurrent deletions landing in the same file by
    /// matching our line rather than asserting an exact count.
    #[test]
    fn sweep_writes_pre_deletion_audit_line() {
        use crate::gc::delete_audit::{StateDirGuard, AUDIT_LOG_FILE};

        let tmp = TemporaryDirectory::new().unwrap();
        let state = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let _guard = StateDirGuard::set(state.path());
        let target = make_crate_with_target(tmp.path(), "c");
        let future_now = SystemTime::now() + StdDuration::from_secs(15 * 24 * 60 * 60);
        let report = sweep_roots_at(
            &[tmp.path().to_path_buf()],
            future_now,
            Duration::from_secs(3 * 24 * 60 * 60),
            false,
        );
        assert_eq!(report.targets_removed, 1);
        assert!(!target.exists(), "the stale target was actually removed");
        let body = fs::read_to_string(state.path().join(AUDIT_LOG_FILE)).unwrap();
        let ours = body
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .find(|v| v["site"] == "gc.target-sweep")
            .expect("a gc.target-sweep line must exist");
        assert_eq!(ours["rule"], "target-sweep stale>3d");
        assert!(
            ours["path"].as_str().unwrap().ends_with("target"),
            "audit line names the exact path: {:?}",
            ours["path"]
        );
    }
}

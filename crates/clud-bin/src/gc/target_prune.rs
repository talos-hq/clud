//! Issue #2302: reclaim cargo build output before trashing a whole worktree.
//! #2487: the same prune runs first for worktrees scheduled by age.
//!
//! About 77% of `~/.clud/worktrees` is `target/`. When the root is over
//! `worktrees.warn_bytes`, each idle, not-in-use large worktree has its
//! `target/` dirs (top-level and nested ones beside a `Cargo.toml`) pruned with
//! `soldr cache prune-target <target> --force`, which deletes only superseded
//! artifacts. Git state and source are never touched. A missing `soldr` or a
//! failed prune is logged and the worktree falls through to the warn-then-trash
//! path unchanged.
//!
//! Safety: symlinks are never followed (a linked `target/` is skipped, and each
//! target must canonicalize inside the canonical worktree); each `soldr` call
//! has a hard [`PRUNE_DEADLINE`]; a target whose newest build output is under
//! [`MIN_IDLE_SECS`] old counts as in use; the in-use predicate is re-checked
//! right before every prune; and a target whose newest build mtime matches the
//! persisted [`PruneState`] is skipped, so an idle over-budget root does not
//! re-run `soldr` or re-walk sizes every sweep.
//!
//! [`select`] and [`prune_worktree`] are pure over injected facts (candidate
//! facts, a runner, a sizer); the daemon wires in the real `soldr` runner.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::gc::target_sweep::{dir_size_bounded, find_target_dirs, newest_build_mtime};
use crate::gc::worktree_root::SIZE_SCAN_ENTRY_BUDGET;
use crate::gc::worktree_schedule::{Candidate, MIN_IDLE_SECS};

/// Hard ceiling for one `soldr cache prune-target` call.
pub const PRUNE_DEADLINE: Duration = Duration::from_secs(5 * 60);

/// Set once `soldr` rejects `prune-target`; stops retrying for this daemon run.
static UNSUPPORTED: AtomicBool = AtomicBool::new(false);

const STATE_FILE_NAME: &str = "worktrees-target-prune.json";

/// Result of pruning one worktree.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PruneOutcome {
    pub reclaimed_bytes: u64,
    pub targets_pruned: usize,
    pub targets_failed: usize,
}

/// Canonical target path -> newest build mtime (unix secs) when last handled.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct PruneState {
    handled: BTreeMap<String, i64>,
}

impl PruneState {
    fn path_for(wt_root: &Path) -> PathBuf {
        wt_root.parent().unwrap_or(wt_root).join(STATE_FILE_NAME)
    }

    /// Missing or corrupt reads as empty: that only costs a redundant prune.
    pub fn load(wt_root: &Path) -> Self {
        fs::read_to_string(Self::path_for(wt_root))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    /// Forget targets that no longer exist, so the file cannot grow forever.
    fn forget_vanished(&mut self) {
        self.handled.retain(|path, _| Path::new(path).exists());
    }

    pub fn save(&mut self, wt_root: &Path) {
        self.forget_vanished();
        if let Ok(text) = serde_json::to_string(self) {
            let _ = fs::write(Self::path_for(wt_root), text);
        }
    }
}

/// Whether one candidate may be pruned: idle long enough and not exempt.
pub fn eligible(c: &Candidate) -> bool {
    !c.exempt && c.idle_secs >= MIN_IDLE_SECS
}

/// Worktrees to prune this tick: root over budget and [`eligible`]. Largest
/// first is the caller's order and is preserved.
pub fn select(over: bool, candidates: &[Candidate]) -> Vec<&Candidate> {
    if !over {
        return Vec::new();
    }
    candidates.iter().filter(|c| eligible(c)).collect()
}

fn secs(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// Prune every `target/` under `worktree` through `run`, measuring each with
/// `size`. `unused` is re-asked before every prune; `now` is unix seconds.
/// A `run` error counts as a failed target and never aborts the rest.
pub fn prune_worktree(
    worktree: &Path,
    run: &mut dyn FnMut(&Path) -> Result<(), String>,
    size: &dyn Fn(&Path) -> u64,
    unused: &dyn Fn() -> bool,
    state: &mut PruneState,
    now: i64,
) -> PruneOutcome {
    let mut out = PruneOutcome::default();
    let Ok(root) = fs::canonicalize(worktree) else {
        return out;
    };
    for target in find_target_dirs(worktree) {
        let Ok(canon) = fs::canonicalize(&target) else {
            continue;
        };
        if !canon.starts_with(&root) {
            continue;
        }
        let Some(newest) = newest_build_mtime(&canon).map(secs) else {
            continue;
        };
        let key = canon.to_string_lossy().into_owned();
        if now.saturating_sub(newest) < MIN_IDLE_SECS || state.handled.get(&key) == Some(&newest) {
            continue;
        }
        if !unused() {
            continue;
        }
        let before = size(&canon);
        match run(&canon) {
            Ok(()) => {
                out.targets_pruned += 1;
                out.reclaimed_bytes = out
                    .reclaimed_bytes
                    .saturating_add(before.saturating_sub(size(&canon)));
            }
            Err(e) => {
                out.targets_failed += 1;
                if !e.is_empty() {
                    eprintln!(
                        "[clud] gc: soldr prune-target failed for {}: {e}",
                        canon.display()
                    );
                }
            }
        }
        // Record the post-prune mtime (pruning itself touches the tree) so an
        // unchanged target is not retried, even after a failure.
        if let Some(after) = newest_build_mtime(&canon).map(secs) {
            state.handled.insert(key, after);
        }
    }
    out
}

/// `soldr` from PATH, else the directory clud installs it into
/// ([`crate::optimize::global_bin_dir`], which honours `CARGO_HOME`).
fn find_soldr() -> Result<PathBuf, String> {
    if let Ok(found) = which::which("soldr") {
        return Ok(found);
    }
    let home = crate::home::user_home().ok_or("soldr not found: no home directory")?;
    find_soldr_in(crate::optimize::global_bin_dir(&home), &home)
}

fn find_soldr_in(dir: PathBuf, cwd: &Path) -> Result<PathBuf, String> {
    which::which_in("soldr", Some(dir), cwd).map_err(|e| format!("soldr not found: {e}"))
}

fn rejects_prune_target(output: &str) -> bool {
    let lower = output.to_ascii_lowercase();
    lower.contains("prune-target")
        && ["unrecognized", "unknown", "unexpected argument", "invalid"]
            .iter()
            .any(|w| lower.contains(w))
}

/// Real runner: `soldr cache prune-target <target> --force`, killed after
/// [`PRUNE_DEADLINE`]. `Err` when soldr is missing, times out or exits
/// non-zero; an old soldr without `prune-target` is reported once per daemon
/// run, then every later call fails with an empty (unlogged) message.
pub fn soldr_runner() -> Result<impl FnMut(&Path) -> Result<(), String>, String> {
    if UNSUPPORTED.load(Ordering::Relaxed) {
        return Err(String::new());
    }
    let soldr = find_soldr()?;
    Ok(move |target: &Path| {
        if UNSUPPORTED.load(Ordering::Relaxed) {
            return Err(String::new());
        }
        let argv = vec![
            soldr.to_string_lossy().into_owned(),
            "cache".into(),
            "prune-target".into(),
            target.to_string_lossy().into_owned(),
            "--force".into(),
        ];
        match crate::backend_bootstrap::run_captured_command_with_deadline(argv, PRUNE_DEADLINE)? {
            (0, _) => Ok(()),
            (code, out) if rejects_prune_target(&out) => {
                UNSUPPORTED.store(true, Ordering::Relaxed);
                Err(format!(
                    "soldr lacks `cache prune-target` (exit {code}); disabled until daemon restart"
                ))
            }
            (code, out) => Err(format!("exit {code}: {}", out.trim())),
        }
    })
}

/// Production sizer, bounded by the worktree size-scan entry budget.
pub fn real_size(path: &Path) -> u64 {
    let mut budget = SIZE_SCAN_ENTRY_BUDGET;
    dir_size_bounded(path, &mut budget)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gc::target_sweep::dir_size;

    const DAY: i64 = 24 * 60 * 60;

    fn cand(path: &str, idle: i64, exempt: bool) -> Candidate {
        Candidate {
            path: path.into(),
            bytes: 1,
            idle_secs: idle,
            exempt,
            dirty: None,
        }
    }

    #[test]
    fn select_skips_in_use_recent_and_under_budget() {
        let c = [
            cand("/a", 2 * DAY, false),
            cand("/b", 2 * DAY, true),
            cand("/c", DAY - 1, false),
        ];
        let got: Vec<_> = select(true, &c).iter().map(|c| c.path.as_str()).collect();
        assert_eq!(got, ["/a"]);
        assert!(select(false, &c).is_empty());
    }

    fn make_target(dir: &Path, bytes: usize) {
        fs::create_dir_all(dir.join("target/debug")).unwrap();
        fs::write(dir.join("Cargo.toml"), "").unwrap();
        fs::write(dir.join("target/debug/big"), vec![0u8; bytes]).unwrap();
    }

    /// Unix "now" two days past the freshly written fixture files.
    fn later() -> i64 {
        secs(SystemTime::now()) + 2 * DAY
    }

    fn go(
        wt: &Path,
        run: &mut dyn FnMut(&Path) -> Result<(), String>,
        state: &mut PruneState,
        now: i64,
    ) -> PruneOutcome {
        prune_worktree(wt, run, &dir_size, &|| true, state, now)
    }

    #[test]
    fn prunes_top_level_and_nested_targets_and_sums_reclaimed() {
        let wt = tempfile::tempdir().unwrap();
        make_target(wt.path(), 1000);
        make_target(&wt.path().join("nested"), 500);
        let mut seen = Vec::new();
        let mut run = |t: &Path| {
            seen.push(t.to_path_buf());
            fs::write(t.join("debug/big"), b"x").unwrap(); // shrink to 1 byte
            Ok(())
        };
        let out = go(wt.path(), &mut run, &mut PruneState::default(), later());
        assert_eq!(seen.len(), 2);
        assert_eq!(out.targets_pruned, 2);
        assert_eq!(out.reclaimed_bytes, 999 + 499);
        assert!(wt.path().join("Cargo.toml").exists());
    }

    #[test]
    fn a_failed_prune_is_counted_and_reclaims_nothing() {
        let wt = tempfile::tempdir().unwrap();
        make_target(wt.path(), 100);
        let mut run = |_: &Path| Err("boom".to_string());
        let out = go(wt.path(), &mut run, &mut PruneState::default(), later());
        assert_eq!(
            out,
            PruneOutcome {
                reclaimed_bytes: 0,
                targets_pruned: 0,
                targets_failed: 1
            }
        );
    }

    #[test]
    fn target_without_cargo_toml_is_left_alone() {
        let wt = tempfile::tempdir().unwrap();
        fs::create_dir_all(wt.path().join("target")).unwrap();
        let mut run = |_: &Path| -> Result<(), String> { panic!("must not run") };
        let out = go(wt.path(), &mut run, &mut PruneState::default(), later());
        assert_eq!(out, PruneOutcome::default());
    }

    #[test]
    fn second_sweep_of_an_unchanged_target_does_not_call_the_runner() {
        let wt = tempfile::tempdir().unwrap();
        make_target(wt.path(), 100);
        let mut state = PruneState::default();
        let mut calls = 0;
        let mut run = |_: &Path| {
            calls += 1;
            Ok(())
        };
        go(wt.path(), &mut run, &mut state, later());
        go(wt.path(), &mut run, &mut state, later());
        assert_eq!(calls, 1);
    }

    #[test]
    fn state_round_trips_through_the_gc_state_dir() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("worktrees");
        let target = home.path().join("target");
        fs::create_dir_all(&target).unwrap();
        let key = target.to_string_lossy().into_owned();
        let mut state = PruneState::default();
        state.handled.insert(key.clone(), 7);
        state.save(&root);
        assert_eq!(PruneState::load(&root).handled.get(&key), Some(&7));
    }

    #[test]
    fn save_drops_entries_for_vanished_targets() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("worktrees");
        let live = home.path().join("live-target");
        fs::create_dir_all(&live).unwrap();
        let mut state = PruneState::default();
        state.handled.insert(live.to_string_lossy().into_owned(), 1);
        state.handled.insert("/gone/target".into(), 2);
        state.save(&root);
        let loaded = PruneState::load(&root);
        assert_eq!(loaded.handled.len(), 1);
        assert!(loaded.handled.contains_key(live.to_string_lossy().as_ref()));
    }

    #[test]
    fn soldr_is_found_in_the_dir_global_bin_dir_picks_from_cargo_home() {
        struct Restore(Option<std::ffi::OsString>);
        impl Drop for Restore {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(v) => std::env::set_var("CARGO_HOME", v),
                    None => std::env::remove_var("CARGO_HOME"),
                }
            }
        }
        let _lock = crate::test_env::home_lock();
        let cargo_home = tempfile::tempdir().unwrap();
        let bin = cargo_home.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        let name = if crate::platform::host::WINDOWS {
            "soldr.exe"
        } else {
            "soldr"
        };
        fs::write(bin.join(name), "").unwrap();
        crate::platform::optimize::make_executable(&bin.join(name)).unwrap();
        let _restore = Restore(std::env::var_os("CARGO_HOME"));
        std::env::set_var("CARGO_HOME", cargo_home.path());
        let dir = crate::optimize::global_bin_dir(Path::new("/nonexistent-home"));
        assert_eq!(dir, bin);
        assert!(find_soldr_in(dir, cargo_home.path()).is_ok());
    }

    #[test]
    fn recent_build_output_counts_as_in_use() {
        let wt = tempfile::tempdir().unwrap();
        make_target(wt.path(), 100);
        let mut run = |_: &Path| -> Result<(), String> { panic!("must not run") };
        let now = secs(SystemTime::now());
        let out = go(wt.path(), &mut run, &mut PruneState::default(), now);
        assert_eq!(out, PruneOutcome::default());
    }

    #[test]
    fn a_worktree_that_becomes_in_use_mid_loop_stops_being_pruned() {
        let wt = tempfile::tempdir().unwrap();
        make_target(wt.path(), 100);
        make_target(&wt.path().join("nested"), 100);
        let asked = std::cell::Cell::new(0);
        let unused = || {
            asked.set(asked.get() + 1);
            asked.get() == 1 // free for the first target only
        };
        let mut calls = 0;
        let mut run = |_: &Path| {
            calls += 1;
            Ok(())
        };
        let mut state = PruneState::default();
        prune_worktree(wt.path(), &mut run, &dir_size, &unused, &mut state, later());
        assert_eq!(calls, 1);
        assert_eq!(asked.get(), 2);
    }

    #[test]
    fn symlinked_target_to_outside_is_not_pruned_and_linked_dirs_are_not_walked() {
        if crate::platform::host::WINDOWS {
            return; // directory symlinks need privileges there
        }
        let outside = tempfile::tempdir().unwrap();
        make_target(outside.path(), 100);
        let wt = tempfile::tempdir().unwrap();
        fs::write(wt.path().join("Cargo.toml"), "").unwrap();
        crate::platform::alias_link::symlink(
            &outside.path().join("target"),
            &wt.path().join("target"),
        )
        .unwrap();
        crate::platform::alias_link::symlink(outside.path(), &wt.path().join("linked")).unwrap();
        assert!(find_target_dirs(wt.path()).is_empty());
        let mut run = |_: &Path| -> Result<(), String> { panic!("must not run") };
        let out = go(wt.path(), &mut run, &mut PruneState::default(), later());
        assert_eq!(out, PruneOutcome::default());
    }

    #[test]
    fn rejected_subcommand_output_is_recognised() {
        assert!(rejects_prune_target(
            "error: unrecognized subcommand 'prune-target'"
        ));
        assert!(!rejects_prune_target("disk full"));
    }
}

use std::collections::HashSet;
use std::path::Path;

use crate::gc::{
    extract_pid_from_lock_reason, Registry, TrackedEntry, EXTERN_REPO_KIND, SIBLING_CLONE_KIND,
    WORKTREE_KIND,
};
use crate::session_registry::{LivenessProbe, OsLivenessProbe};
use crate::worktrees;

/// Paths that `git worktree list --porcelain` reports as `locked` with a
/// reason of the form `agent <pid>` where the PID is still alive. Used to
/// shield in-flight `clud` worktrees from `clud gc purge`.
pub(super) fn collect_live_lock_paths() -> HashSet<String> {
    let mut out = HashSet::new();
    let probe = OsLivenessProbe;
    let main_root = match worktrees::locate_main_repo_root() {
        Ok(p) => p,
        Err(_) => return out,
    };
    let raw = match worktrees::run_git(&main_root, &["worktree", "list", "--porcelain"]) {
        Ok(s) => s,
        Err(_) => return out,
    };
    let entries = worktrees::parse_worktree_porcelain(&raw);
    for e in entries {
        if !e.locked {
            continue;
        }
        let Some(reason) = e.locked_reason.as_deref() else {
            continue;
        };
        let Some(pid) = extract_pid_from_lock_reason(reason) else {
            continue;
        };
        if probe.is_alive(pid) {
            out.insert(e.path.to_string_lossy().to_string());
        }
    }
    out
}

/// Filesystem-only half of removing one tracked entry. Used by both
/// the synchronous path (`DeleteById`) and the parallel purge pool
/// (`PurgeJob`). Safe to call from any thread — does not touch redb.
pub(super) fn remove_entry_filesystem(entry: &TrackedEntry, evidence: &str) -> Result<(), String> {
    remove_entry_filesystem_with_git(entry, evidence, &worktrees::run_git)
}

fn remove_entry_filesystem_with_git<F>(
    entry: &TrackedEntry,
    evidence: &str,
    git: &F,
) -> Result<(), String>
where
    F: Fn(&Path, &[&str]) -> Result<String, String>,
{
    // Audit before acting (#893): the entry path is user data by definition
    // (it is in the registry), so the line names the kind that authorized it.
    crate::gc::delete_audit::record_required(
        "gc.entry-delete",
        Path::new(&entry.path),
        &format!("kind={} registry_id={} {evidence}", entry.kind, entry.id),
    )
    .map_err(|error| format!("refusing unaudited deletion: {error}"))?;
    if entry.kind == "worktree" {
        let main_root = entry.repo_root.clone().unwrap_or_else(|| ".".to_string());
        // Git is the final independent dirty-tree guard. Never force or
        // fall back to recursive deletion after Git refuses the removal.
        git(Path::new(&main_root), &["worktree", "remove", &entry.path]).map(|_| ())
    } else if entry.kind == "trash" {
        remove_tree_summarized(Path::new(&entry.path))
    } else {
        let p = Path::new(&entry.path);
        if p.exists() {
            remove_tree_summarized(p)
        } else {
            Ok(())
        }
    }
}

/// Remove one tree; every failure collapses into a single summary line
/// (#2397): a container-built tree owned by root fails per file, and the entry
/// should read as one problem, not thousands of errors.
fn remove_tree_summarized(path: &Path) -> Result<(), String> {
    let mut undeletable = crate::gc::undeletable::Undeletable::default();
    crate::rm_tool::remove_dir_tree_collecting(path, &mut |p, e| undeletable.record(p, e));
    undeletable.summary_line().map_or(Ok(()), Err)
}

/// Synchronous "remove filesystem entry, then drop redb row" — used by
/// the per-row `GcOp::DeleteById` path which still needs the dashboard
/// to see the row gone before the response returns. Bulk purges use
/// the async fan-out path via `dispatch_purge_entries` instead.
pub(super) fn remove_entry_and_delete_row(
    registry: &Registry,
    entry: &TrackedEntry,
) -> Result<(), String> {
    remove_entry_filesystem(entry, "explicit DeleteById; live-session guard passed")?;
    registry.delete(entry.id).map_err(|e| e.to_string())
}

/// Drop source-checkout rows (`worktree`, `extern-repo`, `sibling-clone`)
/// whose directory is already gone (#2095). Their purge path re-checks the
/// directory, finds nothing and spares them as "dangling: path missing", so
/// without this the row is listed and skipped on every tick, forever. Only
/// the row goes: there is nothing on disk left to delete.
pub(super) fn forget_dangling_source_rows(registry: &Registry) -> Result<usize, String> {
    let mut forgotten = 0usize;
    for kind in [WORKTREE_KIND, EXTERN_REPO_KIND, SIBLING_CLONE_KIND] {
        for entry in registry.list(Some(kind)).map_err(|err| err.to_string())? {
            if std::fs::symlink_metadata(&entry.path)
                .is_err_and(|err| err.kind() == std::io::ErrorKind::NotFound)
            {
                registry.delete(entry.id).map_err(|err| err.to_string())?;
                eprintln!("[gc] forgot {kind} row for missing {}", entry.path);
                forgotten += 1;
            }
        }
    }
    Ok(forgotten)
}

/// [`forget_dangling_source_rows`] for the tick, which only logs.
pub(super) fn forget_dangling_source_rows_logged(registry: &Registry) {
    match forget_dangling_source_rows(registry) {
        Ok(0) => {}
        Ok(n) => eprintln!("[clud] gc tick: forgot {n} dangling row(s)"),
        Err(err) => eprintln!("[clud] gc tick: dangling-row sweep failed: {err}"),
    }
}

/// Remove registered trash rows. A `clud trash` quarantine entry goes as
/// soon as it can be deleted; an `safe-rm` entry (it carries
/// [`crate::rm_tool::TRASH_MANIFEST`]) is kept for
/// [`crate::rm_tool::TRASH_KEEP`] so it can be restored (#1340).
pub(super) fn reap_trash_entries(registry: &Registry) -> Result<(usize, usize), String> {
    reap_trash_entries_at(registry, std::time::SystemTime::now())
}

pub(super) fn reap_trash_entries_at(
    registry: &Registry,
    now: std::time::SystemTime,
) -> Result<(usize, usize), String> {
    let entries = registry
        .list(Some("trash"))
        .map_err(|err| err.to_string())?;
    let mut removed = 0usize;
    let mut failed = 0usize;
    let mut undeletable = crate::gc::undeletable::Undeletable::default();
    for entry in entries {
        if crate::rm_tool::keep_trash_entry(Path::new(&entry.path), now) {
            continue;
        }
        let path = Path::new(&entry.path);
        if std::fs::symlink_metadata(path)
            .is_err_and(|err| err.kind() == std::io::ErrorKind::NotFound)
        {
            // Already gone (size cap, or removed by hand): drop the row
            // instead of failing on it every tick (#1672).
            registry.delete(entry.id).map_err(|err| err.to_string())?;
            removed += 1;
            continue;
        }
        // Audit before acting (#893).
        crate::gc::delete_audit::record("gc.trash-reap", path, "trash");
        if remove_trash_dir(path, &mut undeletable) {
            registry.delete(entry.id).map_err(|err| err.to_string())?;
            eprintln!("[gc] trash: reaped {}", entry.path);
            removed += 1;
        } else {
            // #1672: this used to be silent; one root-owned entry was
            // "reaped" 373 times with no trace of why it stayed.
            failed += 1;
        }
    }
    // #2397: one line for the whole pass, not one error per file.
    if let Some(line) = undeletable.summary_line() {
        eprintln!("[gc] trash: {line}");
    }
    Ok((removed, failed))
}

/// Remove a trash tree after making it owner-writable, as safe-rm's own purge
/// does (#1573): sealed build output is read-only. Files owned by another user
/// (root, from a Docker bind mount) still fail; each failure is recorded in
/// `undeletable` for one summary line (#2397). `true` when the tree is gone.
fn remove_trash_dir(path: &Path, undeletable: &mut crate::gc::undeletable::Undeletable) -> bool {
    let before = undeletable.count();
    crate::rm_tool::remove_dir_tree_collecting(path, &mut |p, e| undeletable.record(p, e));
    undeletable.count() == before
}

/// Remove expired `safe-rm` trash entries under `trash_root` that
/// never reached the registry (the call found no daemon to register with).
/// Registered ones are removed by [`reap_trash_entries`] first.
pub(super) fn reap_unregistered_rm_trash(trash_root: &Path, now: std::time::SystemTime) -> usize {
    let mut removed = 0usize;
    let mut undeletable = crate::gc::undeletable::Undeletable::default();
    for dir in crate::rm_tool::expired_trash_entries(trash_root, now) {
        // Audit before acting (#893).
        crate::gc::delete_audit::record("gc.rm-trash-reap", &dir, "rm-trash expired");
        if remove_trash_dir(&dir, &mut undeletable) {
            eprintln!("[gc] trash: reaped {}", dir.display());
            removed += 1;
        }
    }
    if let Some(line) = undeletable.summary_line() {
        eprintln!("[gc] trash: {line}");
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gc::delete_audit::{StateDirGuard, AUDIT_LOG_FILE};
    use crate::gc::InsertInput;

    #[test]
    fn git_refusal_never_falls_back_to_recursive_source_deletion() {
        let tmp = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let source = tmp.path().join("source");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("new-work"), "changed after probe").unwrap();
        let entry = TrackedEntry {
            id: 1,
            kind: "worktree".into(),
            path: source.to_string_lossy().into_owned(),
            repo_root: Some(tmp.path().to_string_lossy().into_owned()),
            branch: None,
            agent_id: None,
            created_unix: 0,
        };
        let result = remove_entry_filesystem_with_git(
            &entry,
            "verified before concurrent edit",
            &|_, args| {
                assert_eq!(args, &["worktree", "remove", entry.path.as_str()]);
                Err("worktree became dirty".into())
            },
        );
        assert_eq!(result, Err("worktree became dirty".into()));
        assert!(source.join("new-work").exists());
    }

    #[test]
    fn failed_required_audit_preserves_source_and_registry_row() {
        let tmp = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let state = tmp.path().join("state");
        std::fs::create_dir_all(state.join(AUDIT_LOG_FILE)).unwrap();
        let _guard = StateDirGuard::set(&state);
        let source = tmp.path().join("source");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("work"), "unpushed").unwrap();
        let registry = Registry::open_at(&tmp.path().join("registry.redb")).unwrap();
        registry
            .insert_if_new(&InsertInput {
                kind: "extern-repo".into(),
                path: source.to_string_lossy().into_owned(),
                repo_root: None,
                branch: None,
                agent_id: None,
                now_unix: 0,
            })
            .unwrap();
        let entry = registry.list(None).unwrap().pop().unwrap();
        let error = remove_entry_and_delete_row(&registry, &entry).unwrap_err();
        assert!(error.contains("refusing unaudited deletion"));
        assert!(source.join("work").exists());
        assert_eq!(registry.list(None).unwrap().len(), 1);
    }
}

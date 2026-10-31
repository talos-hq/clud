//! Durable, bounded freshness scan for session-temp candidates (#1260).
//!
//! The cursor records completed entries, never an inference of idleness from
//! budget exhaustion. A caller must complete the scan before any deletion.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use super::delete_audit;
use super::session_tmp::{PendingPhases, MAX_NESTED_DEPTH, SIZE_REPORT_THRESHOLD, STALE_THRESHOLD};

const SCHEMA_VERSION: u8 = 8;
const CANDIDATE_QUANTUM: usize = 4_096;
const PERSISTENT_FAILURE_HORIZON_SECS: u64 = 72 * 60 * 60;
/// Consecutive failures after which one queued item is dropped (#1672).
/// Dropping spares the path: nothing is deleted, and the next pass
/// re-derives it from the filesystem. Without the bound one root-owned file
/// kept the queue non-empty, so no new pass ever started. With the
/// one-minute-per-retry backoff, eight failures span about 36 minutes.
pub(super) const MAX_ITEM_RETRIES: u32 = 8;

#[derive(Debug, Default)]
pub(super) struct TickReport {
    pub(super) advanced_steps: usize,
    pub(super) examined: u64,
    pub(super) removed_files: u64,
    pub(super) removed_dirs: u64,
    pub(super) reclaimed_bytes: u64,
    pub(super) pending: usize,
    pub(super) phases: PendingPhases,
    pub(super) current_phase: Option<&'static str>,
    pub(super) current_path: Option<PathBuf>,
    pub(super) cursor_examined: Option<usize>,
    pub(super) failures: usize,
    pub(super) pending_age_secs: u64,
    pub(super) no_progress_secs: u64,
    pub(super) retry_count: u32,
    pub(super) last_error_path: Option<PathBuf>,
    pub(super) last_error_class: Option<String>,
    pub(super) last_error_message: Option<String>,
    pub(super) persistent_failure: bool,
    /// Items given up on after [`MAX_ITEM_RETRIES`] failures (#1672).
    pub(super) abandoned: usize,
    pub(super) oversized: Vec<(PathBuf, u64)>,
    /// Subtrees this pass could not remove because another user owns them or
    /// the OS refused access (#2486). Skipped on the first hit, never retried.
    pub(super) foreign_owned: Vec<PathBuf>,
}

/// Bounded daemon tick. The queue is persisted after every tick and only
/// retired when all work has completed. A delayed or failed item stays in the
/// queue while other candidates continue to make progress.
pub(super) fn sweep_tick_at(
    root: &Path,
    work_path: &Path,
    now: SystemTime,
    dry_run: bool,
    allowance: usize,
) -> io::Result<TickReport> {
    sweep_tick_with_quantum(root, work_path, now, dry_run, allowance, CANDIDATE_QUANTUM)
}

#[expect(
    clippy::too_many_lines,
    reason = "complexity ratchet baseline (zackees/ci.yml#229); split this function"
)]
pub(super) fn sweep_tick_with_quantum(
    root: &Path,
    work_path: &Path,
    now: SystemTime,
    dry_run: bool,
    allowance: usize,
    candidate_quantum: usize,
) -> io::Result<TickReport> {
    let mut report = TickReport::default();
    if !root.exists() {
        match fs::remove_file(work_path) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
        return Ok(report);
    }
    let mut state = match WorkState::load(work_path, root) {
        Ok(Some(state)) => state,
        Ok(None) => WorkState::new(root, now),
        Err(err) if err.kind() == io::ErrorKind::InvalidData => {
            // Preserve the evidence but do not let a torn/old-version state
            // file disable GC forever. Restarting from the root is safe:
            // every candidate still needs a fresh idle proof.
            quarantine_bad_state(work_path)?;
            report.failures += 1;
            WorkState::new(root, now)
        }
        Err(err) => return Err(err),
    };
    let mut remaining = allowance;
    let mut stalled = 0usize;
    while remaining > 0 && !state.queue.is_empty() {
        let item = state.queue.pop_front().expect("nonempty queue");
        let before = state.queue.len();
        let grant = remaining.min(candidate_quantum.max(1));
        let spent = advance_or_retire(item, &mut state, &mut report, now, dry_run, grant);
        report.advanced_steps = report.advanced_steps.saturating_add(spent);
        remaining = remaining.saturating_sub(spent);
        if spent == 0 {
            stalled += 1;
            if stalled > state.queue.len().max(before) {
                break;
            }
        } else {
            stalled = 0;
        }
    }
    report.pending = state.queue.len();
    for item in &state.queue {
        match item {
            WorkItem::Explore { .. } => report.phases.exploring += 1,
            WorkItem::Probe { .. } => report.phases.probing += 1,
            WorkItem::Scan { .. } => report.phases.scanning += 1,
            WorkItem::Recheck { .. } => report.phases.rechecking += 1,
            WorkItem::FlatFile { .. } => report.phases.deleting_files += 1,
            WorkItem::Delete { .. } => report.phases.deleting_dirs += 1,
        }
    }
    if let Some(item) = state.queue.front() {
        match item {
            WorkItem::Explore { path, .. } => {
                report.current_phase = Some("explore");
                report.current_path = Some(path.clone());
            }
            WorkItem::Probe { path, .. } => {
                report.current_phase = Some("probe");
                report.current_path = Some(path.clone());
            }
            WorkItem::Scan { cursor, .. } => {
                report.current_phase = Some("scan");
                report.current_path = Some(cursor.root.clone());
                report.cursor_examined = Some(cursor.examined);
            }
            WorkItem::Recheck { cursor, .. } => {
                report.current_phase = Some("recheck");
                report.current_path = Some(cursor.root.clone());
                report.cursor_examined = Some(cursor.examined);
            }
            WorkItem::FlatFile { path, .. } => {
                report.current_phase = Some("file-delete");
                report.current_path = Some(path.clone());
            }
            WorkItem::Delete { path, .. } => {
                report.current_phase = Some("dir-delete");
                report.current_path = Some(path.clone());
            }
        }
    }
    report.pending_age_secs = unix_secs(now).saturating_sub(state.started_unix_secs);
    report.no_progress_secs = unix_secs(now).saturating_sub(state.last_progress_unix_secs);
    report.retry_count = state.queue.iter().fold(0, |maximum, item| match item {
        WorkItem::Explore { retry, .. }
        | WorkItem::Scan { retry, .. }
        | WorkItem::Recheck { retry, .. } => maximum.max(retry.count),
        WorkItem::Probe { retry_count, .. }
        | WorkItem::FlatFile { retry_count, .. }
        | WorkItem::Delete { retry_count, .. } => maximum.max(*retry_count),
    });
    if let Some(failure) = &state.last_failure {
        report.last_error_path = Some(failure.path.0.clone());
        report.last_error_class = Some(failure.class.clone());
        report.last_error_message = Some(failure.message.clone());
    }
    report.persistent_failure = report.pending > 0
        && report.retry_count >= 2
        && report.no_progress_secs >= PERSISTENT_FAILURE_HORIZON_SECS;
    report.foreign_owned = state.foreign_owned.iter().map(|p| p.0.clone()).collect();
    if state.queue.is_empty() {
        match fs::remove_file(work_path) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
    } else {
        state.save(work_path)?;
    }
    Ok(report)
}

/// #2486: retire an item under an unremovable subtree before doing any work
/// on it; otherwise advance it normally.
fn advance_or_retire(
    item: WorkItem,
    state: &mut WorkState,
    report: &mut TickReport,
    now: SystemTime,
    dry_run: bool,
    grant: usize,
) -> usize {
    if state.retire_if_foreign(item.target().0) {
        return 1;
    }
    advance_item(item, state, report, now, dry_run, grant)
}

#[expect(
    clippy::too_many_lines,
    clippy::cognitive_complexity,
    reason = "complexity ratchet baseline (zackees/ci.yml#229); split this function"
)]
fn advance_item(
    item: WorkItem,
    state: &mut WorkState,
    report: &mut TickReport,
    now: SystemTime,
    dry_run: bool,
    grant: usize,
) -> usize {
    if item.retry_count() >= MAX_ITEM_RETRIES {
        // Spare it and let the pass retire; `last_failure` keeps the cause.
        report.abandoned += 1;
        return 1;
    }
    if item.deferred(now) {
        state.queue.push_back(item);
        return 0;
    }
    let (target, directory) = item.target();
    match safe_path_now(&state.root, target, directory) {
        Ok(true) => {}
        Ok(false) => {
            let err = io::Error::new(io::ErrorKind::InvalidData, "unsafe session-tmp work path");
            note_error(state, report, target, &err, now);
            return 1;
        }
        Err(err) => {
            note_error(state, report, target, &err, now);
            retry_item(state, item, now);
            return 1;
        }
    }
    match item {
        WorkItem::Explore {
            path,
            depth,
            mut after,
            retry,
        } => {
            if retry.deferred(now) {
                state.queue.push_back(WorkItem::Explore {
                    path,
                    depth,
                    after,
                    retry,
                });
                return 0;
            }
            let entries = match read_dir_after(&path, after.as_ref()) {
                Ok(entries) => entries,
                Err(err) if err.kind() == io::ErrorKind::NotFound => return 1,
                Err(err) => {
                    note_error(state, report, &path, &err, now);
                    state.queue.push_back(WorkItem::Explore {
                        path,
                        depth,
                        after,
                        retry: retry.after_failure(now),
                    });
                    return 0;
                }
            };
            let mut examined = 0usize;
            let mut directory_read_failed = false;
            for entry in entries.take(grant) {
                examined += 1;
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(err) => {
                        note_error(state, report, &path, &err, now);
                        directory_read_failed = true;
                        continue;
                    }
                };
                let child = entry.path();
                match safe_path_now(&state.root, &child, false) {
                    Ok(true) => {}
                    Ok(false) => {
                        let err = io::Error::new(
                            io::ErrorKind::InvalidData,
                            "unsafe session-tmp child path",
                        );
                        note_error(state, report, &child, &err, now);
                        directory_read_failed = true;
                        continue;
                    }
                    Err(err) => {
                        note_error(state, report, &child, &err, now);
                        directory_read_failed = true;
                        continue;
                    }
                }
                after = Some(EncodedPath(child.clone()));
                let meta = match fs::symlink_metadata(&child) {
                    Ok(meta) => meta,
                    Err(err) => {
                        note_error(state, report, &child, &err, now);
                        queue_probe_retry(state, child, depth + 1, 0, now);
                        continue;
                    }
                };
                let mtime = match meta.modified() {
                    Ok(mtime) => mtime,
                    Err(err) => {
                        note_error(state, report, &child, &err, now);
                        queue_probe_retry(state, child, depth + 1, 0, now);
                        continue;
                    }
                };
                let stale = matches!(now.duration_since(mtime), Ok(age) if age > STALE_THRESHOLD);
                if meta.is_dir() {
                    state.queue.push_back(WorkItem::Scan {
                        cursor: ScanCursor::new(&child),
                        depth: depth + 1,
                        eligible_for_delete: stale,
                        retry: RetryState::default(),
                    });
                } else if stale {
                    state.queue.push_back(WorkItem::FlatFile {
                        path: child,
                        retry_count: 0,
                        retry_after_unix_secs: 0,
                    });
                }
            }
            report.examined += examined as u64;
            state.examined += examined as u64;
            if examined > 0 && retry.count == 0 && !directory_read_failed {
                state.last_progress_unix_secs = unix_secs(now);
            }
            if directory_read_failed || examined == grant {
                state.queue.push_back(WorkItem::Explore {
                    path,
                    depth,
                    after: if directory_read_failed { None } else { after },
                    retry: if directory_read_failed {
                        retry.after_failure(now)
                    } else {
                        RetryState::default()
                    },
                });
            }
            examined.max(1)
        }
        WorkItem::Probe {
            path,
            depth,
            retry_count,
            retry_after_unix_secs,
        } => {
            if unix_secs(now) < retry_after_unix_secs {
                state.queue.push_back(WorkItem::Probe {
                    path,
                    depth,
                    retry_count,
                    retry_after_unix_secs,
                });
                return 0;
            }
            let meta = match fs::symlink_metadata(&path) {
                Ok(meta) => meta,
                Err(err) if err.kind() == io::ErrorKind::NotFound => return 1,
                Err(err) => {
                    note_error(state, report, &path, &err, now);
                    queue_probe_retry(state, path, depth, retry_count, now);
                    return 1;
                }
            };
            let mtime = match meta.modified() {
                Ok(mtime) => mtime,
                Err(err) => {
                    note_error(state, report, &path, &err, now);
                    queue_probe_retry(state, path, depth, retry_count, now);
                    return 1;
                }
            };
            let stale = matches!(now.duration_since(mtime), Ok(age) if age > STALE_THRESHOLD);
            if meta.is_dir() {
                state.queue.push_back(WorkItem::Scan {
                    cursor: ScanCursor::new(&path),
                    depth,
                    eligible_for_delete: stale,
                    retry: RetryState::default(),
                });
            } else if stale {
                state.queue.push_back(WorkItem::FlatFile {
                    path,
                    retry_count: 0,
                    retry_after_unix_secs: 0,
                });
            }
            1
        }
        WorkItem::Scan {
            mut cursor,
            depth,
            eligible_for_delete,
            retry,
        } => {
            if retry.deferred(now) {
                state.queue.push_back(WorkItem::Scan {
                    cursor,
                    depth,
                    eligible_for_delete,
                    retry,
                });
                return 0;
            }
            if eligible_for_delete
                && fs::symlink_metadata(&cursor.root).is_ok_and(|meta| {
                    recently_changed_unexpectedly(&cursor.root, &meta, &BTreeMap::new(), now)
                })
            {
                return 1;
            }
            let before = cursor.examined();
            let verdict = cursor.advance(now, grant);
            let examined = cursor.examined() - before;
            report.examined += examined as u64;
            state.examined += examined as u64;
            if examined > 0 && retry.count == 0 {
                state.last_progress_unix_secs = unix_secs(now);
            }
            match verdict {
                ScanVerdict::Pending => state.queue.push_back(WorkItem::Scan {
                    cursor,
                    depth,
                    eligible_for_delete,
                    retry,
                }),
                ScanVerdict::Idle if eligible_for_delete => {
                    state.queue.push_back(WorkItem::Recheck {
                        cursor: ScanCursor::new(&cursor.root),
                        depth,
                        touched_dirs: BTreeMap::new(),
                        retry: RetryState::default(),
                    });
                }
                ScanVerdict::Busy | ScanVerdict::Idle => {
                    if cursor.apparent_bytes() >= SIZE_REPORT_THRESHOLD {
                        report
                            .oversized
                            .push((cursor.root.clone(), cursor.apparent_bytes()));
                    }
                    if depth < MAX_NESTED_DEPTH {
                        state.queue.push_back(WorkItem::Explore {
                            path: cursor.root,
                            depth,
                            after: None,
                            retry: RetryState::default(),
                        });
                    }
                }
                ScanVerdict::Inconclusive => {
                    report.failures += 1;
                    if let Some(failure) = cursor.last_failure.clone() {
                        state.last_failure = Some(failure);
                    }
                    state.queue.push_back(WorkItem::Scan {
                        cursor: ScanCursor::new(&cursor.root),
                        depth,
                        eligible_for_delete,
                        retry: retry.after_failure(now),
                    });
                }
            }
            examined.max(1)
        }
        WorkItem::FlatFile {
            path,
            retry_count,
            retry_after_unix_secs,
        } => {
            if unix_secs(now) < retry_after_unix_secs {
                state.queue.push_back(WorkItem::FlatFile {
                    path,
                    retry_count,
                    retry_after_unix_secs,
                });
                return 0;
            }
            let meta = match fs::symlink_metadata(&path) {
                Ok(meta) => meta,
                Err(err) if err.kind() == io::ErrorKind::NotFound => return 1,
                Err(err) => {
                    note_error(state, report, &path, &err, now);
                    queue_flat_retry(state, path, retry_count, now);
                    return 1;
                }
            };
            if meta.is_dir() || recently_changed_unexpectedly(&path, &meta, &BTreeMap::new(), now) {
                return 1;
            }
            if dry_run {
                report.removed_files += 1;
                return 1;
            }
            delete_audit::record("gc.session-tmp", &path, &super::session_tmp::stale_rule());
            match fs::remove_file(&path) {
                Ok(()) => {
                    report.removed_files += 1;
                    state.removed_files += 1;
                    report.reclaimed_bytes = report.reclaimed_bytes.saturating_add(meta.len());
                    state.reclaimed_bytes = state.reclaimed_bytes.saturating_add(meta.len());
                    state.last_progress_unix_secs = unix_secs(now);
                }
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => {
                    note_error(state, report, &path, &err, now);
                    queue_flat_retry(state, path, retry_count, now);
                }
            }
            1
        }
        WorkItem::Recheck {
            mut cursor,
            depth,
            touched_dirs,
            retry,
        } => {
            if retry.deferred(now) {
                state.queue.push_back(WorkItem::Recheck {
                    cursor,
                    depth,
                    touched_dirs,
                    retry,
                });
                return 0;
            }
            let root_meta = match fs::symlink_metadata(&cursor.root) {
                Ok(meta) => meta,
                Err(err) if err.kind() == io::ErrorKind::NotFound => return 1,
                Err(err) => {
                    note_error(state, report, &cursor.root, &err, now);
                    state.queue.push_back(WorkItem::Recheck {
                        cursor: ScanCursor::new(&cursor.root),
                        depth,
                        touched_dirs,
                        retry: retry.after_failure(now),
                    });
                    return 1;
                }
            };
            if recently_changed_unexpectedly(&cursor.root, &root_meta, &touched_dirs, now) {
                return 1;
            }
            let before = cursor.examined();
            let verdict = cursor.advance_with_touched(now, grant, &touched_dirs);
            let examined = cursor.examined() - before;
            report.examined += examined as u64;
            state.examined += examined as u64;
            if examined > 0 && retry.count == 0 {
                state.last_progress_unix_secs = unix_secs(now);
            }
            match verdict {
                ScanVerdict::Pending => state.queue.push_back(WorkItem::Recheck {
                    cursor,
                    depth,
                    touched_dirs,
                    retry,
                }),
                ScanVerdict::Idle => state.queue.push_back(WorkItem::Delete {
                    path: cursor.root,
                    depth,
                    retry_count: 0,
                    retry_after_unix_secs: 0,
                    touched_dirs,
                }),
                ScanVerdict::Busy => {}
                ScanVerdict::Inconclusive => {
                    report.failures += 1;
                    if let Some(failure) = cursor.last_failure.clone() {
                        state.last_failure = Some(failure);
                    }
                    state.queue.push_back(WorkItem::Recheck {
                        cursor: ScanCursor::new(&cursor.root),
                        depth,
                        touched_dirs,
                        retry: retry.after_failure(now),
                    });
                }
            }
            examined.max(1)
        }
        WorkItem::Delete {
            path,
            depth,
            retry_count,
            retry_after_unix_secs,
            mut touched_dirs,
        } => {
            if unix_secs(now) < retry_after_unix_secs {
                state.queue.push_back(WorkItem::Delete {
                    path,
                    depth,
                    retry_count,
                    retry_after_unix_secs,
                    touched_dirs,
                });
                return 0;
            }
            if dry_run {
                report.removed_dirs += 1;
                return 1;
            }
            match delete_candidate_batch(&path, grant, &mut touched_dirs, now, report, state) {
                Ok(BatchOutcome::Complete(spent)) => return spent,
                Ok(BatchOutcome::Pending(spent)) => {
                    state.queue.push_back(WorkItem::Recheck {
                        cursor: ScanCursor::new(&path),
                        depth,
                        touched_dirs,
                        retry: RetryState::default(),
                    });
                    return spent;
                }
                Ok(BatchOutcome::Busy(spent)) => return spent,
                Err(err) => {
                    note_error(state, report, &path, &err, now);
                    queue_retry(state, path, depth, retry_count, touched_dirs, now);
                }
            }
            1
        }
    }
}

/// Stream one directory without collecting or sorting all children. If the
/// saved marker disappeared, revisit from the beginning; duplicate probes
/// are safe, whereas skipping an unknown suffix is not.
fn read_dir_after(path: &Path, after: Option<&EncodedPath>) -> io::Result<fs::ReadDir> {
    let mut entries = fs::read_dir(path)?;
    let Some(after) = after else {
        return Ok(entries);
    };
    for entry in entries.by_ref() {
        if entry?.path() == after.0 {
            return Ok(entries);
        }
    }
    fs::read_dir(path)
}

fn modified_nanos(meta: &fs::Metadata) -> u128 {
    meta.modified()
        .ok()
        .and_then(|mtime| mtime.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |duration| duration.as_nanos())
}

fn recently_changed_unexpectedly(
    path: &Path,
    meta: &fs::Metadata,
    touched_dirs: &BTreeMap<EncodedPath, u128>,
    now: SystemTime,
) -> bool {
    if meta.is_dir() {
        if let Some(expected_mtime) = touched_dirs.get(&EncodedPath(path.to_path_buf())) {
            return modified_nanos(meta) != *expected_mtime;
        }
    }
    let Ok(mtime) = meta.modified() else {
        return true;
    };
    !matches!(now.duration_since(mtime), Ok(age) if age > STALE_THRESHOLD)
}

fn queue_retry(
    state: &mut WorkState,
    path: PathBuf,
    depth: usize,
    prior_retries: u32,
    touched_dirs: BTreeMap<EncodedPath, u128>,
    now: SystemTime,
) {
    let retry_count = prior_retries.saturating_add(1);
    let delay_minutes = u64::from(retry_count.min(60));
    state.queue.push_back(WorkItem::Delete {
        path,
        depth,
        retry_count,
        retry_after_unix_secs: unix_secs(now).saturating_add(delay_minutes * 60),
        touched_dirs,
    });
}

fn retry_item(state: &mut WorkState, item: WorkItem, now: SystemTime) {
    match item {
        WorkItem::Explore {
            path,
            depth,
            after,
            retry,
        } => state.queue.push_back(WorkItem::Explore {
            path,
            depth,
            after,
            retry: retry.after_failure(now),
        }),
        WorkItem::Scan {
            cursor,
            depth,
            eligible_for_delete,
            retry,
        } => state.queue.push_back(WorkItem::Scan {
            cursor,
            depth,
            eligible_for_delete,
            retry: retry.after_failure(now),
        }),
        WorkItem::Recheck {
            cursor,
            depth,
            touched_dirs,
            retry,
        } => state.queue.push_back(WorkItem::Recheck {
            cursor,
            depth,
            touched_dirs,
            retry: retry.after_failure(now),
        }),
        WorkItem::Probe {
            path,
            depth,
            retry_count,
            ..
        } => queue_probe_retry(state, path, depth, retry_count, now),
        WorkItem::FlatFile {
            path, retry_count, ..
        } => queue_flat_retry(state, path, retry_count, now),
        WorkItem::Delete {
            path,
            depth,
            retry_count,
            touched_dirs,
            ..
        } => queue_retry(state, path, depth, retry_count, touched_dirs, now),
    }
}

fn note_error(
    state: &mut WorkState,
    report: &mut TickReport,
    path: &Path,
    err: &io::Error,
    now: SystemTime,
) {
    report.failures += 1;
    if is_terminal_denial(err) && path != state.root {
        // #2486: terminal on the first hit; the re-queued item is retired by
        // `retire_if_foreign` instead of retrying with backoff. The sweep
        // root itself is never recorded: its removal hint would wipe every
        // live scratchpad.
        state.foreign_owned.insert(EncodedPath(path.to_path_buf()));
    }
    state.last_failure = Some(FailureRecord {
        path: EncodedPath(path.to_path_buf()),
        class: format!("{:?}", err.kind()),
        message: err.to_string(),
        seen_unix_secs: unix_secs(now),
    });
}

fn queue_flat_retry(state: &mut WorkState, path: PathBuf, prior_retries: u32, now: SystemTime) {
    let retry_count = prior_retries.saturating_add(1);
    let delay_minutes = u64::from(retry_count.min(60));
    state.queue.push_back(WorkItem::FlatFile {
        path,
        retry_count,
        retry_after_unix_secs: unix_secs(now).saturating_add(delay_minutes * 60),
    });
}

fn queue_probe_retry(
    state: &mut WorkState,
    path: PathBuf,
    depth: usize,
    prior_retries: u32,
    now: SystemTime,
) {
    let retry_count = prior_retries.saturating_add(1);
    let delay_minutes = u64::from(retry_count.min(60));
    state.queue.push_back(WorkItem::Probe {
        path,
        depth,
        retry_count,
        retry_after_unix_secs: unix_secs(now).saturating_add(delay_minutes * 60),
    });
}

enum BatchOutcome {
    Pending(usize),
    Complete(usize),
    Busy(usize),
}

fn delete_candidate_batch(
    path: &Path,
    grant: usize,
    touched_dirs: &mut BTreeMap<EncodedPath, u128>,
    now: SystemTime,
    report: &mut TickReport,
    state: &mut WorkState,
) -> io::Result<BatchOutcome> {
    if !safe_path_now(&state.root, path, true)? {
        return Ok(BatchOutcome::Busy(1));
    }
    let root_meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(BatchOutcome::Complete(1)),
        Err(err) => return Err(err),
    };
    if recently_changed_unexpectedly(path, &root_meta, touched_dirs, now) {
        return Ok(BatchOutcome::Busy(1));
    }
    // Drop all directory iterators before removing anything. Windows can
    // refuse removal when the sweeper itself holds the directory open.
    let batch = WalkDir::new(path)
        .follow_links(false)
        .contents_first(true)
        .min_depth(1)
        .into_iter()
        .take(grant)
        .map(|entry| {
            entry
                .map(|entry| entry.into_path())
                .map_err(io::Error::other)
        })
        .collect::<io::Result<Vec<_>>>()?;
    // A writer can refresh an existing file without changing its parent's
    // mtime. Check the entire selected batch before removing an older sibling;
    // the per-child check below still closes as much of the race as possible.
    for child in &batch {
        if !safe_path_now(&state.root, child, false)? {
            return Ok(BatchOutcome::Busy(1));
        }
        let meta = match fs::symlink_metadata(child) {
            Ok(meta) => meta,
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err),
        };
        if recently_changed_unexpectedly(child, &meta, touched_dirs, now) {
            return Ok(BatchOutcome::Busy(1));
        }
    }
    report.examined += batch.len() as u64;
    state.examined += batch.len() as u64;
    let mut spent = 0;
    for child in &batch {
        spent += 1;
        if !safe_path_now(&state.root, child, false)? {
            return Ok(BatchOutcome::Busy(spent));
        }
        let meta = match fs::symlink_metadata(child) {
            Ok(meta) => meta,
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err),
        };
        if recently_changed_unexpectedly(child, &meta, touched_dirs, now) {
            return Ok(BatchOutcome::Busy(spent));
        }
        delete_audit::record("gc.session-tmp", child, &super::session_tmp::stale_rule());
        if meta.is_dir() {
            remove_allowing_read_only_parent(child, true)?;
            report.removed_dirs += 1;
            state.removed_dirs += 1;
        } else {
            remove_allowing_read_only_parent(child, false)?;
            report.removed_files += 1;
            state.removed_files += 1;
            report.reclaimed_bytes = report.reclaimed_bytes.saturating_add(meta.len());
            state.reclaimed_bytes = state.reclaimed_bytes.saturating_add(meta.len());
        }
        state.last_progress_unix_secs = unix_secs(now);
        if let Some(parent) = child.parent() {
            if let Ok(parent_meta) = fs::symlink_metadata(parent) {
                touched_dirs.insert(
                    EncodedPath(parent.to_path_buf()),
                    modified_nanos(&parent_meta),
                );
            }
        }
    }
    if batch.len() == grant {
        return Ok(BatchOutcome::Pending(spent));
    }
    let root_meta = fs::symlink_metadata(path)?;
    if recently_changed_unexpectedly(path, &root_meta, touched_dirs, now) {
        return Ok(BatchOutcome::Busy(spent.max(1)));
    }
    delete_audit::record("gc.session-tmp", path, &super::session_tmp::stale_rule());
    remove_allowing_read_only_parent(path, true)?;
    report.removed_dirs += 1;
    state.removed_dirs += 1;
    state.last_progress_unix_secs = unix_secs(now);
    Ok(BatchOutcome::Complete(spent + 1))
}

/// Remove one already-vetted entry. A read-only parent (Go's module cache,
/// sealed build outputs) refuses the unlink with `PermissionDenied`; make
/// the parent owner-writable, as safe-rm does (#1573), and try once more.
/// `chmod` changes ctime, not mtime, so the `touched_dirs` checks still
/// hold. A foreign-owned parent stays read-only and the error surfaces.
fn remove_allowing_read_only_parent(path: &Path, directory: bool) -> io::Result<()> {
    let remove = || {
        if directory {
            fs::remove_dir(path)
        } else {
            fs::remove_file(path)
        }
    };
    match remove() {
        Err(err) if err.kind() == io::ErrorKind::PermissionDenied => {
            let Some(parent) = path.parent() else {
                return Err(err);
            };
            if !make_owner_writable(parent) {
                return Err(err);
            }
            remove()
        }
        other => other,
    }
}

use crate::platform::session_tmp_continuation::make_owner_writable;

/// The whole sweep is a durable queue, not a one-shot walk. A candidate that
/// uses up this tick's allowance goes to the back so later candidates still
/// get a turn. `Explore` only enumerates one directory level; `Scan` proves a
/// potentially removable subtree idle before it can enter `Delete`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct WorkState {
    schema_version: u8,
    #[serde(with = "path_serde")]
    root: PathBuf,
    started_unix_secs: u64,
    last_progress_unix_secs: u64,
    examined: u64,
    removed_files: u64,
    removed_dirs: u64,
    reclaimed_bytes: u64,
    last_failure: Option<FailureRecord>,
    /// #2486: subtrees skipped as unremovable (foreign owner or denied).
    #[serde(default)]
    foreign_owned: BTreeSet<EncodedPath>,
    queue: VecDeque<WorkItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FailureRecord {
    path: EncodedPath,
    class: String,
    message: String,
    seen_unix_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
struct EncodedPath(#[serde(with = "path_serde")] PathBuf);

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct RetryState {
    count: u32,
    after_unix_secs: u64,
}

impl RetryState {
    fn deferred(&self, now: SystemTime) -> bool {
        unix_secs(now) < self.after_unix_secs
    }

    fn after_failure(mut self, now: SystemTime) -> Self {
        self.count = self.count.saturating_add(1);
        self.after_unix_secs = unix_secs(now).saturating_add(u64::from(self.count.min(60)) * 60);
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum WorkItem {
    Explore {
        #[serde(with = "path_serde")]
        path: PathBuf,
        depth: usize,
        after: Option<EncodedPath>,
        retry: RetryState,
    },
    Probe {
        #[serde(with = "path_serde")]
        path: PathBuf,
        depth: usize,
        retry_count: u32,
        retry_after_unix_secs: u64,
    },
    Scan {
        cursor: ScanCursor,
        depth: usize,
        eligible_for_delete: bool,
        retry: RetryState,
    },
    FlatFile {
        #[serde(with = "path_serde")]
        path: PathBuf,
        retry_count: u32,
        retry_after_unix_secs: u64,
    },
    Recheck {
        cursor: ScanCursor,
        depth: usize,
        touched_dirs: BTreeMap<EncodedPath, u128>,
        retry: RetryState,
    },
    Delete {
        #[serde(with = "path_serde")]
        path: PathBuf,
        depth: usize,
        retry_count: u32,
        retry_after_unix_secs: u64,
        /// Directory mtimes changed by our own successful removals. A later
        /// unexpected mtime change is treated as concurrent activity.
        touched_dirs: BTreeMap<EncodedPath, u128>,
    },
}

impl WorkState {
    fn new(root: &Path, now: SystemTime) -> Self {
        let started_unix_secs = unix_secs(now);
        Self {
            schema_version: SCHEMA_VERSION,
            root: root.to_path_buf(),
            started_unix_secs,
            last_progress_unix_secs: started_unix_secs,
            examined: 0,
            removed_files: 0,
            removed_dirs: 0,
            reclaimed_bytes: 0,
            last_failure: None,
            foreign_owned: BTreeSet::new(),
            queue: VecDeque::from([WorkItem::Explore {
                path: root.to_path_buf(),
                depth: 0,
                after: None,
                retry: RetryState::default(),
            }]),
        }
    }

    fn load(path: &Path, root: &Path) -> io::Result<Option<Self>> {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err),
        };
        let state: Self = serde_json::from_slice(&bytes)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        if state.schema_version != SCHEMA_VERSION
            || state.root != root
            || !state.queue.iter().all(|item| item.is_within(root))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "session-tmp work state contains an invalid sweep path",
            ));
        }
        Ok(Some(state))
    }

    fn save(&self, path: &Path) -> io::Result<()> {
        save_json(path, self)
    }

    /// #2486: drop work under a subtree already found unremovable, and mark a
    /// directory owned by another user unremovable before touching it.
    fn retire_if_foreign(&mut self, target: &Path) -> bool {
        if target
            .ancestors()
            .any(|a| self.foreign_owned.contains(&EncodedPath(a.to_path_buf())))
        {
            return true;
        }
        if target == self.root {
            return false;
        }
        let foreign = fs::symlink_metadata(target)
            .is_ok_and(|meta| meta.is_dir() && owned_by_other_user(&meta));
        if foreign {
            self.foreign_owned.insert(EncodedPath(target.to_path_buf()));
        }
        foreign
    }
}

/// #2486: `PermissionDenied` is terminal only on Unix, where it means a mode
/// or owner the running user cannot change. On Windows it is also what a
/// briefly locked file reports, so there it keeps the backoff retry.
fn is_terminal_denial(err: &io::Error) -> bool {
    crate::platform::host::UNIX && err.kind() == io::ErrorKind::PermissionDenied
}

/// True when `meta` names a file whose owner is not the running user. Hosts
/// without uids (Windows) report `false`: the denied-access path covers them.
fn owned_by_other_user(meta: &fs::Metadata) -> bool {
    match (
        crate::platform::rm_tool::file_owner(meta),
        crate::platform::rm_tool::effective_uid(),
    ) {
        (Some(owner), Some(me)) => owner != me,
        _ => false,
    }
}

fn is_within(root: &Path, path: &Path, allow_root: bool) -> bool {
    if path == root {
        return allow_root;
    }
    path.strip_prefix(root).is_ok_and(|relative| {
        relative
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
            && relative.components().next().is_some()
    })
}

/// Recheck directory components when a persisted path is used: a path can
/// remain lexically beneath the root after an ancestor is replaced by a
/// symlink to somewhere else. The final component may be a symlink only for
/// file work, where unlinking it removes the link rather than its target.
fn safe_path_now(root: &Path, path: &Path, directory: bool) -> io::Result<bool> {
    if !is_within(root, path, true) {
        return Ok(false);
    }
    let Ok(relative) = path.strip_prefix(root) else {
        return Ok(false);
    };
    let components = relative.components().collect::<Vec<_>>();
    let mut current = root.to_path_buf();
    for (index, component) in components.iter().enumerate() {
        current.push(component.as_os_str());
        if index + 1 < components.len() {
            match fs::symlink_metadata(&current) {
                Ok(meta) if meta.is_dir() => {}
                Ok(_) => return Ok(false),
                Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
                Err(err) => return Err(err),
            }
        } else if directory {
            match fs::symlink_metadata(&current) {
                Ok(meta) if meta.file_type().is_symlink() => return Ok(false),
                Ok(_) => {}
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => return Err(err),
            }
        }
    }
    if components.is_empty() && directory {
        match fs::symlink_metadata(root) {
            Ok(meta) if meta.file_type().is_symlink() => return Ok(false),
            Ok(_) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
    }
    Ok(true)
}

impl WorkItem {
    /// The path this item acts on, and whether it must be a directory.
    fn target(&self) -> (&Path, bool) {
        match self {
            Self::Explore { path, .. } | Self::Delete { path, .. } => (path.as_path(), true),
            Self::Scan { cursor, .. } | Self::Recheck { cursor, .. } => {
                (cursor.root.as_path(), true)
            }
            Self::Probe { path, .. } | Self::FlatFile { path, .. } => (path.as_path(), false),
        }
    }

    fn retry_count(&self) -> u32 {
        match self {
            Self::Explore { retry, .. }
            | Self::Scan { retry, .. }
            | Self::Recheck { retry, .. } => retry.count,
            Self::Probe { retry_count, .. }
            | Self::FlatFile { retry_count, .. }
            | Self::Delete { retry_count, .. } => *retry_count,
        }
    }

    fn deferred(&self, now: SystemTime) -> bool {
        match self {
            Self::Explore { retry, .. }
            | Self::Scan { retry, .. }
            | Self::Recheck { retry, .. } => retry.deferred(now),
            Self::Probe {
                retry_after_unix_secs,
                ..
            }
            | Self::FlatFile {
                retry_after_unix_secs,
                ..
            }
            | Self::Delete {
                retry_after_unix_secs,
                ..
            } => unix_secs(now) < *retry_after_unix_secs,
        }
    }

    fn is_within(&self, root: &Path) -> bool {
        match self {
            Self::Explore { path, after, .. } => {
                is_within(root, path, true)
                    && after.as_ref().is_none_or(|after| {
                        is_within(root, &after.0, false) && after.0.parent() == Some(path.as_path())
                    })
            }
            Self::Probe { path, .. } | Self::FlatFile { path, .. } => is_within(root, path, false),
            Self::Delete {
                path, touched_dirs, ..
            } => {
                is_within(root, path, false)
                    && touched_dirs
                        .keys()
                        .all(|touched| is_within(path, &touched.0, true))
            }
            Self::Scan { cursor, .. } => cursor.is_within(root),
            Self::Recheck {
                cursor,
                touched_dirs,
                ..
            } => {
                cursor.is_within(root)
                    && touched_dirs
                        .keys()
                        .all(|touched| is_within(&cursor.root, &touched.0, true))
            }
        }
    }
}

impl ScanCursor {
    fn is_within(&self, root: &Path) -> bool {
        is_within(root, &self.root, false)
            && self
                .dir_mtimes
                .keys()
                .all(|dir| is_within(&self.root, &dir.0, true))
    }
}

fn unix_secs(now: SystemTime) -> u64 {
    now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

fn quarantine_bad_state(path: &Path) -> io::Result<()> {
    let preferred = path.with_extension("corrupt.json");
    let destination = if preferred.exists() {
        path.with_extension(format!(
            "corrupt-{}.json",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ))
    } else {
        preferred
    };
    fs::rename(path, destination)
}

fn save_json(path: &Path, value: &impl Serialize) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("missing state parent"))?;
    fs::create_dir_all(parent)?;
    let temp = path.with_extension(format!(
        "tmp-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
    fs::write(&temp, bytes)?;
    fs::rename(&temp, path)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct ScanCursor {
    schema_version: u8,
    #[serde(with = "path_serde")]
    root: PathBuf,
    examined: usize,
    apparent_bytes: u64,
    recent: bool,
    inconclusive: bool,
    last_failure: Option<FailureRecord>,
    /// #2486: unremovable subtrees found by this walk and skipped.
    #[serde(default)]
    foreign: BTreeSet<EncodedPath>,
    /// A sorted walk can shift when a previously visited directory changes.
    /// Its old entry count then ceases to be a safe resume position.
    dir_mtimes: BTreeMap<EncodedPath, u128>,
}

/// JSON cannot encode arbitrary OS path bytes. Store them as base64 instead
/// of lossy UTF-8 so a foreign tool's non-Unicode temp name is never changed
/// into a different deletion target on resume.
mod path_serde {
    use super::*;
    use crate::platform::session_tmp_continuation::{bytes_path, path_bytes};
    use serde::{Deserializer, Serializer};

    pub fn serialize<S>(path: &Path, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(
            &base64::engine::general_purpose::STANDARD_NO_PAD.encode(path_bytes(path)),
        )
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<PathBuf, D::Error>
    where
        D: Deserializer<'de>,
    {
        let encoded = String::deserialize(deserializer)?;
        let bytes = base64::engine::general_purpose::STANDARD_NO_PAD
            .decode(encoded)
            .map_err(serde::de::Error::custom)?;
        bytes_path(&bytes).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ScanVerdict {
    Pending,
    Idle,
    Busy,
    Inconclusive,
}

impl ScanCursor {
    fn capture_error(&mut self, path: &Path, err: &io::Error, now: SystemTime) {
        self.inconclusive = true;
        self.last_failure = Some(FailureRecord {
            path: EncodedPath(path.to_path_buf()),
            class: format!("{:?}", err.kind()),
            message: err.to_string(),
            seen_unix_secs: unix_secs(now),
        });
    }

    pub(super) fn new(root: &Path) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            root: root.to_path_buf(),
            examined: 0,
            apparent_bytes: 0,
            recent: false,
            inconclusive: false,
            last_failure: None,
            foreign: BTreeSet::new(),
            dir_mtimes: BTreeMap::new(),
        }
    }

    /// #2486: classify a walk entry or error as an unremovable subtree. A
    /// directory owned by another user is skipped without descending; a
    /// `PermissionDenied` is recorded once instead of failing the scan. Both
    /// walks (prefix revalidation and new entries) call this, so they stay in
    /// step.
    fn absorb_foreign(
        &mut self,
        walk: &mut walkdir::IntoIter,
        entry: &walkdir::Result<walkdir::DirEntry>,
    ) -> bool {
        match entry {
            Ok(entry) => {
                let foreign = entry.file_type().is_dir()
                    && fs::symlink_metadata(entry.path())
                        .is_ok_and(|meta| owned_by_other_user(&meta));
                if foreign {
                    self.foreign.insert(EncodedPath(entry.path().to_path_buf()));
                    walk.skip_current_dir();
                }
                foreign
            }
            Err(err) if err.io_error().is_some_and(is_terminal_denial) => {
                let path = err.path().unwrap_or(&self.root).to_path_buf();
                self.foreign.insert(EncodedPath(path));
                true
            }
            Err(_) => false,
        }
    }

    pub(super) fn examined(&self) -> usize {
        self.examined
    }

    pub(super) fn apparent_bytes(&self) -> u64 {
        self.apparent_bytes
    }

    /// Advance by at most `allowance` yielded walk entries. Reopening the
    /// sorted walk costs directory enumeration, but never repeats metadata
    /// accounting; the persisted cursor survives daemon restarts.
    pub(super) fn advance(&mut self, now: SystemTime, allowance: usize) -> ScanVerdict {
        self.advance_with_touched(now, allowance, &BTreeMap::new())
    }

    /// Every directory seen so far must be unchanged; on the first call this
    /// records the root's own mtime. `None` means carry on walking.
    fn check_known_dirs(&mut self, now: SystemTime) -> Option<ScanVerdict> {
        let mut directory_error = None;
        for (dir, expected_mtime) in &self.dir_mtimes {
            let meta = match fs::symlink_metadata(&dir.0) {
                Ok(meta) => meta,
                Err(err) if err.kind() == io::ErrorKind::NotFound => {
                    return Some(ScanVerdict::Busy)
                }
                Err(err) => {
                    directory_error = Some((dir.0.clone(), err));
                    break;
                }
            };
            if !meta.is_dir() || modified_nanos(&meta) != *expected_mtime {
                return Some(ScanVerdict::Busy);
            }
        }
        if let Some((path, err)) = directory_error {
            self.capture_error(&path, &err, now);
            return Some(ScanVerdict::Inconclusive);
        }
        if self.dir_mtimes.is_empty() {
            let meta = match fs::symlink_metadata(&self.root) {
                Ok(meta) => meta,
                // Gone before we started: nothing to prove idle or delete.
                // Busy never deletes (#1672: this was retried for 8 days).
                Err(err) if err.kind() == io::ErrorKind::NotFound => {
                    return Some(ScanVerdict::Busy)
                }
                Err(err) => {
                    let path = self.root.clone();
                    self.capture_error(&path, &err, now);
                    return Some(ScanVerdict::Inconclusive);
                }
            };
            if !meta.is_dir() {
                return Some(ScanVerdict::Busy);
            }
            self.dir_mtimes
                .insert(EncodedPath(self.root.clone()), modified_nanos(&meta));
        }
        None
    }

    /// Reopened walks must validate the prefix, not merely skip its entry
    /// count: a file can become active without changing its parent's mtime.
    fn revalidate_prefix(
        &mut self,
        walk: &mut walkdir::IntoIter,
        now: SystemTime,
        touched_dirs: &BTreeMap<EncodedPath, u128>,
    ) -> Option<ScanVerdict> {
        for _ in 0..self.examined {
            let Some(entry) = walk.next() else {
                return Some(ScanVerdict::Busy);
            };
            if self.absorb_foreign(walk, &entry) {
                continue;
            }
            let entry = match entry {
                Ok(entry) => entry,
                Err(err) => {
                    let path = err.path().unwrap_or(&self.root).to_path_buf();
                    let kind = err.io_error().map_or(io::ErrorKind::Other, io::Error::kind);
                    self.capture_error(&path, &io::Error::new(kind, err.to_string()), now);
                    return Some(ScanVerdict::Inconclusive);
                }
            };
            let meta = match fs::symlink_metadata(entry.path()) {
                Ok(meta) => meta,
                Err(err) if err.kind() == io::ErrorKind::NotFound => {
                    return Some(ScanVerdict::Busy)
                }
                Err(err) => {
                    self.capture_error(entry.path(), &err, now);
                    return Some(ScanVerdict::Inconclusive);
                }
            };
            if recently_changed_unexpectedly(entry.path(), &meta, touched_dirs, now) {
                return Some(ScanVerdict::Busy);
            }
        }
        None
    }

    fn advance_with_touched(
        &mut self,
        now: SystemTime,
        allowance: usize,
        touched_dirs: &BTreeMap<EncodedPath, u128>,
    ) -> ScanVerdict {
        if allowance == 0 {
            return ScanVerdict::Pending;
        }
        if let Some(verdict) = self.check_known_dirs(now) {
            return verdict;
        }
        let mut walk = WalkDir::new(&self.root)
            .follow_links(false)
            .min_depth(1)
            .into_iter();
        if let Some(verdict) = self.revalidate_prefix(&mut walk, now, touched_dirs) {
            return verdict;
        }
        let mut unseen = walk;
        let mut processed = 0usize;
        while processed < allowance {
            let Some(entry) = unseen.next() else {
                return self.verdict();
            };
            self.examined = self.examined.saturating_add(1);
            processed += 1;
            if self.absorb_foreign(&mut unseen, &entry) {
                continue;
            }
            let entry = match entry {
                Ok(entry) => entry,
                Err(err) => {
                    let path = err.path().unwrap_or(&self.root).to_path_buf();
                    let kind = err.io_error().map_or(io::ErrorKind::Other, io::Error::kind);
                    self.capture_error(&path, &io::Error::new(kind, err.to_string()), now);
                    continue;
                }
            };
            let meta = match fs::symlink_metadata(entry.path()) {
                Ok(meta) => meta,
                Err(err) => {
                    self.capture_error(entry.path(), &err, now);
                    continue;
                }
            };
            if let Err(err) = meta.modified() {
                self.capture_error(entry.path(), &err, now);
                continue;
            }
            if !meta.is_dir() {
                self.apparent_bytes = self.apparent_bytes.saturating_add(meta.len());
            } else {
                self.dir_mtimes.insert(
                    EncodedPath(entry.path().to_path_buf()),
                    modified_nanos(&meta),
                );
            }
            if recently_changed_unexpectedly(entry.path(), &meta, touched_dirs, now) {
                self.recent = true;
            }
        }
        if unseen.next().is_none() {
            self.verdict()
        } else {
            ScanVerdict::Pending
        }
    }

    fn verdict(&self) -> ScanVerdict {
        if self.inconclusive {
            ScanVerdict::Inconclusive
        } else if self.recent || !self.foreign.is_empty() {
            // A subtree holding an unremovable part cannot be deleted whole;
            // Busy keeps it and lets Explore reach its removable siblings.
            ScanVerdict::Busy
        } else {
            ScanVerdict::Idle
        }
    }

    #[cfg(test)]
    pub(super) fn load(path: &Path, root: &Path) -> io::Result<Option<Self>> {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err),
        };
        let value: Self = serde_json::from_slice(&bytes)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        if value.schema_version != SCHEMA_VERSION || value.root != root {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "session-tmp cursor does not match this candidate",
            ));
        }
        Ok(Some(value))
    }

    #[cfg(test)]
    pub(super) fn save(&self, path: &Path) -> io::Result<()> {
        save_json(path, self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tempfile::tempdir;

    #[test]
    fn scan_cursor_survives_reload_and_finishes_after_multiple_quanta() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("candidate");
        fs::create_dir(&root).unwrap();
        let old = SystemTime::now() - STALE_THRESHOLD - Duration::from_secs(3_600);
        for index in 0..6 {
            let path = root.join(format!("{index}.txt"));
            fs::write(&path, b"x").unwrap();
            filetime::set_file_mtime(&path, filetime::FileTime::from_system_time(old)).unwrap();
        }
        let cursor_path = temp.path().join("state/cursor.json");
        let now = SystemTime::now();
        let mut verdict = ScanVerdict::Pending;
        for _ in 0..8 {
            let mut cursor = ScanCursor::load(&cursor_path, &root)
                .unwrap()
                .unwrap_or_else(|| ScanCursor::new(&root));
            verdict = cursor.advance(now, 2);
            cursor.save(&cursor_path).unwrap();
            if verdict != ScanVerdict::Pending {
                break;
            }
        }
        assert_eq!(verdict, ScanVerdict::Idle);
        let reloaded = ScanCursor::load(&cursor_path, &root).unwrap().unwrap();
        assert_eq!(reloaded.examined(), 6);
        assert_eq!(reloaded.apparent_bytes(), 6);
    }

    #[test]
    fn cursor_preserves_non_utf8_path_across_reload() {
        if !crate::platform::host::UNIX {
            return;
        }

        let temp = tempdir().unwrap();
        let root = temp.path().join(
            crate::platform::gc_test_fs::os_string_from_bytes(b"candidate-\xff".to_vec()).unwrap(),
        );
        // APFS may reject invalid UTF-8 names even though PathBuf can carry
        // them; serialization does not need a real directory on disk.
        let cursor_path = temp.path().join("cursor.json");
        ScanCursor::new(&root).save(&cursor_path).unwrap();
        assert!(ScanCursor::load(&cursor_path, &root).unwrap().is_some());
    }

    #[test]
    fn partial_scan_never_proves_idle_and_later_recent_file_keeps_tree() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("candidate");
        fs::create_dir(&root).unwrap();
        let now = SystemTime::now();
        let old = now - STALE_THRESHOLD - Duration::from_secs(3_600);
        for name in ["a-old", "b-old"] {
            let path = root.join(name);
            fs::write(&path, b"old").unwrap();
            filetime::set_file_mtime(&path, filetime::FileTime::from_system_time(old)).unwrap();
        }
        fs::write(root.join("z-recent"), b"live").unwrap();

        let mut cursor = ScanCursor::new(&root);
        assert_eq!(cursor.advance(now, 2), ScanVerdict::Pending);
        assert_eq!(cursor.advance(now, 2), ScanVerdict::Busy);
        assert!(cursor.examined() >= 2);
    }

    #[test]
    fn insertion_before_saved_scan_offset_cannot_prove_idle() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("candidate");
        fs::create_dir(&root).unwrap();
        let now = SystemTime::now();
        let old = now - STALE_THRESHOLD - Duration::from_secs(3_600);
        for name in ["a-old", "b-old", "c-old"] {
            let path = root.join(name);
            fs::write(&path, b"old").unwrap();
            filetime::set_file_mtime(&path, filetime::FileTime::from_system_time(old)).unwrap();
        }
        filetime::set_file_mtime(&root, filetime::FileTime::from_system_time(old)).unwrap();

        let mut cursor = ScanCursor::new(&root);
        assert_eq!(cursor.advance(now, 1), ScanVerdict::Pending);
        fs::write(root.join("0-new-live"), b"live").unwrap();
        assert_ne!(cursor.advance(now, 10), ScanVerdict::Idle);
    }

    #[test]
    fn changed_file_before_saved_scan_offset_cannot_prove_idle() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("candidate");
        fs::create_dir(&root).unwrap();
        let now = SystemTime::now();
        let old = now - STALE_THRESHOLD - Duration::from_secs(3_600);
        for name in ["a-old", "b-old", "c-old"] {
            let path = root.join(name);
            fs::write(&path, b"old").unwrap();
            filetime::set_file_mtime(&path, filetime::FileTime::from_system_time(old)).unwrap();
        }
        let mut cursor = ScanCursor::new(&root);
        assert_eq!(cursor.advance(now, 1), ScanVerdict::Pending);
        filetime::set_file_mtime(
            root.join("a-old"),
            filetime::FileTime::from_system_time(now),
        )
        .unwrap();
        assert_ne!(cursor.advance(now, 10), ScanVerdict::Idle);
    }

    #[test]
    fn fresh_direct_child_is_sized_and_reported_without_deletion() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("tmp");
        let candidate = root.join("large-active-session");
        fs::create_dir_all(&candidate).unwrap();
        let file = candidate.join("sparse-scratchpad");
        fs::File::create(&file)
            .unwrap()
            .set_len(SIZE_REPORT_THRESHOLD + 1)
            .unwrap();
        let work_path = temp.path().join("work.json");
        let mut reported = false;
        for _ in 0..12 {
            let report = sweep_tick_at(&root, &work_path, SystemTime::now(), false, 10).unwrap();
            reported |= report
                .oversized
                .iter()
                .any(|(path, size)| path == &candidate && *size >= SIZE_REPORT_THRESHOLD);
            if report.pending == 0 {
                break;
            }
        }
        assert!(reported, "a large live direct child must be observable");
        assert!(file.exists(), "fresh data must be kept");
    }

    #[test]
    fn work_queue_survives_reload_including_candidate_phases() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("tmp");
        fs::create_dir(&root).unwrap();
        let now = SystemTime::now();
        let mut state = WorkState::new(&root, now);
        state.queue.push_back(WorkItem::Scan {
            cursor: ScanCursor::new(&root.join("large")),
            depth: 1,
            eligible_for_delete: true,
            retry: RetryState::default(),
        });
        state.queue.push_back(WorkItem::Delete {
            path: root.join("smaller"),
            depth: 1,
            retry_count: 2,
            retry_after_unix_secs: unix_secs(now) + 60,
            touched_dirs: BTreeMap::new(),
        });
        let path = temp.path().join("state/work.json");
        state.save(&path).unwrap();
        let reloaded = WorkState::load(&path, &root).unwrap().unwrap();
        assert_eq!(reloaded.queue.len(), 3);
        assert!(matches!(reloaded.queue[1], WorkItem::Scan { .. }));
        assert!(matches!(
            reloaded.queue[2],
            WorkItem::Delete { retry_count: 2, .. }
        ));
    }

    #[test]
    fn tick_continues_large_candidate_and_visits_later_sibling() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("tmp");
        let wide = root.join("a-wide");
        let later = root.join("z-later");
        fs::create_dir_all(&wide).unwrap();
        fs::create_dir_all(&later).unwrap();
        let now = SystemTime::now();
        let old = now - STALE_THRESHOLD - Duration::from_secs(3_600);
        for index in 0..12 {
            let path = wide.join(format!("{index}.txt"));
            fs::write(&path, b"x").unwrap();
            filetime::set_file_mtime(&path, filetime::FileTime::from_system_time(old)).unwrap();
        }
        let later_file = later.join("old.txt");
        fs::write(&later_file, b"x").unwrap();
        filetime::set_file_mtime(&later_file, filetime::FileTime::from_system_time(old)).unwrap();
        for path in [&wide, &later, &root] {
            filetime::set_file_mtime(path, filetime::FileTime::from_system_time(old)).unwrap();
        }
        let work_path = temp.path().join("state/work.json");
        for _ in 0..80 {
            let result = sweep_tick_at(&root, &work_path, now, false, 3).unwrap();
            if result.pending == 0 {
                break;
            }
        }
        assert!(!wide.exists(), "large stale tree must eventually go");
        assert!(!later.exists(), "later sibling must not be starved");
        assert!(!work_path.exists(), "finished queue should be retired");
    }

    #[test]
    #[ignore = "large local stress fixture; run explicitly before the #1260 PR"]
    fn more_than_200k_entries_eventually_reclaim_without_starving_later_sibling() {
        if !crate::platform::host::UNIX {
            return;
        }
        let temp = tempdir().unwrap();
        let root = temp.path().join("tmp");
        let wide = root.join("a-wide");
        let live = root.join("m-live");
        let later = root.join("z-later");
        fs::create_dir_all(&wide).unwrap();
        fs::create_dir_all(&live).unwrap();
        fs::create_dir_all(&later).unwrap();
        let now = SystemTime::now();
        let old = now - STALE_THRESHOLD - Duration::from_secs(3_600);
        let mut seeds = Vec::new();
        for index in 0..4 {
            let seed = wide.join(format!("seed-{index}"));
            fs::write(&seed, b"x").unwrap();
            filetime::set_file_mtime(&seed, filetime::FileTime::from_system_time(old)).unwrap();
            seeds.push(seed);
        }
        for index in 0..200_001 {
            fs::hard_link(
                &seeds[index % seeds.len()],
                wide.join(format!("{index:06}")),
            )
            .unwrap();
        }
        let later_file = later.join("old.txt");
        fs::write(&later_file, b"x").unwrap();
        filetime::set_file_mtime(&later_file, filetime::FileTime::from_system_time(old)).unwrap();
        let live_file = live.join("recent.txt");
        fs::write(&live_file, b"x").unwrap();
        for path in [&wide, &live, &later] {
            filetime::set_file_mtime(path, filetime::FileTime::from_system_time(old)).unwrap();
        }
        let work_path = temp.path().join("work.json");
        for tick in 0..30 {
            let started = std::time::Instant::now();
            let report =
                sweep_tick_with_quantum(&root, &work_path, now, false, 200_000, 200_000).unwrap();
            eprintln!(
                "#1260 stress tick {tick}: elapsed={:?}, examined={}, removed_files={}, pending={}",
                started.elapsed(),
                report.examined,
                report.removed_files,
                report.pending,
            );
            if report.pending == 0 {
                break;
            }
        }
        assert!(!wide.exists(), ">200k stale entries must eventually go");
        assert!(live_file.exists(), "recently used file must survive");
        assert!(!later.exists(), "later sibling must not be starved");
    }

    #[test]
    fn deletion_obeys_each_ticks_operation_allowance() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("tmp");
        let candidate = root.join("candidate");
        fs::create_dir_all(&candidate).unwrap();
        let now = SystemTime::now();
        let old = now - STALE_THRESHOLD - Duration::from_secs(3_600);
        for index in 0..12 {
            let path = candidate.join(format!("{index}.txt"));
            fs::write(&path, b"x").unwrap();
            filetime::set_file_mtime(&path, filetime::FileTime::from_system_time(old)).unwrap();
        }
        filetime::set_file_mtime(&candidate, filetime::FileTime::from_system_time(old)).unwrap();
        let work_path = temp.path().join("work.json");
        let mut prior: usize = 12;
        let mut observed_partial_removal = false;
        for _ in 0..30 {
            sweep_tick_at(&root, &work_path, now, false, 3).unwrap();
            let left = if candidate.exists() {
                fs::read_dir(&candidate).unwrap().count()
            } else {
                0
            };
            assert!(
                prior.saturating_sub(left) <= 3,
                "one tick removed beyond its allowance"
            );
            if (1..12).contains(&left) {
                observed_partial_removal = true;
            }
            prior = left;
            if left == 0 {
                break;
            }
        }
        assert!(observed_partial_removal, "deletion should be incremental");
    }

    #[test]
    fn new_activity_before_deletion_keeps_the_remaining_candidate() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("tmp");
        let candidate = root.join("candidate");
        fs::create_dir_all(&candidate).unwrap();
        let now = SystemTime::now();
        let old = now - STALE_THRESHOLD - Duration::from_secs(3_600);
        let old_file = candidate.join("old.txt");
        fs::write(&old_file, b"old").unwrap();
        filetime::set_file_mtime(&old_file, filetime::FileTime::from_system_time(old)).unwrap();
        filetime::set_file_mtime(&candidate, filetime::FileTime::from_system_time(old)).unwrap();
        let work_path = temp.path().join("work.json");
        let mut state = WorkState::new(&root, now);
        state.queue = VecDeque::from([WorkItem::Delete {
            path: candidate.clone(),
            depth: 1,
            retry_count: 0,
            retry_after_unix_secs: 0,
            touched_dirs: BTreeMap::new(),
        }]);
        state.save(&work_path).unwrap();
        fs::write(candidate.join("new-live-file"), b"live").unwrap();
        sweep_tick_at(&root, &work_path, now, false, 10).unwrap();
        assert!(old_file.exists(), "deletion must stop after new activity");
        assert!(candidate.join("new-live-file").exists());
    }

    #[test]
    fn refreshed_existing_file_before_delete_survives_small_batches() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("tmp");
        let candidate = root.join("candidate");
        fs::create_dir_all(&candidate).unwrap();
        let now = SystemTime::now();
        let old = now - STALE_THRESHOLD - Duration::from_secs(3_600);
        for index in 0..8 {
            let file = candidate.join(format!("{index}.txt"));
            fs::write(&file, b"old").unwrap();
            filetime::set_file_mtime(&file, filetime::FileTime::from_system_time(old)).unwrap();
        }
        filetime::set_file_mtime(&candidate, filetime::FileTime::from_system_time(old)).unwrap();
        let work_path = temp.path().join("work.json");
        let mut state = WorkState::new(&root, now);
        state.queue = VecDeque::from([WorkItem::Delete {
            path: candidate.clone(),
            depth: 1,
            retry_count: 0,
            retry_after_unix_secs: 0,
            touched_dirs: BTreeMap::new(),
        }]);
        state.save(&work_path).unwrap();
        let children = fs::read_dir(&candidate)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        assert_eq!(children.len(), 8);
        let refreshed = children.last().unwrap();
        filetime::set_file_mtime(refreshed, filetime::FileTime::from_system_time(now)).unwrap();
        sweep_tick_at(&root, &work_path, now, false, 100).unwrap();
        assert_eq!(fs::read_dir(&candidate).unwrap().count(), 8);
        for _ in 0..10 {
            sweep_tick_at(&root, &work_path, now, false, 3).unwrap();
        }
        assert!(
            refreshed.exists(),
            "fresh file must survive small delete batches"
        );
    }

    #[test]
    fn replaced_directory_symlink_cannot_reach_outside_file() {
        if !crate::platform::host::UNIX {
            return;
        }

        let temp = tempdir().unwrap();
        let root = temp.path().join("tmp");
        let outside = temp.path().join("outside");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&outside).unwrap();
        let outside_file = outside.join("old.txt");
        fs::write(&outside_file, b"keep").unwrap();
        let now = SystemTime::now();
        let old = now - STALE_THRESHOLD - Duration::from_secs(3_600);
        filetime::set_file_mtime(&outside_file, filetime::FileTime::from_system_time(old)).unwrap();
        let queued_dir = root.join("queued");
        crate::platform::gc_test_fs::symlink(&outside, &queued_dir).unwrap();
        let mut state = WorkState::new(&root, now);
        state.queue = VecDeque::from([WorkItem::Explore {
            path: queued_dir,
            depth: 1,
            after: None,
            retry: RetryState::default(),
        }]);
        let work_path = temp.path().join("work.json");
        state.save(&work_path).unwrap();

        sweep_tick_at(&root, &work_path, now, false, 10).unwrap();
        assert!(
            outside_file.exists(),
            "symlinked ancestor escaped sweep root"
        );
    }

    #[test]
    fn altered_work_queue_cannot_delete_outside_sweep_root() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("tmp");
        fs::create_dir(&root).unwrap();
        let outside = temp.path().join("outside.txt");
        fs::write(&outside, b"keep").unwrap();
        let now = SystemTime::now();
        let old = now - STALE_THRESHOLD - Duration::from_secs(3_600);
        filetime::set_file_mtime(&outside, filetime::FileTime::from_system_time(old)).unwrap();
        let work_path = temp.path().join("work.json");
        let mut state = WorkState::new(&root, now);
        state.queue.clear();
        state.queue.push_back(WorkItem::FlatFile {
            path: outside.clone(),
            retry_count: 0,
            retry_after_unix_secs: 0,
        });
        state.save(&work_path).unwrap();

        sweep_tick_at(&root, &work_path, now, false, 10).unwrap();
        assert!(
            outside.exists(),
            "invalid work state must never delete outside root"
        );
        assert!(work_path.with_extension("corrupt.json").exists());
    }

    #[test]
    fn deferred_work_does_not_spin_continuous_worker() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("tmp");
        fs::create_dir(&root).unwrap();
        let now = SystemTime::now();
        let mut state = WorkState::new(&root, now);
        state.queue = VecDeque::from([WorkItem::Explore {
            path: root.clone(),
            depth: 0,
            after: None,
            retry: RetryState {
                count: 1,
                after_unix_secs: unix_secs(now) + 60,
            },
        }]);
        let work_path = temp.path().join("work.json");
        state.save(&work_path).unwrap();
        let report = sweep_tick_at(&root, &work_path, now, false, 200_000).unwrap();
        assert_eq!(report.pending, 1);
        assert_eq!(report.advanced_steps, 0);
        assert_eq!(report.retry_count, 1);
    }

    #[test]
    fn failed_candidate_retries_while_other_candidate_finishes() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("tmp");
        fs::create_dir(&root).unwrap();
        let failing = root.join("failing");
        let succeeding = root.join("succeeding");
        fs::write(&failing, b"not a directory yet").unwrap();
        fs::create_dir(&succeeding).unwrap();
        let now = SystemTime::now();
        let old = now - STALE_THRESHOLD - Duration::from_secs(3_600);
        for path in [&failing, &succeeding] {
            filetime::set_file_mtime(path, filetime::FileTime::from_system_time(old)).unwrap();
        }
        let mut state = WorkState::new(&root, now);
        state.queue.clear();
        for path in [&failing, &succeeding] {
            state.queue.push_back(WorkItem::Delete {
                path: path.to_path_buf(),
                depth: 1,
                retry_count: 0,
                retry_after_unix_secs: 0,
                touched_dirs: BTreeMap::new(),
            });
        }
        let work_path = temp.path().join("state/work.json");
        state.save(&work_path).unwrap();

        let first = sweep_tick_at(&root, &work_path, now, false, 10).unwrap();
        assert_eq!(first.failures, 1);
        assert!(!succeeding.exists(), "other candidate must finish");
        assert!(failing.exists(), "failed candidate remains visible");
        assert_eq!(first.pending, 1);

        fs::remove_file(&failing).unwrap();
        fs::create_dir(&failing).unwrap();
        filetime::set_file_mtime(&failing, filetime::FileTime::from_system_time(old)).unwrap();
        let second =
            sweep_tick_at(&root, &work_path, now + Duration::from_secs(61), false, 10).unwrap();
        assert!(
            !failing.exists(),
            "transient failure must eventually succeed"
        );
        assert_eq!(second.pending, 0);
    }

    #[test]
    fn flat_file_deletion_does_not_shift_explore_cursor_past_siblings() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("tmp");
        fs::create_dir(&root).unwrap();
        let now = SystemTime::now();
        let old = now - STALE_THRESHOLD - Duration::from_secs(3_600);
        for index in 0..6 {
            let path = root.join(format!("{index}.txt"));
            fs::write(&path, b"old").unwrap();
            filetime::set_file_mtime(&path, filetime::FileTime::from_system_time(old)).unwrap();
        }
        let work_path = temp.path().join("work.json");
        for _ in 0..10 {
            if sweep_tick_at(&root, &work_path, now, false, 2)
                .unwrap()
                .pending
                == 0
            {
                break;
            }
        }
        assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
    }

    #[test]
    fn flat_file_is_queued_then_rechecked_before_deletion() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("tmp");
        fs::create_dir(&root).unwrap();
        let file = root.join("old.txt");
        fs::write(&file, b"old").unwrap();
        let now = SystemTime::now();
        let old = now - STALE_THRESHOLD - Duration::from_secs(3_600);
        filetime::set_file_mtime(&file, filetime::FileTime::from_system_time(old)).unwrap();
        let work_path = temp.path().join("work.json");

        let first = sweep_tick_at(&root, &work_path, now, false, 1).unwrap();
        assert!(file.exists(), "exploration must only schedule deletion");
        assert!(first.pending > 0);
        filetime::set_file_mtime(&file, filetime::FileTime::from_system_time(now)).unwrap();
        for _ in 0..5 {
            if sweep_tick_at(&root, &work_path, now, false, 1)
                .unwrap()
                .pending
                == 0
            {
                break;
            }
        }
        assert!(
            file.exists(),
            "a file refreshed after discovery must be kept"
        );
    }

    #[test]
    fn malformed_work_state_is_preserved_and_does_not_block_future_ticks() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("tmp");
        fs::create_dir(&root).unwrap();
        let work_path = temp.path().join("state/work.json");
        fs::create_dir_all(work_path.parent().unwrap()).unwrap();
        fs::write(&work_path, b"not-json").unwrap();

        let report = sweep_tick_at(&root, &work_path, SystemTime::now(), false, 1).unwrap();
        assert!(report.failures > 0);
        assert!(temp.path().join("state/work.corrupt.json").exists());
    }

    #[test]
    fn recent_write_to_already_scanned_file_stops_deletion_before_first_batch() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("tmp");
        let candidate = root.join("candidate");
        fs::create_dir_all(&candidate).unwrap();
        let now = SystemTime::now();
        let old = now - STALE_THRESHOLD - Duration::from_secs(3_600);
        for index in 0..20 {
            let path = candidate.join(format!("{index:02}.txt"));
            fs::write(&path, b"old").unwrap();
            filetime::set_file_mtime(&path, filetime::FileTime::from_system_time(old)).unwrap();
        }
        filetime::set_file_mtime(&candidate, filetime::FileTime::from_system_time(old)).unwrap();
        let work_path = temp.path().join("work.json");
        for _ in 0..30 {
            sweep_tick_at(&root, &work_path, now, false, 1).unwrap();
            let state = WorkState::load(&work_path, &root).unwrap();
            if state.as_ref().is_some_and(|state| {
                state
                    .queue
                    .iter()
                    .any(|job| matches!(job, WorkItem::Recheck { .. }))
            }) {
                break;
            }
        }
        let refreshed = candidate.join("19.txt");
        filetime::set_file_mtime(&refreshed, filetime::FileTime::from_system_time(now)).unwrap();
        for _ in 0..30 {
            if sweep_tick_at(&root, &work_path, now, false, 2)
                .unwrap()
                .pending
                == 0
            {
                break;
            }
        }
        assert_eq!(fs::read_dir(&candidate).unwrap().count(), 20);
    }

    #[test]
    fn persistent_failure_is_reported_without_abandoning_retryable_work() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("tmp");
        fs::create_dir(&root).unwrap();
        let failing = root.join("failing");
        fs::write(&failing, b"not a directory").unwrap();
        let now = SystemTime::now();
        let old = now - STALE_THRESHOLD - Duration::from_secs(3_600);
        filetime::set_file_mtime(&failing, filetime::FileTime::from_system_time(old)).unwrap();
        let mut state = WorkState::new(&root, now);
        state.queue = VecDeque::from([WorkItem::Delete {
            path: failing.clone(),
            depth: 1,
            retry_count: 0,
            retry_after_unix_secs: 0,
            touched_dirs: BTreeMap::new(),
        }]);
        let work_path = temp.path().join("work.json");
        state.save(&work_path).unwrap();

        let first = sweep_tick_at(&root, &work_path, now, false, 4).unwrap();
        assert_eq!(first.last_error_path.as_deref(), Some(failing.as_path()));
        assert_eq!(first.last_error_class.as_deref(), Some("NotADirectory"));
        assert!(!first.persistent_failure);

        let later = now + Duration::from_secs(72 * 60 * 60);
        let second = sweep_tick_at(&root, &work_path, later, false, 4).unwrap();
        assert!(second.persistent_failure);
        assert!(second.retry_count >= 2);
        assert_eq!(second.pending, 1, "alert must not discard work");
        assert!(failing.exists());
    }

    #[test]
    fn unreadable_entry_metadata_is_skipped_then_reclaimed_by_a_later_pass() {
        if !crate::platform::host::UNIX {
            return;
        }

        let temp = tempdir().unwrap();
        let root = temp.path().join("tmp");
        fs::create_dir(&root).unwrap();
        let file = root.join("old.txt");
        fs::write(&file, b"old").unwrap();
        let now = SystemTime::now();
        let old = now - STALE_THRESHOLD - Duration::from_secs(3_600);
        filetime::set_file_mtime(&file, filetime::FileTime::from_system_time(old)).unwrap();
        let work_path = temp.path().join("work.json");
        crate::platform::gc_test_fs::set_mode(&root, 0o400).unwrap();
        if fs::symlink_metadata(&file).is_ok() {
            // A root-runner can bypass this permission bit; it cannot
            // exercise the intended failure reliably.
            crate::platform::gc_test_fs::set_mode(&root, 0o700).unwrap();
            return;
        }
        let first = sweep_tick_at(&root, &work_path, now, false, 10).unwrap();
        crate::platform::gc_test_fs::set_mode(&root, 0o700).unwrap();
        assert!(first.failures > 0);
        // #2486: denied is terminal for this pass, not retried with backoff.
        assert_eq!(first.foreign_owned, vec![file.clone()]);
        assert!(file.exists());
        // The next pass starts fresh and sees the recovered permission.
        for _ in 0..10 {
            sweep_tick_at(&root, &work_path, now + Duration::from_secs(61), false, 10).unwrap();
        }
        assert!(!file.exists(), "permission recovery must unblock deletion");
    }

    #[test]
    fn denied_scan_candidate_is_kept_and_reclaimed_after_recovery() {
        if !crate::platform::host::UNIX {
            return;
        }

        let temp = tempdir().unwrap();
        let root = temp.path().join("tmp");
        let candidate = root.join("candidate");
        fs::create_dir_all(&candidate).unwrap();
        let file = candidate.join("old.txt");
        fs::write(&file, b"old").unwrap();
        let now = SystemTime::now();
        let old = now - STALE_THRESHOLD - Duration::from_secs(3_600);
        for path in [&file, &candidate] {
            filetime::set_file_mtime(path, filetime::FileTime::from_system_time(old)).unwrap();
        }
        let mut state = WorkState::new(&root, now);
        state.queue = VecDeque::from([WorkItem::Scan {
            cursor: ScanCursor::new(&candidate),
            depth: 1,
            eligible_for_delete: true,
            retry: RetryState::default(),
        }]);
        let work_path = temp.path().join("work.json");
        state.save(&work_path).unwrap();
        crate::platform::gc_test_fs::set_mode(&candidate, 0o000).unwrap();
        if fs::read_dir(&candidate).is_ok() {
            crate::platform::gc_test_fs::set_mode(&candidate, 0o700).unwrap();
            return;
        }
        let first = sweep_tick_at(&root, &work_path, now, false, 10).unwrap();
        crate::platform::gc_test_fs::set_mode(&candidate, 0o700).unwrap();
        // #2486: one tick marks it unremovable and retires the pass.
        assert_eq!(first.pending, 0, "{first:?}");
        assert_eq!(first.foreign_owned, vec![candidate.clone()]);
        assert!(
            candidate.exists(),
            "a denied candidate is never deleted blind"
        );
        for _ in 0..10 {
            sweep_tick_at(&root, &work_path, now + Duration::from_secs(61), false, 10).unwrap();
        }
        assert!(!candidate.exists());
    }

    #[test]
    fn failed_exploration_backs_off_and_recovers() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("tmp");
        fs::create_dir(&root).unwrap();
        let candidate = root.join("candidate");
        fs::write(&candidate, b"temporarily a file").unwrap();
        let now = SystemTime::now();
        let mut state = WorkState::new(&root, now);
        state.queue = VecDeque::from([WorkItem::Explore {
            path: candidate.clone(),
            depth: 1,
            after: None,
            retry: RetryState::default(),
        }]);
        let work_path = temp.path().join("work.json");
        state.save(&work_path).unwrap();

        let first = sweep_tick_at(&root, &work_path, now, false, 4).unwrap();
        let immediate = sweep_tick_at(&root, &work_path, now, false, 4).unwrap();
        assert_eq!(first.failures, 1);
        assert_eq!(immediate.failures, 0);
        assert_eq!(immediate.retry_count, 1);
        fs::remove_file(&candidate).unwrap();
        fs::create_dir(&candidate).unwrap();
        let recovered =
            sweep_tick_at(&root, &work_path, now + Duration::from_secs(61), false, 4).unwrap();
        assert_eq!(recovered.pending, 0);
    }

    // #1672: on a real box the pass that started 8 days earlier never
    // finished. 195 `Scan` items pointed at directories that no longer
    // existed (NotFound -> Inconclusive -> retry, forever) and 50 `Delete`
    // items hit root-owned files (EACCES -> retry, forever). A new pass only
    // starts when the queue is empty, so no session that went stale after the
    // pass began was ever evaluated: 1084 of 1184 session dirs were >72h old.

    fn backdate(path: &Path, now: SystemTime) {
        let old = now - STALE_THRESHOLD - Duration::from_secs(3_600);
        filetime::set_file_mtime(path, filetime::FileTime::from_system_time(old)).unwrap();
    }

    #[test]
    fn vanished_scan_candidate_completes_instead_of_stalling_the_pass() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("tmp");
        let gone = root.join("cache").join("staging");
        fs::create_dir_all(&gone).unwrap();
        let now = SystemTime::now();
        let mut state = WorkState::new(&root, now);
        state.queue = VecDeque::from([WorkItem::Scan {
            cursor: ScanCursor::new(&gone),
            depth: 2,
            eligible_for_delete: false,
            retry: RetryState::default(),
        }]);
        let work_path = temp.path().join("state/work.json");
        state.save(&work_path).unwrap();
        fs::remove_dir(&gone).unwrap();

        let report = sweep_tick_at(&root, &work_path, now, false, 16).unwrap();
        assert_eq!(
            report.pending, 0,
            "a candidate that no longer exists has nothing left to do: {report:?}"
        );
        assert!(!work_path.exists(), "the pass must retire");
    }

    #[test]
    fn persistently_failing_candidate_is_spared_and_a_new_pass_reaches_later_stale_sessions() {
        if !crate::platform::host::UNIX {
            return;
        }

        let temp = tempdir().unwrap();
        let root = temp.path().join("tmp");
        let failing = root.join("failing");
        let sealed = failing.join("sealed");
        fs::create_dir_all(&sealed).unwrap();
        let kept = sealed.join("kept.txt");
        fs::write(&kept, b"x").unwrap();
        let now = SystemTime::now();
        for path in [&kept, &sealed, &failing] {
            backdate(path, now);
        }
        let mut state = WorkState::new(&root, now);
        state.queue = VecDeque::from([WorkItem::Scan {
            cursor: ScanCursor::new(&failing),
            depth: 1,
            eligible_for_delete: true,
            retry: RetryState::default(),
        }]);
        let work_path = temp.path().join("state/work.json");
        state.save(&work_path).unwrap();
        // A session that went stale after the pass started: not in the queue.
        let later = root.join("later");
        fs::create_dir_all(&later).unwrap();
        let later_file = later.join("old.txt");
        fs::write(&later_file, b"old").unwrap();
        backdate(&later_file, now);
        backdate(&later, now);
        crate::platform::gc_test_fs::set_mode(&sealed, 0o000).unwrap();
        if fs::read_dir(&sealed).is_ok() {
            crate::platform::gc_test_fs::set_mode(&sealed, 0o700).unwrap();
            return; // root bypasses the permission bit
        }
        let mut reported = false;
        for step in 0..60u64 {
            let at = now + Duration::from_secs(step * 61 * 60);
            let report = sweep_tick_at(&root, &work_path, at, false, 64).unwrap();
            reported |= report.foreign_owned.contains(&sealed);
            if !later.exists() {
                break;
            }
        }
        crate::platform::gc_test_fs::set_mode(&sealed, 0o700).unwrap();
        assert!(
            reported,
            "the denied subtree is reported as unremovable (#2486)"
        );
        assert!(kept.exists(), "giving up on an item spares it");
        assert!(
            !later.exists(),
            "a stuck item must not stop later passes from reaching new stale sessions"
        );
    }

    #[test]
    fn read_only_directory_inside_stale_candidate_is_removed() {
        if !crate::platform::host::UNIX {
            return;
        }

        let temp = tempdir().unwrap();
        let root = temp.path().join("tmp");
        let candidate = root.join("session");
        let sealed = candidate.join("pkg").join("mod@v1");
        fs::create_dir_all(&sealed).unwrap();
        let file = sealed.join("go.mod");
        fs::write(&file, b"module x").unwrap();
        let now = SystemTime::now();
        for path in [&file, &sealed, &candidate.join("pkg"), &candidate] {
            backdate(path, now);
        }
        // Go's module cache and sealed cargo outputs are 0555 trees.
        crate::platform::gc_test_fs::set_mode(&sealed, 0o555).unwrap();
        let work_path = temp.path().join("state/work.json");
        for step in 0..20u64 {
            let at = now + Duration::from_secs(step * 61 * 60);
            if sweep_tick_at(&root, &work_path, at, false, 64)
                .unwrap()
                .pending
                == 0
                && !candidate.exists()
            {
                break;
            }
        }
        if sealed.exists() {
            crate::platform::gc_test_fs::set_mode(&sealed, 0o700).unwrap();
        }
        assert!(
            !candidate.exists(),
            "a read-only tree must not be retried forever"
        );
    }

    /// #2486: a denied subtree (root-owned Docker output) is marked
    /// unremovable on the first hit; the same pass finishes the rest of the
    /// tree instead of retrying it with backoff.
    #[test]
    fn denied_subtree_is_skipped_on_first_hit_and_the_pass_finishes() {
        if !crate::platform::host::UNIX || crate::platform::rm_tool::effective_uid() == Some(0) {
            return; // root ignores mode bits, so nothing can be denied
        }
        let temp = tempdir().unwrap();
        let root = temp.path().join("tmp");
        let removable = root.join("a-removable");
        let mixed = root.join("b-mixed");
        let denied = mixed.join("zc-cache");
        let sibling = mixed.join("scratch");
        for dir in [&removable, &denied, &sibling] {
            fs::create_dir_all(dir).unwrap();
        }
        fs::write(removable.join("log"), b"old").unwrap();
        fs::write(denied.join("blob"), b"old").unwrap();
        fs::write(sibling.join("out"), b"old").unwrap();
        let now = SystemTime::now();
        for path in [
            removable.join("log"),
            denied.join("blob"),
            sibling.join("out"),
            removable.clone(),
            denied.clone(),
            sibling.clone(),
            mixed.clone(),
        ] {
            backdate(&path, now);
        }
        crate::platform::gc_test_fs::set_mode(&denied, 0o000).unwrap();
        let work_path = temp.path().join("state/work.json");
        let mut last = None;
        // Same instant every tick: a backoff retry could never fire, so the
        // pass only completes if the denied subtree is terminal.
        for _ in 0..10 {
            let report =
                sweep_tick_with_quantum(&root, &work_path, now, false, 10_000, 10_000).unwrap();
            let done = report.pending == 0;
            last = Some(report);
            if done {
                break;
            }
        }
        crate::platform::gc_test_fs::set_mode(&denied, 0o700).unwrap();
        let report = last.unwrap();
        assert_eq!(report.pending, 0, "the pass must retire");
        assert!(
            !removable.exists(),
            "unrelated stale tree is still reclaimed"
        );
        assert!(
            !sibling.exists(),
            "a removable sibling of the denied dir goes"
        );
        assert!(denied.exists(), "the denied subtree is kept");
        assert_eq!(report.foreign_owned, vec![denied]);
        assert_eq!(report.abandoned, 0, "skipped, not retried until abandoned");
    }
}

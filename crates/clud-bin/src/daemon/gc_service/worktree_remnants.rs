//! Durable evidence for recovering a directory left by a failed Git removal.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use kernal_api::hash::Sha256Hasher as Sha256;
use serde::{Deserialize, Serialize};

use super::repo_worktree_probe::git;
use crate::path_norm::{canonicalize_lenient, normalize_for_key, same_path};

pub(super) const UNKNOWN_PROVENANCE: &str = "unknown remnant provenance";

#[derive(Serialize, Deserialize)]
pub(super) struct RemovalReceipt {
    path: PathBuf,
    repo: PathBuf,
    admin: PathBuf,
    tip: String,
    created: SystemTime,
    prepared: SystemTime,
    observed_partial: bool,
    caches: Vec<(PathBuf, SystemTime)>,
}

impl RemovalReceipt {
    pub(super) fn prepare(root: &Path, path: &Path, repo: &Path, tip: &str) -> Option<Self> {
        let admin = super::worktree_activity::registration(path, root)?;
        Some(Self {
            path: path.to_owned(),
            repo: repo.to_owned(),
            admin,
            tip: tip.to_owned(),
            created: fs::symlink_metadata(path).ok()?.created().ok()?,
            prepared: SystemTime::now(),
            observed_partial: false,
            caches: super::worktree_activity::ignored_paths(path)?
                .into_iter()
                .filter_map(|entry| {
                    let cache = path.join(&entry);
                    if !super::worktree_activity::tagged_cache(&cache) {
                        return None;
                    }
                    Some((entry, fs::symlink_metadata(cache).ok()?.created().ok()?))
                })
                .collect(),
        })
    }

    pub(super) fn save(&self, root: &Path) -> std::io::Result<()> {
        let receipt = receipt_path(root, &self.path);
        fs::create_dir_all(receipt.parent().expect("receipt directory"))?;
        crate::fs_private::write_private_atomic(&receipt, &serde_json::to_vec(self)?)
    }

    /// A claimed worktree can become dirty after a failed remove. Authorize
    /// recovery with surviving metadata only if deletions were observed
    /// immediately after the original attempt.
    pub(super) fn confirm_partial(&mut self, root: &Path) {
        if surviving_registration_matches(root, &self.path, self) {
            self.observed_partial = true;
            if let Err(error) = self.save(root) {
                eprintln!("[clud] gc: could not record partial worktree removal: {error}");
            }
        }
    }
}

fn receipt_path(root: &Path, path: &Path) -> PathBuf {
    let normalized = normalize_for_key(&canonicalize_lenient(path));
    let key = Sha256::digest(normalized.as_bytes());
    root.with_file_name("worktree-removal-receipts")
        .join(format!("{key:x}.json"))
}

pub(super) fn forget(root: &Path, path: &Path) {
    let _ = fs::remove_file(receipt_path(root, path));
}

/// Verify every surviving file against the recorded commit. Ignored files,
/// symlinks and other unknown contents always require a human decision.
pub(super) fn verify(root: &Path, path: &Path) -> Result<PathBuf, &'static str> {
    let receipt: RemovalReceipt = fs::read(receipt_path(root, path))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .ok_or(UNKNOWN_PROVENANCE)?;
    let meta = fs::symlink_metadata(path).map_err(|_| "remnant unreadable")?;
    if !same_path(&receipt.path, path)
        || !path.parent().is_some_and(|parent| same_path(parent, root))
        || !meta.is_dir()
        || meta.created().ok() != Some(receipt.created)
    {
        return Err("remnant identity or Git registration changed");
    }
    if fs::symlink_metadata(receipt.admin.join("clud-last-session"))
        .and_then(|meta| meta.modified())
        .is_ok_and(|last| last > receipt.prepared)
    {
        return Err("worktree used since removal attempt");
    }
    match receipt.admin.symlink_metadata() {
        Ok(meta) if meta.is_dir() => {
            if !super::worktree_activity::admin_private_refs_published(
                &receipt.repo,
                &receipt.admin,
            ) {
                return Err("remnant has unpublished or unverifiable private refs");
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        _ => return Err("remnant administration unreadable"),
    }
    let dot_git = path.join(".git");
    match dot_git.symlink_metadata() {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Ok(_)
            if receipt.observed_partial && surviving_registration_matches(root, path, &receipt) => {
        }
        _ => return Err("remnant identity or Git registration changed"),
    }
    let entries = git(&receipt.repo, &["worktree", "list", "--porcelain"])
        .ok_or("remnant repository unavailable")?;
    if crate::worktrees::parse_worktree_porcelain(&entries)
        .iter()
        .any(|entry| entry.locked && crate::path_norm::same_path(&entry.path, path))
    {
        return Err("remnant registration is locked");
    }
    verify_contents(path, &receipt)?;
    Ok(receipt.repo)
}

fn verify_contents(path: &Path, receipt: &RemovalReceipt) -> Result<(), &'static str> {
    let dot_git = path.join(".git");
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut files = walkdir::WalkDir::new(path)
        .min_depth(1)
        .max_open(8)
        .into_iter();
    let mut remaining = 128usize;
    while let Some(entry) = files.next() {
        if remaining == 0 || Instant::now() >= deadline {
            return Err("remnant exceeds verification budget");
        }
        remaining -= 1;
        let entry = entry.map_err(|_| "remnant unreadable")?;
        if entry.path() == dot_git {
            continue;
        }
        if entry.file_type().is_dir() {
            if receipt.caches.iter().any(|(cache, created)| {
                entry.path() == path.join(cache)
                    && fs::symlink_metadata(entry.path())
                        .and_then(|meta| meta.created())
                        .ok()
                        == Some(*created)
                    && super::worktree_activity::tagged_cache(entry.path())
            }) {
                files.skip_current_dir();
            }
            continue;
        }
        if !entry.file_type().is_file() {
            return Err("remnant has unknown contents");
        }
        let relative = entry
            .path()
            .strip_prefix(path)
            .map_err(|_| "remnant path changed")?;
        let relative = relative
            .components()
            .map(|part| part.as_os_str().to_str())
            .collect::<Option<Vec<_>>>()
            .ok_or("remnant has an unreadable file name")?
            .join("/");
        let expected = git(
            &receipt.repo,
            &["rev-parse", &format!("{}:{relative}", receipt.tip)],
        );
        let file = entry
            .path()
            .to_str()
            .ok_or("remnant has an unreadable file name")?;
        let actual = git(&receipt.repo, &["hash-object", "--no-filters", "--", file]);
        if !matches!((expected, actual), (Some(a), Some(b)) if a.trim() == b.trim()) {
            return Err("remnant has unknown contents");
        }
    }
    Ok(())
}

fn surviving_registration_matches(root: &Path, path: &Path, receipt: &RemovalReceipt) -> bool {
    if super::worktree_activity::registration(path, root).as_ref() != Some(&receipt.admin)
        || git(path, &["rev-parse", "HEAD"]).is_none_or(|tip| tip.trim() != receipt.tip)
        || super::worktree_activity::safe_contents(path) != Some(true)
        || !super::worktree_activity::private_refs_published(path)
    {
        return false;
    }
    // Only unstaged deletions explain a dirty remnant. Reject staged
    // state or modes differing from the original even if bytes match.
    git(
        path,
        &[
            "--no-optional-locks",
            "status",
            "--porcelain=v1",
            "--ignore-submodules=none",
        ],
    )
    .is_some_and(|status| !status.is_empty() && status.lines().all(|line| line.starts_with(" D ")))
}

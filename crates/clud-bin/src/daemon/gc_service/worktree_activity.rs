//! Activity and ownership evidence for the managed-worktree idle deadline.

use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::repo_worktree_probe::git;

/// Require a direct, real child of the managed root and a reciprocal Git
/// registration. A broken or one-way registration cannot authorize removal.
pub(super) fn registration(path: &Path, root: &Path) -> Option<PathBuf> {
    let root = root.canonicalize().ok()?;
    let canonical = path.canonicalize().ok()?;
    if canonical.parent()? != root || std::fs::symlink_metadata(path).ok()?.is_symlink() {
        return None;
    }
    let dot_git = path.join(".git");
    let pointer = git_pointer(&dot_git)?;
    let admin = path
        .join(pointer.strip_prefix("gitdir: ")?)
        .canonicalize()
        .ok()?;
    let common = admin
        .join(git_pointer(&admin.join("commondir"))?)
        .canonicalize()
        .ok()?;
    if admin.parent()? != common.join("worktrees") {
        return None;
    }
    let backlink = git_pointer(&admin.join("gitdir"))?;
    if admin.join(backlink).canonicalize().ok()? != dot_git.canonicalize().ok()? {
        return None;
    }
    Some(admin)
}

fn git_pointer(path: &Path) -> Option<String> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if !meta.is_file() || meta.len() > 64 * 1024 {
        return None;
    }
    let mut value = std::fs::read_to_string(path).ok()?;
    value.truncate(value.trim_end_matches(['\r', '\n']).len());
    (!value.is_empty()).then_some(value)
}

fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::symlink_metadata(path).ok()?.modified().ok()
}

fn nul_records(output: &str) -> Option<impl Iterator<Item = &str>> {
    let raw = output.strip_suffix('\n').unwrap_or(output);
    if !raw.is_empty() && !raw.ends_with('\0') {
        return None;
    }
    Some(raw.split_terminator('\0'))
}

fn normal_relative(file: &str) -> bool {
    !file.is_empty()
        && !file.contains('\u{fffd}')
        && Path::new(file)
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
}

/// Only an explicit cache-directory tag makes ignored contents disposable.
/// The tag format marks the entire directory as regenerable.
pub(super) fn tagged_cache(path: &Path) -> bool {
    use std::io::Read;
    const TAG: &[u8] = b"Signature: 8a477f597d28d172789f06886806bc55";
    let tag = path.join("CACHEDIR.TAG");
    if !std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir())
        || !std::fs::symlink_metadata(&tag).is_ok_and(|m| m.is_file())
    {
        return false;
    }
    let mut bytes = [0; 43];
    std::fs::File::open(tag)
        .and_then(|mut file| file.read_exact(&mut bytes[..TAG.len()]))
        .is_ok()
        && &bytes[..TAG.len()] == TAG
}

/// Refuse an index Git could use to hide changes from status, and preserve
/// unknown ignored files.
pub(super) fn safe_contents(path: &Path) -> Option<bool> {
    let tracked = git(path, &["ls-files", "-v", "-z"])?;
    for record in nul_records(&tracked)? {
        let tag = *record.as_bytes().first()?;
        if tag.is_ascii_lowercase() || tag == b'S' {
            return Some(false);
        }
    }
    Some(
        ignored_paths(path)?
            .iter()
            .all(|entry| tagged_cache(&path.join(entry))),
    )
}

pub(super) fn ignored_paths(path: &Path) -> Option<Vec<PathBuf>> {
    let ignored = git(
        path,
        &[
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--directory",
            "-z",
        ],
    )?;
    let entries = nul_records(&ignored)?;
    entries
        .map(|entry| normal_relative(entry).then(|| PathBuf::from(entry)))
        .collect()
}

/// Removing a worktree also removes its private refs. Shared branch and
/// stash refs live in the common Git directory and survive this operation.
pub(super) fn private_refs_published(path: &Path) -> bool {
    private_refs_published_with(path, &[])
}

pub(super) fn admin_private_refs_published(repo: &Path, admin: &Path) -> bool {
    let admin = crate::path_norm::strip_verbatim(admin);
    let Some(admin) = admin.to_str() else {
        return false;
    };
    private_refs_published_with(repo, &["--git-dir", admin])
}

fn private_refs_published_with(path: &Path, options: &[&str]) -> bool {
    let mut args = options.to_vec();
    args.extend([
        "rev-list",
        "--count",
        "--glob=refs/worktree/*",
        "--glob=refs/bisect/*",
        "--glob=refs/rewritten/*",
        "--not",
        "--remotes",
    ]);
    git(path, &args).is_some_and(|out| out.trim() == "0")
}

pub(crate) fn record_worktree_session(cwd: &Path) {
    if cfg!(test) {
        return;
    }
    let Some(root) = crate::gc::worktree_root::worktree_root() else {
        return;
    };
    let (Ok(root), Ok(cwd)) = (root.canonicalize(), cwd.canonicalize()) else {
        return;
    };
    let Ok(relative) = cwd.strip_prefix(&root) else {
        return;
    };
    let Some(Component::Normal(name)) = relative.components().next() else {
        return;
    };
    if let Some(admin) = registration(&root.join(name), &root) {
        touch_session(&admin);
    }
}

pub(super) fn touch_session(admin: &Path) {
    let marker = admin.join("clud-last-session");
    // The metadata is in the Git admin dir, where clean/status ignore it.
    let _ = crate::fs_private::write_private_atomic(&marker, b"");
}

/// The root mtime covers entry creation/removal. The index, HEAD and its
/// reflog cover Git activity, while tracked file mtimes cover in-place edits.
/// Session history covers sessions that can finish between daemon passes.
pub(super) fn last_activity(path: &Path, admin: &Path) -> Option<i64> {
    let mut latest = modified(path)?.max(modified(&admin.join("HEAD"))?);
    for name in ["index", "logs/HEAD", "clud-last-session"] {
        if let Some(time) = modified(&admin.join(name)) {
            latest = latest.max(time);
        }
    }
    let tracked = git(path, &["ls-files", "-z"])?;
    for file in nul_records(&tracked)? {
        if !normal_relative(file) {
            return None;
        }
        latest = latest.max(modified(&path.join(file))?);
    }
    if let Ok(state) = crate::daemon::default_state_dir() {
        use crate::session_history::index;
        let cwd = index::canonical_cwd(path);
        if let Some(time) = modified(&index::index_path(&state, &cwd)) {
            latest = latest.max(time);
        }
        for session in index::read(&state, &cwd).sessions {
            if let Some(time) = modified(&session.transcript_path) {
                latest = latest.max(time);
            }
        }
    }
    i64::try_from(latest.duration_since(UNIX_EPOCH).ok()?.as_secs()).ok()
}

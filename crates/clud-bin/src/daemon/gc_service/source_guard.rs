//! Fresh source-use checks before automatic reclamation (#1790).
//!
//! A container's cwd is in its own mount namespace, so the host process
//! table alone cannot show which source tree it is using.

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::extern_repo::probe_cmd;
use super::repo_worktree_probe::collect_process_cwds;
use super::repo_worktree_reclaim::ProcessCwdSnapshot;

pub(super) fn source_in_use(path: &Path) -> Option<&'static str> {
    source_in_use_with_processes(path, &collect_process_cwds())
}

pub(super) fn source_in_use_with_processes(
    path: &Path,
    processes: &ProcessCwdSnapshot,
) -> Option<&'static str> {
    let mounts = container_sources(path);
    source_use_reason(path, processes, mounts.as_deref())
}

fn source_use_reason(
    path: &Path,
    processes: &ProcessCwdSnapshot,
    mounts: Option<&[PathBuf]>,
) -> Option<&'static str> {
    let Ok(root) = path.canonicalize() else {
        return Some("pinned: source path unreadable");
    };
    let root = PathBuf::from(crate::path_norm::normalize_for_key(&root));
    if !processes.available {
        return Some("pinned: process references unavailable");
    }
    if processes
        .cwds
        .iter()
        .any(|cwd| PathBuf::from(crate::path_norm::normalize_for_key(cwd)).starts_with(&root))
    {
        return Some("pinned: live process inside source tree");
    }
    let Some(mounts) = mounts else {
        return Some("pinned: container references unavailable");
    };
    if mounts.iter().any(|source| {
        // Canonicalize existing prefixes too: a deleted/recreated bind must
        // not become evidence that the new inode is safe to reclaim.
        let Ok(source) = crate::rm_tool::canonical_with_missing_tail(source) else {
            return true;
        };
        let source = PathBuf::from(crate::path_norm::normalize_for_key(&source));
        source.starts_with(&root) || root.starts_with(&source)
    }) {
        return Some("pinned: container bind references source tree");
    }
    None
}

fn container_sources(cwd: &Path) -> Option<Vec<PathBuf>> {
    let path = std::env::var_os("PATH")?;
    let executable = std::env::current_exe().ok()?;
    let Some(docker) = crate::shim_registry::first_on_path("docker", &path, &executable, &[])
    else {
        return Some(Vec::new());
    };
    let docker = docker.to_str()?;
    let (code, ids) = probe_cmd(docker, cwd, &["ps", "--quiet"], Duration::from_secs(5))?;
    if code != 0 {
        return None;
    }
    let ids: Vec<&str> = ids.split_whitespace().collect();
    if ids.is_empty() {
        return Some(Vec::new());
    }
    // Bound command size and work. Incomplete inspection must spare.
    if ids.len() > 256 {
        return None;
    }
    let mut args = vec!["inspect", "--format", "{{json .Mounts}}"];
    let expected = ids.len();
    args.extend(ids);
    let (code, output) = probe_cmd(docker, cwd, &args, Duration::from_secs(10))?;
    if code != 0
        || output
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count()
            != expected
    {
        return None;
    }
    parse_mount_sources(&output)
}

fn parse_mount_sources(output: &str) -> Option<Vec<PathBuf>> {
    let mut sources = Vec::new();
    for line in output.lines().filter(|line| !line.trim().is_empty()) {
        let mounts: serde_json::Value = serde_json::from_str(line).ok()?;
        for mount in mounts.as_array()? {
            if mount.get("Type")?.as_str()? == "bind" {
                let source = PathBuf::from(mount.get("Source")?.as_str()?);
                if !source.is_absolute() {
                    return None;
                }
                sources.push(source);
            }
        }
    }
    Some(sources)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn child_process_and_child_or_parent_container_binds_pin_source() {
        let tmp = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let child = root.join("nested-clone");
        let live = ProcessCwdSnapshot {
            available: true,
            cwds: vec![child.clone()],
        };
        assert_eq!(
            source_use_reason(&root, &live, Some(&[])),
            Some("pinned: live process inside source tree")
        );
        let idle = ProcessCwdSnapshot {
            available: true,
            cwds: vec![],
        };
        for source in [child, root.clone(), root.parent().unwrap().to_path_buf()] {
            assert_eq!(
                source_use_reason(&root, &idle, Some(&[source])),
                Some("pinned: container bind references source tree")
            );
        }
        assert_eq!(
            source_use_reason(&root, &idle, Some(&[root.join("../unrelated")])),
            None
        );
        assert_eq!(source_use_reason(&root, &idle, Some(&[])), None);
        assert_eq!(
            source_use_reason(&root, &idle, None),
            Some("pinned: container references unavailable")
        );
        assert_eq!(
            source_use_reason(&root, &ProcessCwdSnapshot::default(), Some(&[])),
            Some("pinned: process references unavailable")
        );
    }

    #[test]
    fn missing_bind_descendant_under_symlink_alias_still_pins_source() {
        let tmp = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let source = tmp.path().join("source");
        let alias = tmp.path().join("alias");
        std::fs::create_dir(&source).unwrap();
        let result = crate::alias_link::install_alias_with(
            &source,
            &alias,
            &[crate::alias_link::Method::Symlink],
        );
        let traversable = alias.canonicalize().ok() == source.canonicalize().ok();
        let alias = if result.is_ok() && traversable {
            alias
        } else {
            // Still exercise missing-tail protection on hosts without
            // directory symlinks; only the alias-specific fixture is absent.
            eprintln!("directory alias unavailable; checking the real source prefix: {result:?}");
            source.clone()
        };
        let idle = ProcessCwdSnapshot {
            available: true,
            cwds: Vec::new(),
        };
        assert_eq!(
            source_use_reason(&source, &idle, Some(&[alias.join("deleted-child")])),
            Some("pinned: container bind references source tree")
        );
    }

    #[test]
    fn mount_reader_keeps_only_bind_sources_and_rejects_incomplete_data() {
        let tmp = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let document = serde_json::json!([
            {"Type": "bind", "Source": root},
            {"Type": "volume", "Source": "cache"}
        ]);
        let mounts = parse_mount_sources(&document.to_string()).unwrap();
        assert_eq!(mounts, vec![root]);
        assert!(parse_mount_sources(r#"[{"Type":"bind"}]"#).is_none());
        assert!(parse_mount_sources("not json").is_none());
        assert!(parse_mount_sources(r#"[{"Type":"bind","Source":"relative"}]"#).is_none());
    }
}

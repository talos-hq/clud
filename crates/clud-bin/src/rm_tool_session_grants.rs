//! Explicit, session-scoped deletion roots for sibling Git checkouts.

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use kernal_api::hash::Sha256Hasher as Sha256;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionGrant {
    pub root: PathBuf,
    pub reason: String,
    pub session_id: String,
    pub created_unix_ms: u128,
    /// The checkout's `device` / `inode` on Unix; nothing elsewhere.
    #[serde(flatten)]
    pub identity: crate::platform::rm_tool::GrantIdentity,
}

fn file_path(state_dir: &Path, session_id: &str) -> PathBuf {
    let digest = Sha256::digest(session_id.as_bytes());
    state_dir
        .join("rm-grants")
        .join(format!("{digest:x}.jsonl"))
}

fn checkout(root: &Path) -> bool {
    root.join(".git").exists()
        && crate::block_bad_cmd::nearest_repo_root_public(root)
            .and_then(|path| crate::path_norm::canonicalize_plain(path).ok())
            .is_some_and(|path| path == root)
}

fn eligible(root: &Path, launch_roots: &[PathBuf]) -> bool {
    checkout(root)
        && crate::platform::rm_tool::owned_by_caller(root)
        && launch_roots.iter().any(|launch| {
            // `root` is canonical; compare a canonical launch root too, or a
            // symlinked temp prefix (macOS `/var` -> `/private/var`) makes
            // every sibling look foreign.
            let launch =
                crate::path_norm::canonicalize_plain(launch).unwrap_or_else(|_| launch.clone());
            let launch = &launch;
            checkout(launch)
                && launch != root
                && launch
                    .parent()
                    .is_some_and(|parent| root.parent() == Some(parent))
        })
}

fn same_identity(grant: &SessionGrant) -> bool {
    crate::platform::rm_tool::same_grant_identity(&grant.root, &grant.identity)
}

/// Record a sibling checkout grant. The reason appears in safe-rm's audit.
pub fn grant(
    raw_root: &Path,
    reason: &str,
    session_id: &str,
    launch_roots: &[PathBuf],
    state_dir: &Path,
) -> Result<PathBuf, String> {
    if reason.trim().is_empty() {
        return Err("--reason must explain the authorized sibling cleanup".into());
    }
    if session_id.is_empty() {
        return Err("no clud session id; launch through clud, or set CLUD_SESSION_ID to a unique value before starting this session".into());
    }
    let root = crate::path_norm::canonicalize_plain(raw_root)
        .map_err(|e| format!("cannot resolve sibling checkout: {e}"))?;
    if !eligible(&root, launch_roots) {
        return Err("grant requires a sibling Git checkout beside this session's checkout".into());
    }
    let identity = crate::platform::rm_tool::grant_identity(&root)?;
    let record = SessionGrant {
        root: root.clone(),
        reason: reason.trim().to_owned(),
        session_id: session_id.to_owned(),
        created_unix_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
        identity,
    };
    let path = file_path(state_dir, session_id);
    let dir = path.parent().ok_or("invalid grant path")?;
    fs::create_dir_all(dir).map_err(|e| format!("cannot create grant directory: {e}"))?;
    let options = {
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        crate::platform::rm_tool::grant_record_mode(&mut options);
        options
    };
    let mut file = options
        .open(&path)
        .map_err(|e| format!("cannot open grant record: {e}"))?;
    serde_json::to_writer(&mut file, &record).map_err(|e| format!("cannot record grant: {e}"))?;
    file.write_all(b"\n")
        .map_err(|e| format!("cannot finish grant record: {e}"))?;
    file.sync_all()
        .map_err(|e| format!("cannot sync grant record: {e}"))?;
    Ok(root)
}

/// Only records tied to this live session and unchanged sibling checkouts apply.
pub fn load(state_dir: &Path, session_id: &str, launch_roots: &[PathBuf]) -> Vec<SessionGrant> {
    let path = file_path(state_dir, session_id);
    let Ok(file) = OpenOptions::new().read(true).open(path) else {
        return Vec::new();
    };
    let mut data = String::new();
    if file.take(1024 * 1024).read_to_string(&mut data).is_err() {
        return Vec::new();
    }
    data.lines()
        .filter_map(|line| serde_json::from_str::<SessionGrant>(line).ok())
        .filter(|grant| {
            grant.session_id == session_id
                && eligible(&grant.root, launch_roots)
                && same_identity(grant)
        })
        .collect()
}

pub fn reason(grant: &SessionGrant) -> String {
    format!(
        "session root grant: {} (session {}, reason: {})",
        grant.root.display(),
        grant.session_id,
        grant.reason
    )
}

#[cfg(test)]
#[path = "rm_tool_session_grants_tests.rs"]
pub(crate) mod tests;

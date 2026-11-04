//! Strong liveness signals for the session-temp size cap (#2397, DD-194).
//!
//! An mtime says only that a unit was quiet, not that nobody is using it: a
//! session parked on a long CI wait or overnight writes nothing for hours and
//! still expects its scratch. So before [`super::session_tmp_cap`] evicts an
//! idle unit it asks this module whether anything points at it:
//!
//! - a running process whose cwd, or `TMPDIR`/`TMP`/`TEMP`/`CLAUDE_CODE_TMPDIR`
//!   value, lies inside the unit or names its ancestor strictly inside the
//!   managed temp root (never the global root itself); or
//! - a unit path component equal to the Claude-encoded form of a running
//!   process's cwd (or any ancestor of it): the whole `claude-<uid>/<project>/`
//!   subtree is live while a process works in that project; or
//! - a unit path component equal to a session id the daemon still tracks (a
//!   non-exited snapshot under `<state>/sessions`, a non-terminated record
//!   under `<state>/api-sessions`).
//!
//! Fail safe: when a source cannot be consulted (the process scan cannot read this process's own cwd or environ, or a
//! unverified process of ours returned neither, or the session records are unreadable) every
//! unit counts as live, so the cap simply stays exceeded.
//! Verified protected desktop services are excluded using their OS service
//! identity and manager lineage, never their name alone.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

/// Env vars that name a process's scratch directory.
const SCRATCH_ENV: &[&str] = &["TMPDIR", "TMP", "TEMP", "CLAUDE_CODE_TMPDIR"];

/// What is known to be in use right now. `None` in a field means that source
/// could not be consulted, which makes every unit live.
#[derive(Debug, Clone, Default)]
pub struct LiveSignals {
    paths: Option<Vec<PathBuf>>,
    /// Claude project-dir names of every process cwd and each of its
    /// ancestors (see [`crate::session_history::import::project_slug`]).
    project_slugs: HashSet<String>,
    session_ids: Option<HashSet<String>>,
}

impl LiveSignals {
    /// Signals from explicit parts (production scan results, or a test).
    pub fn new(paths: Option<Vec<PathBuf>>, session_ids: Option<HashSet<String>>) -> Self {
        let project_slugs = paths
            .iter()
            .flatten()
            .flat_map(|p| p.ancestors())
            .filter(|a| a.parent().is_some())
            .map(|a| crate::session_history::import::project_slug(&a.to_string_lossy()))
            .collect();
        Self {
            paths,
            project_slugs,
            session_ids,
        }
    }

    /// Both sources consulted and nothing found.
    pub fn nothing_live() -> Self {
        Self::new(Some(Vec::new()), Some(HashSet::new()))
    }

    pub fn unavailable_reason(&self) -> Option<&'static str> {
        if self.paths.is_none() {
            Some("process cwd/environment references unavailable")
        } else if self.session_ids.is_none() {
            Some("tracked session records unavailable")
        } else {
            None
        }
    }

    /// Scan the host processes and the daemon's session records.
    pub fn collect() -> Self {
        let ids = crate::daemon::default_state_dir()
            .ok()
            .and_then(|state| tracked_session_ids(&state));
        Self::new(process_paths(), ids)
    }

    /// Is `unit` (a path under `root`) pointed at by a live process or a
    /// tracked session? An unconsulted source answers `true`.
    pub fn is_live(&self, root: &Path, unit: &Path) -> bool {
        let (Some(paths), Some(ids)) = (&self.paths, &self.session_ids) else {
            return true;
        };
        let canonical = fs::canonicalize(unit).unwrap_or_else(|_| unit.to_path_buf());
        let canonical_root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        if paths.iter().any(|p| {
            p.starts_with(unit)
                || p.starts_with(&canonical)
                || (p != root && p.starts_with(root) && unit.starts_with(p))
                || (p != &canonical_root
                    && p.starts_with(&canonical_root)
                    && canonical.starts_with(p))
        }) {
            return true;
        }
        let rel = unit.strip_prefix(root).unwrap_or(unit);
        // `TMPDIR` is the tmp root and Claude's cwd is the project dir, so
        // neither matches a unit; the project-dir name does (Claude names
        // `claude-<uid>/<project>` after the encoded cwd).
        rel.components().any(|c| {
            let name = c.as_os_str().to_string_lossy();
            ids.contains(name.as_ref())
                || self
                    .project_slugs
                    .iter()
                    .any(|live| slug_matches(name.as_ref(), live))
        })
    }
}

/// Prefix length compared once both names are long enough to be suspected
/// truncations (#2400).
const LONG_SLUG: usize = 100;

/// Does the unit component `name` plausibly name the project whose encoded
/// cwd is `live`? Exact equality, plus two deliberately generous long-path
/// rules: newer Claude Code is believed (unverified) to truncate long encoded
/// names and append `-<hash>`. When both names are at least [`LONG_SLUG`]
/// long and share their first [`LONG_SLUG`] characters, or `name` minus a
/// trailing `-<hex>` is a prefix of `live`, we treat them as the same
/// project. A false positive only spares scratch for a sibling of a live deep
/// path; a false negative evicts a live session's files, so this errs toward
/// sparing.
fn slug_matches(name: &str, live: &str) -> bool {
    if name == live {
        return true;
    }
    if name.len() >= LONG_SLUG
        && live.len() >= LONG_SLUG
        && name.as_bytes()[..LONG_SLUG] == live.as_bytes()[..LONG_SLUG]
    {
        return true;
    }
    let base = match name.rsplit_once('-') {
        Some((base, hex)) if !hex.is_empty() && hex.bytes().all(|b| b.is_ascii_hexdigit()) => base,
        _ => name,
    };
    base.len() >= LONG_SLUG && live.starts_with(base)
}

fn env_value<'a>(environ: &'a [std::ffi::OsString], key: &str) -> Option<&'a str> {
    environ.iter().find_map(|entry| {
        entry
            .to_str()?
            .strip_prefix(key)?
            .strip_prefix('=')
            .filter(|v| !v.is_empty())
    })
}

/// What the process scan learned about one process. Plain data so the
/// fail-safe rules are testable without a live process table.
#[derive(Debug, Clone, Default)]
pub struct ProcFacts {
    /// Working directory, when the OS let us read it.
    pub cwd: Option<PathBuf>,
    /// `KEY=value` environment entries, empty when unreadable.
    pub environ: Vec<std::ffi::OsString>,
    /// Zombies have neither cwd nor environ by design; they prove nothing.
    pub zombie: bool,
    /// Owned by the user running this scan.
    pub same_user: bool,
    /// Verified protected desktop infrastructure in its OS service lineage.
    pub desktop_service: bool,
    /// Kernel-confirmed disappearance since the process table was captured.
    pub gone: bool,
}

fn cwd_available(cwd: Option<&Path>) -> bool {
    cwd.is_some_and(|path| !path.as_os_str().is_empty())
}

/// Turn process facts into in-use paths. `None` (unconsulted, everything
/// live) when this process cannot read its own cwd or environ, or when an
/// unverified live process of ours returned neither. A denied read looks
/// exactly like absence, so it cannot authorize eviction.
pub fn paths_from_facts(own: &ProcFacts, procs: &[ProcFacts]) -> Option<Vec<PathBuf>> {
    if !cwd_available(own.cwd.as_deref()) || own.environ.is_empty() {
        return None;
    }
    let mut paths = Vec::new();
    for process in procs {
        if process.same_user
            && !process.zombie
            && !process.gone
            && !process.desktop_service
            && !cwd_available(process.cwd.as_deref())
            && process.environ.is_empty()
        {
            return None;
        }
        if cwd_available(process.cwd.as_deref()) {
            paths.extend(process.cwd.clone());
        }
        for key in SCRATCH_ENV {
            paths.extend(env_value(&process.environ, key).map(PathBuf::from));
        }
    }
    paths.sort();
    paths.dedup();
    Some(paths)
}

/// Every process cwd and scratch-env path on the host (see
/// [`paths_from_facts`] for when this is `None`).
fn process_paths() -> Option<Vec<PathBuf>> {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessStatus, ProcessesToUpdate, System, UpdateKind};
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing()
            .with_cwd(UpdateKind::Always)
            .with_environ(UpdateKind::Always)
            .with_user(UpdateKind::Always),
    );
    let me = system.process(Pid::from_u32(std::process::id()))?;
    let uid = me.user_id().cloned();
    let facts = |p: &sysinfo::Process| ProcFacts {
        cwd: p.cwd().map(Path::to_path_buf),
        environ: p.environ().to_vec(),
        zombie: p.status() == ProcessStatus::Zombie,
        same_user: p.user_id().cloned() == uid,
        desktop_service: !cwd_available(p.cwd())
            && p.environ().is_empty()
            && p.user_id().cloned() == uid
            && p.status() != ProcessStatus::Zombie
            && crate::platform::session_tmp_live::verified_desktop_service(p.pid().as_u32()),
        gone: !cwd_available(p.cwd())
            && p.environ().is_empty()
            && crate::platform::session_tmp_live::task_has_exited(p.pid().as_u32()),
    };
    let mut blocked = Vec::new();
    let procs: Vec<ProcFacts> = system
        .processes()
        .values()
        .map(|process| {
            let fact = facts(process);
            if fact.same_user
                && !fact.zombie
                && !fact.gone
                && !fact.desktop_service
                && !cwd_available(fact.cwd.as_deref())
                && fact.environ.is_empty()
            {
                blocked.push(format!(
                    "{} ({})",
                    process.pid(),
                    process.name().to_string_lossy()
                ));
            }
            fact
        })
        .collect();
    let own = facts(me);
    let paths = paths_from_facts(&own, &procs);
    if paths.is_none() {
        eprintln!("tmp: liveness census unavailable: own cwd={}, own environment={}; {} unreadable process(es): {}",
            cwd_available(own.cwd.as_deref()), !own.environ.is_empty(), blocked.len(),
            blocked.iter().take(8).cloned().collect::<Vec<_>>().join(", "));
    }
    paths
}

/// Ids of sessions the daemon still tracks, from its state directory.
/// `None` when a records directory exists but cannot be read.
pub fn tracked_session_ids(state_dir: &Path) -> Option<HashSet<String>> {
    let mut ids = HashSet::new();
    for (sub, is_live) in [
        (
            "sessions",
            snapshot_is_live as fn(&serde_json::Value) -> bool,
        ),
        ("api-sessions", api_record_is_live),
    ] {
        let entries = match fs::read_dir(state_dir.join(sub)) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return None,
        };
        for path in entries.flatten().map(|e| e.path()) {
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            // An unreadable or unparsable record is doubt: fail safe.
            let doc = fs::read_to_string(&path)
                .ok()
                .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())?;
            if is_live(&doc) {
                collect_ids(&doc, &mut ids);
            }
        }
    }
    Some(ids)
}

fn collect_ids(doc: &serde_json::Value, ids: &mut HashSet<String>) {
    for key in ["id", "provider_session_id", "session_id"] {
        if let Some(id) = doc.get(key).and_then(|v| v.as_str()) {
            ids.insert(id.to_string());
        }
    }
}

/// A worker snapshot is tracked until it records an exit code.
fn snapshot_is_live(doc: &serde_json::Value) -> bool {
    doc.get("exit_code").is_none_or(serde_json::Value::is_null)
}

/// An API session is tracked until it is `terminated`.
fn api_record_is_live(doc: &serde_json::Value) -> bool {
    doc.get("state").and_then(|s| s.as_str()) != Some("terminated")
}

#[cfg(test)]
#[path = "session_tmp_live_tests.rs"]
mod tests;

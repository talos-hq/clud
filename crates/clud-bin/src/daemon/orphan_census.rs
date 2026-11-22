//! Match process observations to daemon and foreground session registrations.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use serde::Serialize;

use super::{
    api_sessions::{ApiSessionRecord, ApiSessionState, ApiSessionStore},
    io_helpers::read_json_file,
    paths::sessions_dir,
    types::SessionSnapshot,
};
use crate::{
    process_identity::ProcessIdentity,
    process_scan::{scan_env, EnvScan},
    session_registry::RegistryError,
};

#[derive(Default, Serialize)]
pub(crate) struct Census {
    pub(crate) orphans_by_session: BTreeMap<String, usize>,
    pub(crate) stale_sessions: Vec<String>,
    pub(crate) unregistered_children: Vec<u32>,
}

impl Census {
    pub(crate) fn orphan_count(&self) -> usize {
        self.orphans_by_session.values().sum()
    }

    pub(crate) fn has_findings(&self) -> bool {
        self.orphan_count() != 0
            || !self.stale_sessions.is_empty()
            || !self.unregistered_children.is_empty()
    }
}

fn snapshots(state_dir: &Path, include_retired: bool) -> impl Iterator<Item = SessionSnapshot> {
    std::fs::read_dir(sessions_dir(state_dir))
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(move |entry| {
            let path = entry.path();
            let extension = path.extension()?.to_str()?;
            (extension == "json" || (include_retired && extension == "tombstone")).then_some(path)
        })
        .filter_map(|path| read_json_file(&path).ok())
}

fn live(scan: &EnvScan, identity: ProcessIdentity) -> bool {
    scan.live
        .get(&identity.pid)
        .is_some_and(|now| identity.matches(now))
}

pub(crate) fn census(state_dir: &Path) -> Census {
    let api = ApiSessionStore::new(state_dir).list().unwrap_or_default();
    census_with_api(state_dir, &api)
}

pub(super) fn census_with_api(state_dir: &Path, api: &[ApiSessionRecord]) -> Census {
    let foreground = crate::session_registry::recorded_identities().unwrap_or_default();
    let records: Vec<_> = snapshots(state_dir, false).collect();
    let scan = scan_env("CLUD");
    summarize(&scan, &records, api, foreground)
}

fn summarize(
    scan: &EnvScan,
    snapshots: &[SessionSnapshot],
    api: &[ApiSessionRecord],
    foreground: Vec<ProcessIdentity>,
) -> Census {
    let mut report = Census::default();
    let mut registered = HashMap::new();
    for record in snapshots {
        registered.insert(record.worker_pid, record.id.clone());
        if record.exit_code.is_none()
            && !live(scan, record.worker_identity())
            && !record.root_identity().is_some_and(|root| live(scan, root))
        {
            report.stale_sessions.push(record.id.clone());
        }
    }
    for identity in foreground {
        let label = format!("CLUD:{}", identity.pid);
        let present = scan.live.get(&identity.pid).is_some_and(|observed| {
            !identity.has_start_time()
                || crate::process_identity::origin_start(identity.pid, observed.start_time)
                    .is_none_or(|start| start == identity.start_time)
        });
        if !present {
            report.stale_sessions.push(label.clone());
        }
        registered.insert(identity.pid, label);
    }
    let api_ids: HashSet<_> = api.iter().map(|record| &record.id).collect();
    for record in api.iter().filter(|record| {
        matches!(
            record.state,
            ApiSessionState::Running | ApiSessionState::Interrupting
        )
    }) {
        let root = record
            .turns
            .iter()
            .find(|turn| Some(&turn.id) == record.current_turn_id.as_ref())
            .and_then(|turn| turn.root_identity);
        if root.is_none_or(|root| !live(scan, root)) {
            report.stale_sessions.push(record.id.clone());
        }
    }
    for child in &scan.tagged {
        if scan.declared_daemons.contains(&child.pid) {
            continue;
        }
        let api_label = scan
            .api_session_ids
            .get(&child.pid)
            .filter(|id| api_ids.contains(id));
        let label = api_label.or_else(|| {
            registered
                .get(&child.parent_pid)
                .filter(|_| child.parent_alive || !scan.live.contains_key(&child.parent_pid))
        });
        if scan.orphaned_children.contains(&child.pid) {
            *report
                .orphans_by_session
                .entry(label.unwrap_or(&child.originator).clone())
                .or_default() += 1;
        }
        if label.is_none() {
            report.unregistered_children.push(child.pid);
        }
    }
    report.stale_sessions.sort_unstable();
    report.unregistered_children.sort_unstable();
    report
}

/// Add peer processes in a stale worker's dedicated process group. Require a
/// tagged member to corroborate ownership if the recorded leader is gone.
pub(super) fn expand_stale_groups(
    state_dir: &Path,
    scan: &mut EnvScan,
    handover: &mut std::collections::BTreeSet<u32>,
    recorded_foreground: impl FnOnce() -> Result<Vec<ProcessIdentity>, RegistryError>,
) {
    if scan.tagged.iter().all(|child| child.parent_alive) {
        return;
    }
    let records: Vec<_> = snapshots(state_dir, true).collect();
    let mut protected = HashSet::new();
    for record in &records {
        if live(scan, record.worker_identity())
            || record.root_identity().is_some_and(|root| live(scan, root))
        {
            protected.insert(record.worker_pid);
            handover.insert(record.worker_pid);
        }
    }
    let group = crate::platform::process_identity::process_group;
    let groups: HashMap<_, _> = records
        .iter()
        .filter_map(|record| {
            let root = record.root_identity()?;
            (!protected.contains(&record.worker_pid) && !scan.live.contains_key(&root.pid))
                .then_some((root.pid, record.worker_pid))
        })
        .collect();
    if groups.is_empty() {
        return;
    }
    let tagged_groups: Vec<_> = scan
        .tagged
        .iter()
        .filter_map(|child| group(child.pid).map(|pgid| (pgid, child)))
        .collect();
    let mut eligible = HashMap::new();
    for &(pgid, child) in &tagged_groups {
        if groups.get(&pgid) == Some(&child.parent_pid)
            && !child.parent_alive
            && !handover.contains(&child.parent_pid)
        {
            eligible.insert(pgid, child.clone());
        }
    }
    for (pgid, child) in tagged_groups {
        if child.parent_alive
            || handover.contains(&child.parent_pid)
            || scan.declared_daemons.contains(&child.pid)
        {
            eligible.remove(&pgid);
        }
    }
    for pid in scan.declared_daemons.iter().chain(&protected) {
        if let Some(group) = group(*pid) {
            eligible.remove(&group);
        }
    }
    if eligible.is_empty() {
        return;
    }
    // Foreground clud executables may have inherited the group before they
    // have spawned a tagged backend. Treat a registration as a veto.
    let Ok(foreground) = recorded_foreground() else {
        return;
    };
    for identity in foreground {
        if let Some(group) = group(identity.pid).filter(|_| scan.live.contains_key(&identity.pid)) {
            eligible.remove(&group);
        }
    }
    add_group_peers(scan, &eligible);
}

fn add_group_peers(
    scan: &mut EnvScan,
    eligible: &HashMap<u32, crate::process_scan::TaggedProcess>,
) {
    let group = crate::platform::process_identity::process_group;
    let tagged: HashSet<_> = scan.tagged.iter().map(|child| child.pid).collect();
    let mut peers: Vec<_> = scan
        .live
        .values()
        .filter_map(|identity| {
            if !identity.has_start_time()
                || tagged.contains(&identity.pid)
                || scan.declared_daemons.contains(&identity.pid)
            {
                return None;
            }
            let leader = eligible.get(&group(identity.pid)?)?;
            let mut peer = leader.clone();
            peer.pid = identity.pid;
            peer.start_time = identity.start_time;
            peer.name.clear();
            peer.command.clear();
            Some(peer)
        })
        .collect();
    let ids: Vec<_> = peers
        .iter()
        .map(|peer| sysinfo::Pid::from_u32(peer.pid))
        .collect();
    if ids.is_empty() {
        return;
    }
    let mut system = sysinfo::System::new();
    system.refresh_processes_specifics(
        sysinfo::ProcessesToUpdate::Some(&ids),
        true,
        sysinfo::ProcessRefreshKind::nothing(),
    );
    peers.retain_mut(|peer| {
        let Some(process) = system.process(sysinfo::Pid::from_u32(peer.pid)) else {
            return false;
        };
        let Some(pgid) = group(peer.pid).filter(|group| eligible.contains_key(group)) else {
            return false;
        };
        if process.start_time() != peer.start_time
            || process.parent().and_then(|parent| group(parent.as_u32())) == Some(pgid)
        {
            // A parent in the same group owns this subtree. The normal
            // tree walk either reaps it or prunes it with that parent.
            return false;
        }
        peer.name = process.name().to_string_lossy().into_owned();
        true
    });
    scan.tagged.extend(peers);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process_scan::TaggedProcess;

    #[test]
    fn detects_registry_drift_in_both_directions() {
        let mut scan = EnvScan::default();
        scan.tagged.push(TaggedProcess {
            pid: 303,
            start_time: 30,
            name: "child".into(),
            command: "child".into(),
            originator: "CLUD:202".into(),
            parent_pid: 202,
            parent_alive: false,
        });
        scan.orphaned_children.insert(303);
        let report = summarize(&scan, &[], &[], vec![ProcessIdentity::new(101, 10)]);
        assert_eq!(report.stale_sessions, ["CLUD:101"]);
        assert_eq!(report.unregistered_children, [303]);
        assert_eq!(
            report.orphans_by_session,
            BTreeMap::from([("CLUD:202".into(), 1)])
        );
        assert!(report.has_findings());
    }

    #[test]
    #[ignore = "opt-in host scan timing"]
    fn measure_orphan_census() {
        let mut elapsed = Vec::new();
        let mut count = 0;
        for _ in 0..7 {
            let start = std::time::Instant::now();
            let scan = scan_env("CLUD");
            let _ = summarize(&scan, &[], &[], Vec::new());
            elapsed.push(start.elapsed());
            count = scan.live.len();
        }
        elapsed.sort_unstable();
        eprintln!(
            "orphan census over {count} live processes: median {:?}, max {:?}",
            elapsed[3], elapsed[6]
        );
    }
}

#[cfg(test)]
#[path = "orphan_census_group_tests.rs"]
mod group_tests;

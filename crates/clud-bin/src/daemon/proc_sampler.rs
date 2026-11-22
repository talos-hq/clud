use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

use super::io_helpers::read_json_file;
use super::paths::sessions_dir;
use super::types::{
    unix_millis_now, ProcRow, ProcTier, ProcTreeSnapshot, ProcTreeSummary, SessionSnapshot,
};

pub(super) const DEFAULT_SAMPLE_INTERVAL_MS: u64 = 2_000;

/// Cadence once the sampler parks (issue #548).
///
/// An idle daemon refreshed **every process on the host** every 2 s forever,
/// whether or not anything consumed the result — cost that scales with the
/// host's process count rather than with clud activity. The #464 harness
/// measured that refresh at ~112 ms p50 on a 517-process Windows box, i.e.
/// ~5.6 % of a core sustained.
///
/// 30 s bounds how stale a parked snapshot gets without pinning the CPU. A
/// consumer arriving mid-sleep does not wait it out — see
/// [`sleep_until_next_tick`].
pub(super) const PARKED_SAMPLE_INTERVAL_MS: u64 = 30_000;

/// How long after the last snapshot request the fast cadence is kept.
///
/// Spans a human paging through `clud top` without re-parking between
/// refreshes, and covers the gap between a session registering and its first
/// snapshot request.
pub(super) const CONSUMER_IDLE_GRACE_MS: u64 = 30_000;

const ORIGINATOR_SCAN_INTERVAL_MS: u64 = 30_000;
pub(super) const DEAD_ROW_RETENTION_MS: u64 = 60_000;
const MAX_TRACKED_PIDS: usize = 5_000;
const EWMA_ALPHA: f32 = 0.3;

/// Which cadence the sampler should run at right now.
///
/// A named type rather than a bare `u64` so the decision and the duration
/// stay distinguishable at the call site — "we are parked" is the fact worth
/// asserting in a test, not the number 30000.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SampleCadence {
    Active,
    Parked,
}

impl SampleCadence {
    pub(super) fn interval_ms(self, active_ms: u64) -> u64 {
        match self {
            // Never park *below* the configured active cadence: an operator
            // who set 60 s deliberately should not be sped up to 30 s by the
            // parking logic.
            Self::Parked => PARKED_SAMPLE_INTERVAL_MS.max(active_ms),
            Self::Active => active_ms,
        }
    }
}

/// Decide the cadence for the next tick (issue #548).
///
/// Parked requires **both** conditions: nobody has asked for a snapshot
/// recently, *and* there are no live sessions. Either alone is insufficient —
/// a running session with no dashboard attached still needs the fast cadence
/// to keep its history intact, and a consumer polling an empty machine still
/// expects fresh numbers.
///
/// The reason for the live-session half is **snapshot continuity**, not
/// reconciliation (#722). This comment previously claimed the daemon
/// reconciles the session's process tree "on its behalf"; it does not. The
/// snapshot this sampler produces has exactly one consumer path —
/// `DaemonRequest::ProcSnapshot` (`server.rs`) — and every caller of that is
/// by definition someone watching, which bumps `last_request_ms`. Session
/// reconciliation (`sessions::reconcile_session_records`) takes its own
/// independent `refreshed_minimal_system()` and never touches
/// `ProcSamplerHandle`; so does orphan reaping, which shares only the
/// `EnvScanCache`.
///
/// What actually breaks at a 30 s tick is the per-pid history: `EWMA_ALPHA`
/// smoothing over a 15x coarser interval, and `DEAD_ROW_RETENTION_MS` of 60 s
/// meaning short-lived children could be born *and* die between two ticks and
/// never appear at all. A consumer attaching later would then read a session
/// history full of holes. That is worth the idle cost; the vanished
/// reconciliation was not.
pub(super) fn decide_cadence(
    ms_since_last_request: u64,
    live_sessions: usize,
    grace_ms: u64,
) -> SampleCadence {
    if live_sessions > 0 || ms_since_last_request < grace_ms {
        SampleCadence::Active
    } else {
        SampleCadence::Parked
    }
}

/// Should this tick pay for the host environment scan? (#548)
///
/// The scan is a `ReadProcessMemory` walk of **every process's PEB** — the
/// most expensive thing the daemon does, and it is `O(host processes)` rather
/// than `O(clud activity)`. It exists to annotate `ProcSnapshot` rows with
/// their originator tag, and `ProcSnapshot` has exactly one consumer path.
///
/// Parking already means *nobody asked for a snapshot in the grace window and
/// there are no live sessions* — so the annotation this produces has no
/// reader. Before this, the parked interval (30 s) and the scan interval
/// (30 s) were equal, so a fully idle daemon still paid a full-host PEB walk
/// on **every** parked tick: parking cut the sampling but not the scan, which
/// was the expensive half.
///
/// The cost of skipping is bounded and already accepted by #548: the first
/// snapshot served after a park may carry a stale originator annotation, and
/// `clud top` renders `snapshot.refresh_age()` so that staleness is visible.
/// A consumer's arrival wakes the sleep early, so the following tick is
/// `Active` and rescans.
pub(super) fn originator_scan_due(cadence: SampleCadence, interval_elapsed: bool) -> bool {
    match cadence {
        SampleCadence::Parked => false,
        SampleCadence::Active => interval_elapsed,
    }
}

#[derive(Clone)]
pub(super) struct ProcSamplerHandle {
    snapshot: Arc<Mutex<ProcTreeSnapshot>>,
    /// Wall-clock ms of the most recent [`Self::snapshot`] call. Read by the
    /// sampler thread to decide whether anyone is still watching.
    last_request_ms: Arc<AtomicU64>,
}

impl ProcSamplerHandle {
    pub(super) fn empty(interval_ms: u64) -> Self {
        Self {
            snapshot: Arc::new(Mutex::new(ProcTreeSnapshot::empty(interval_ms))),
            // Start "just requested" so a daemon that comes up because someone
            // ran `clud top` samples fast immediately rather than parking
            // through the first grace window.
            last_request_ms: Arc::new(AtomicU64::new(unix_millis_now())),
        }
    }

    pub(super) fn snapshot(&self, include_dead_since_ms: u64) -> ProcTreeSnapshot {
        // Recording before the read matters: it is what lets a parked sampler
        // notice the consumer on its next 100 ms wake instead of finishing a
        // 30 s sleep first.
        self.last_request_ms
            .store(unix_millis_now(), Ordering::Relaxed);
        let mut snapshot = self
            .snapshot
            .lock()
            .expect("proc sampler snapshot mutex poisoned")
            .clone();
        snapshot.refresh_age();
        if include_dead_since_ms == 0 {
            // Unreaped zombies are still in the OS process table.
            snapshot.rows.retain(|row| row.exited_at_ms.is_none());
        } else {
            let now = unix_millis_now();
            snapshot.rows.retain(|row| {
                row.exited_at_ms
                    .is_none_or(|exited| now.saturating_sub(exited) <= include_dead_since_ms)
            });
        }
        snapshot.recompute_summary();
        snapshot
    }
}

pub(super) fn spawn_proc_sampler(
    state_dir: PathBuf,
    shutdown_requested: Arc<AtomicBool>,
    env_scans: Arc<crate::process_scan::EnvScanCache>,
) -> ProcSamplerHandle {
    let interval_ms = configured_interval_ms();
    let handle = ProcSamplerHandle::empty(interval_ms);
    let snapshot = Arc::clone(&handle.snapshot);
    let last_request_ms = Arc::clone(&handle.last_request_ms);
    let _ = thread::Builder::new()
        .name("clud-proc-sampler".to_string())
        .spawn(move || {
            let mut sampler = ProcSampler::new(state_dir, interval_ms, env_scans);
            while !shutdown_requested.load(Ordering::SeqCst) {
                // #548: decide the cadence *before* sampling, so a parked tick
                // can skip the host environment scan. This reads the previous
                // tick's session count, which is one tick stale — acceptable
                // because a consumer's arrival already wakes the sleep early,
                // so the tick that serves them is `Active` either way.
                //
                // The post-sample decision below is deliberately left alone:
                // it uses this tick's fresh session count to size the sleep,
                // and moving it here would delay waking for a session that
                // appeared during the sample.
                let cadence_for_scan = decide_cadence(
                    unix_millis_now().saturating_sub(last_request_ms.load(Ordering::Relaxed)),
                    sampler.last_live_sessions,
                    CONSUMER_IDLE_GRACE_MS,
                );
                let next = sampler.sample(cadence_for_scan);
                *snapshot
                    .lock()
                    .expect("proc sampler snapshot mutex poisoned") = next;

                let requested_at = last_request_ms.load(Ordering::Relaxed);
                let cadence = decide_cadence(
                    unix_millis_now().saturating_sub(requested_at),
                    sampler.last_live_sessions,
                    CONSUMER_IDLE_GRACE_MS,
                );
                sleep_until_next_tick(
                    cadence.interval_ms(interval_ms),
                    &shutdown_requested,
                    &last_request_ms,
                    requested_at,
                );
            }
        });
    handle
}

fn configured_interval_ms() -> u64 {
    let Ok(document) = crate::clud_settings::load_or_init_global_settings() else {
        return DEFAULT_SAMPLE_INTERVAL_MS;
    };
    document
        .pointer("/daemon/proc_sampler/interval_ms")
        .and_then(serde_json::Value::as_u64)
        .filter(|value| (250..=60_000).contains(value))
        .unwrap_or(DEFAULT_SAMPLE_INTERVAL_MS)
}

/// Sleep until the next tick, waking early for shutdown **or** for a new
/// snapshot request.
///
/// The early wake is what makes parking safe to use (issue #548). Without it,
/// a `clud top` that arrives one second into a 30 s parked sleep would show
/// stale data for the remaining 29 — the loop already polls every 100 ms for
/// shutdown, so noticing the consumer on the same poll costs one atomic load.
fn sleep_until_next_tick(
    interval_ms: u64,
    shutdown_requested: &AtomicBool,
    last_request_ms: &AtomicU64,
    requested_at_entry: u64,
) {
    let deadline = Instant::now() + Duration::from_millis(interval_ms);
    while Instant::now() < deadline {
        if shutdown_requested.load(Ordering::SeqCst) {
            return;
        }
        if last_request_ms.load(Ordering::Relaxed) != requested_at_entry {
            // A consumer showed up: resample now rather than serving them a
            // snapshot that could be most of a parked interval old.
            return;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        thread::sleep(remaining.min(Duration::from_millis(100)));
    }
}

struct ProcSampler {
    state_dir: PathBuf,
    system: System,
    ewma_by_pid: HashMap<u32, f32>,
    last_live_rows: HashMap<u32, ProcRow>,
    dead_rows: HashMap<u32, ProcRow>,
    originator_cache: HashMap<u32, OriginatorTag>,
    last_originator_scan: Option<Instant>,
    /// Shared with the periodic orphan sweep so the two never duplicate the
    /// full-host environment pass (#548).
    env_scans: Arc<crate::process_scan::EnvScanCache>,
    interval_ms: u64,
    /// Live sessions seen on the last tick, straight from `SessionIndex`
    /// (issue #548). Counting `rows` instead would be wrong: rows cover the
    /// whole tracked process tree, including the daemon's own row, so it is
    /// never zero and the sampler would never park.
    last_live_sessions: usize,
}

impl ProcSampler {
    fn new(
        state_dir: PathBuf,
        interval_ms: u64,
        env_scans: Arc<crate::process_scan::EnvScanCache>,
    ) -> Self {
        Self {
            state_dir,
            system: System::new(),
            ewma_by_pid: HashMap::new(),
            last_live_rows: HashMap::new(),
            dead_rows: HashMap::new(),
            originator_cache: HashMap::new(),
            last_originator_scan: None,
            env_scans,
            interval_ms,
            last_live_sessions: 0,
        }
    }

    /// Take one sample.
    ///
    /// `cadence` is the decision made for *this* tick, not just the next
    /// sleep: a parked tick skips the host environment scan entirely (#548).
    fn sample(&mut self, cadence: SampleCadence) -> ProcTreeSnapshot {
        self.system.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing()
                .with_cpu()
                .with_memory()
                // #673 Appendix A: `command_from_process` reads `cmd()`, which
                // this refresh never populated. The originator cache masked it,
                // so only a `clud.exe` root with no originator tag showed the
                // bug — its `command` silently degraded to `name()`.
                // `OnlyIfNotSet` fetches each process's command line once and
                // then leaves it alone, so the 2 s tick does not re-read it.
                .with_cmd(UpdateKind::OnlyIfNotSet)
                .without_tasks(),
        );
        self.refresh_originator_cache_if_due(cadence);
        self.originator_cache.retain(|pid, tag| {
            tag.start_time != 0
                && self
                    .system
                    .process(Pid::from_u32(*pid))
                    .is_some_and(|process| process.start_time() == tag.start_time)
        });

        let now_ms = unix_millis_now();
        let parent_by_pid = parent_map(&self.system);
        let children_by_parent = children_map(&self.system);
        let sessions = SessionIndex::from_state(&self.state_dir, &self.system);
        self.last_live_sessions = sessions.sessions.len();
        let included = included_pids(
            &self.system,
            &children_by_parent,
            &self.originator_cache,
            &sessions,
        );

        let mut rows = Vec::new();
        let mut live_rows = HashMap::new();
        for pid_u32 in included.into_iter().take(MAX_TRACKED_PIDS) {
            let pid = Pid::from_u32(pid_u32);
            let Some(process) = self.system.process(pid) else {
                continue;
            };
            let Some(assignment) =
                resolve_assignment(pid_u32, &parent_by_pid, &self.originator_cache, &sessions)
                    .or_else(|| local_launch_assignment(pid_u32, &parent_by_pid, &self.system))
            else {
                continue;
            };
            let cpu_pct = process.cpu_usage().max(0.0);
            let cpu_ewma_pct = ewma(self.ewma_by_pid.get(&pid_u32).copied(), cpu_pct);
            self.ewma_by_pid.insert(pid_u32, cpu_ewma_pct);
            let rss_bytes = process.memory();
            let command = self
                .originator_cache
                .get(&pid_u32)
                .map(|tag| tag.command.clone())
                .filter(|command| !command.is_empty())
                .unwrap_or_else(|| command_from_process(process));
            let row = ProcRow {
                pid: pid_u32,
                ppid: process.parent().map(Pid::as_u32),
                originator: assignment.originator,
                originator_pid: assignment.originator_pid,
                session_id: assignment.session_id,
                session_name: assignment.session_name,
                cpu_pct,
                cpu_ewma_pct,
                rss_bytes,
                age_secs: process.run_time(),
                start_time: process.start_time(),
                command,
                depth: depth_from_originator(pid_u32, assignment.originator_pid, &parent_by_pid),
                tier: tier_for(cpu_pct, cpu_ewma_pct, rss_bytes),
                live: !matches!(
                    process.status(),
                    sysinfo::ProcessStatus::Zombie | sysinfo::ProcessStatus::Dead
                ),
                exited_at_ms: None,
            };
            live_rows.insert(pid_u32, row.clone());
            rows.push(row);
        }

        self.record_dead_rows(now_ms, &live_rows);
        rows.extend(self.dead_rows.values().cloned());
        rows.sort_by(|left, right| {
            left.originator
                .cmp(&right.originator)
                .then(left.depth.cmp(&right.depth))
                .then(left.pid.cmp(&right.pid))
        });
        self.last_live_rows = live_rows;

        let mut snapshot = ProcTreeSnapshot {
            schema_version: 1,
            sampled_at_ms: now_ms,
            sample_age_ms: 0,
            sampler_pid: std::process::id(),
            interval_ms: self.interval_ms,
            rows,
            summary: ProcTreeSummary::default(),
        };
        snapshot.recompute_summary();
        snapshot
    }

    fn refresh_originator_cache_if_due(&mut self, cadence: SampleCadence) {
        let due = self
            .last_originator_scan
            .map(|last| last.elapsed() >= Duration::from_millis(ORIGINATOR_SCAN_INTERVAL_MS))
            .unwrap_or(true);
        if !originator_scan_due(cadence, due) {
            return;
        }
        self.last_originator_scan = Some(Instant::now());
        // #548: shared with the periodic orphan sweep. Asking for a scan no
        // older than our own interval means we refresh only when the sweep has
        // not already done it for us, and vice versa — one host pass per
        // whichever cadence is tighter, instead of one each.
        self.originator_cache = self
            .env_scans
            .get("CLUD", Duration::from_millis(ORIGINATOR_SCAN_INTERVAL_MS))
            .tagged
            .iter()
            .cloned()
            .map(|process| {
                (
                    process.pid,
                    OriginatorTag {
                        originator: process.originator,
                        originator_pid: process.parent_pid,
                        command: process.command,
                        start_time: process.start_time,
                    },
                )
            })
            .collect();
    }

    fn record_dead_rows(&mut self, now_ms: u64, live_rows: &HashMap<u32, ProcRow>) {
        for (pid, row) in &self.last_live_rows {
            if live_rows.contains_key(pid) {
                continue;
            }
            let mut dead = row.clone();
            dead.live = false;
            dead.exited_at_ms = Some(now_ms);
            dead.cpu_pct = 0.0;
            dead.cpu_ewma_pct = ewma(self.ewma_by_pid.get(pid).copied(), 0.0);
            dead.tier = ProcTier::Frozen;
            self.dead_rows.insert(*pid, dead);
        }

        for pid in live_rows.keys() {
            self.dead_rows.remove(pid);
        }
        self.dead_rows.retain(|_, row| {
            row.exited_at_ms
                .map(|exited| now_ms.saturating_sub(exited) <= DEAD_ROW_RETENTION_MS)
                .unwrap_or(false)
        });
        self.ewma_by_pid
            .retain(|pid, _| live_rows.contains_key(pid) || self.dead_rows.contains_key(pid));
    }
}

#[derive(Debug, Clone)]
struct OriginatorTag {
    originator: String,
    originator_pid: u32,
    command: String,
    start_time: u64,
}

#[derive(Debug, Clone)]
struct Assignment {
    originator: String,
    originator_pid: Option<u32>,
    session_id: Option<String>,
    session_name: Option<String>,
}

#[derive(Debug, Clone)]
struct SessionRoot {
    id: String,
    name: Option<String>,
    worker_pid: u32,
    root_pid: Option<u32>,
}

#[derive(Debug, Default)]
struct SessionIndex {
    sessions: Vec<SessionRoot>,
    root_to_session: HashMap<u32, usize>,
}

impl SessionIndex {
    fn from_state(state_dir: &Path, system: &System) -> Self {
        let Ok(entries) = std::fs::read_dir(sessions_dir(state_dir)) else {
            return Self::default();
        };
        let mut sessions = Vec::new();
        let mut root_to_session = HashMap::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let Ok(session) = read_json_file::<SessionSnapshot>(&path) else {
                continue;
            };
            if session.exit_code.is_some() {
                continue;
            }
            if system.process(Pid::from_u32(session.worker_pid)).is_none() {
                continue;
            }
            if let Some(root_pid) = session.root_pid {
                if system.process(Pid::from_u32(root_pid)).is_none() {
                    continue;
                }
            }
            let index = sessions.len();
            root_to_session.insert(session.worker_pid, index);
            if let Some(root_pid) = session.root_pid {
                root_to_session.insert(root_pid, index);
            }
            sessions.push(SessionRoot {
                id: session.id,
                name: session.name,
                worker_pid: session.worker_pid,
                root_pid: session.root_pid,
            });
        }
        Self {
            sessions,
            root_to_session,
        }
    }

    fn session_for_chain(
        &self,
        pid: u32,
        parent_by_pid: &HashMap<u32, u32>,
    ) -> Option<&SessionRoot> {
        let mut current = Some(pid);
        let mut seen = HashSet::new();
        while let Some(cur) = current {
            if !seen.insert(cur) {
                return None;
            }
            if let Some(index) = self.root_to_session.get(&cur) {
                return self.sessions.get(*index);
            }
            current = parent_by_pid.get(&cur).copied();
        }
        None
    }
}

fn included_pids(
    system: &System,
    children_by_parent: &HashMap<u32, Vec<u32>>,
    originator_cache: &HashMap<u32, OriginatorTag>,
    sessions: &SessionIndex,
) -> BTreeSet<u32> {
    let mut included = BTreeSet::new();
    // #571: roots must include every live clud process, not merely backend
    // processes that happened to inherit RUNNING_PROCESS_ORIGINATOR.
    for (pid, process) in system.processes() {
        if is_clud_process(process) {
            include_subtree(pid.as_u32(), children_by_parent, &mut included);
        }
    }
    for pid in originator_cache.keys().copied() {
        if system.process(Pid::from_u32(pid)).is_some() {
            include_subtree(pid, children_by_parent, &mut included);
        }
    }
    for session in &sessions.sessions {
        include_subtree(session.worker_pid, children_by_parent, &mut included);
        if let Some(root_pid) = session.root_pid {
            include_subtree(root_pid, children_by_parent, &mut included);
        }
    }
    included
}

fn is_clud_process(process: &sysinfo::Process) -> bool {
    matches!(
        process
            .name()
            .to_string_lossy()
            .to_ascii_lowercase()
            .as_str(),
        "clud" | "clud.exe"
    )
}

fn local_launch_assignment(
    pid: u32,
    parents: &HashMap<u32, u32>,
    system: &System,
) -> Option<Assignment> {
    let mut pid = pid;
    let mut visited = HashSet::new();
    while visited.insert(pid) {
        let process = system.process(Pid::from_u32(pid))?;
        if is_clud_process(process) {
            return Some(Assignment {
                originator: format!("CLUD:{pid}"),
                originator_pid: Some(pid),
                session_id: None,
                session_name: None,
            });
        }
        let parent_pid = *parents.get(&pid)?;
        let parent_start = system
            .process(Pid::from_u32(parent_pid))
            .map(sysinfo::Process::start_time);
        if !crate::process_scan::parent_is_plausible(parent_start, process.start_time()) {
            return None;
        }
        pid = parent_pid;
    }
    None
}

fn include_subtree(
    root: u32,
    children_by_parent: &HashMap<u32, Vec<u32>>,
    out: &mut BTreeSet<u32>,
) {
    let mut stack = vec![root];
    while let Some(pid) = stack.pop() {
        if !out.insert(pid) {
            continue;
        }
        if let Some(children) = children_by_parent.get(&pid) {
            stack.extend(children.iter().copied());
        }
    }
}

fn resolve_assignment(
    pid: u32,
    parent_by_pid: &HashMap<u32, u32>,
    originator_cache: &HashMap<u32, OriginatorTag>,
    sessions: &SessionIndex,
) -> Option<Assignment> {
    if let Some(tag) = tag_for_chain(pid, parent_by_pid, originator_cache) {
        let session = sessions.session_for_chain(pid, parent_by_pid).or_else(|| {
            sessions
                .sessions
                .iter()
                .find(|session| session.worker_pid == tag.originator_pid)
        });
        return Some(Assignment {
            originator: tag.originator.clone(),
            originator_pid: Some(tag.originator_pid),
            session_id: session.map(|session| session.id.clone()),
            session_name: session.and_then(|session| session.name.clone()),
        });
    }
    let session = sessions.session_for_chain(pid, parent_by_pid)?;
    Some(Assignment {
        originator: format!("CLUD:{}", session.worker_pid),
        originator_pid: Some(session.worker_pid),
        session_id: Some(session.id.clone()),
        session_name: session.name.clone(),
    })
}

fn tag_for_chain<'a>(
    pid: u32,
    parent_by_pid: &HashMap<u32, u32>,
    originator_cache: &'a HashMap<u32, OriginatorTag>,
) -> Option<&'a OriginatorTag> {
    let mut current = Some(pid);
    let mut seen = HashSet::new();
    while let Some(cur) = current {
        if !seen.insert(cur) {
            return None;
        }
        if let Some(tag) = originator_cache.get(&cur) {
            return Some(tag);
        }
        current = parent_by_pid.get(&cur).copied();
    }
    None
}

fn parent_map(system: &System) -> HashMap<u32, u32> {
    system
        .processes()
        .iter()
        .filter_map(|(pid, process)| {
            process
                .parent()
                .map(|parent| (pid.as_u32(), parent.as_u32()))
        })
        .collect()
}

fn children_map(system: &System) -> HashMap<u32, Vec<u32>> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for (pid, process) in system.processes() {
        if let Some(parent) = process.parent() {
            children
                .entry(parent.as_u32())
                .or_default()
                .push(pid.as_u32());
        }
    }
    children
}

fn depth_from_originator(
    pid: u32,
    originator_pid: Option<u32>,
    parent_by_pid: &HashMap<u32, u32>,
) -> u32 {
    let Some(originator_pid) = originator_pid else {
        return 0;
    };
    let mut depth = 0_u32;
    let mut current = Some(pid);
    let mut seen = HashSet::new();
    while let Some(cur) = current {
        if !seen.insert(cur) {
            return depth;
        }
        if cur == originator_pid {
            return depth;
        }
        current = parent_by_pid.get(&cur).copied();
        if current.is_some() {
            depth = depth.saturating_add(1);
        }
        if depth > 128 {
            return depth;
        }
    }
    0
}

fn command_from_process(process: &sysinfo::Process) -> String {
    let parts: Vec<String> = process
        .cmd()
        .iter()
        .map(|part| part.to_string_lossy().into_owned())
        .filter(|part| !part.is_empty())
        .collect();
    if parts.is_empty() {
        process.name().to_string_lossy().into_owned()
    } else {
        parts.join(" ")
    }
}

fn ewma(previous: Option<f32>, sample: f32) -> f32 {
    match previous {
        Some(prev) => (EWMA_ALPHA * sample) + ((1.0 - EWMA_ALPHA) * prev),
        None => sample,
    }
}

fn tier_for(cpu_pct: f32, cpu_ewma_pct: f32, rss_bytes: u64) -> ProcTier {
    const WARM_RSS_BYTES: u64 = 100 * 1024 * 1024;
    if cpu_pct >= 5.0 || cpu_ewma_pct >= 5.0 {
        ProcTier::Hot
    } else if cpu_pct >= 0.5 || cpu_ewma_pct >= 0.5 || rss_bytes >= WARM_RSS_BYTES {
        ProcTier::Warm
    } else {
        ProcTier::Cold
    }
}

#[cfg(test)]
#[cfg(test)]
#[path = "proc_sampler_tests.rs"]
mod tests;

//! One host environment pass, owned by clud (#673 Phase 7).
//!
//! Every reaper in clud needs two answers about the same set of processes:
//!
//! 1. *which processes carry `RUNNING_PROCESS_ORIGINATOR=CLUD:<pid>`* — the
//!    additive kill-target set, and
//! 2. *which processes carry `RUNNING_PROCESS_IS_DAEMON`* — the subtractive
//!    spare-list.
//!
//! Both live in the process environment block, which on Windows means a
//! `ReadProcessMemory` walk of every PEB on the host. `running_process`
//! answers them through two separate entry points
//! ([`running_process::originator::find_processes_by_originator`] and
//! `find_declared_daemon_pids`), each of which builds its own
//! [`sysinfo::System`] and does its own full-host pass — so a single orphan
//! sweep paid for the most expensive enumeration on the machine **twice**,
//! back to back, for two questions about the same processes.
//!
//! ## Why this is reimplemented here rather than fixed upstream
//!
//! `running_process` 4.6.4 depends on `sysinfo 0.30.13`; clud depends directly
//! on `sysinfo 0.37.2`. Those are distinct nominal types, so no snapshot can
//! cross the boundary: clud cannot hand the crate a `System` it already
//! refreshed, and the crate's `System::new()`-per-call shape structurally
//! forbids incremental caching. Reimplementing the two queries against 0.37
//! inside clud is self-contained, needs no upstream release, and is what makes
//! [`DaemonMarkerCache`] possible.
//!
//! The 0.30 `is_parent_alive` start-time comparison is replicated exactly (see
//! [`parent_is_plausible`]); the behaviour of the two queries is otherwise
//! unchanged, with one addition: [`TaggedProcess`] carries `start_time`, so a
//! caller can pin `(pid, creation_time)` at selection time and re-verify it
//! before killing (#673 Phase 6).
//!
//! ## The cache
//!
//! [`DaemonMarkerCache`] answers "did this PID declare itself a daemon?" for a
//! *bounded candidate set* rather than for the host, and reads the environment
//! only for identities it has not seen before. It is safe to keep stale
//! because the daemon marker is **subtractive** — a stale entry can only spare
//! too much, never kill something it should not have.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

use crate::process_identity::{origin_start, ProcessIdentity, UNKNOWN_START_TIME};

/// A process carrying an originator tag that matched the requested tool.
///
/// Mirrors `running_process::originator::OriginatorProcessInfo` field for
/// field, plus [`TaggedProcess::start_time`].
#[derive(Debug, Clone)]
pub struct TaggedProcess {
    pub pid: u32,
    /// Seconds since the UNIX epoch, or [`UNKNOWN_START_TIME`]. Pairs with
    /// `pid` to form the process identity a later kill must re-verify.
    pub start_time: u64,
    pub name: String,
    pub command: String,
    /// The full `RUNNING_PROCESS_ORIGINATOR` value, e.g. `"CLUD:12345"`.
    pub originator: String,
    /// The PID parsed out of the originator value.
    pub parent_pid: u32,
    /// Whether `parent_pid` is still held by a process that could plausibly be
    /// the original originator (alive, and not started *after* this child).
    pub parent_alive: bool,
}

impl TaggedProcess {
    /// The `(pid, creation_time)` key this process was observed under.
    pub fn identity(&self) -> ProcessIdentity {
        ProcessIdentity::new(self.pid, self.start_time)
    }
}

/// Both env-derived populations, from one host pass.
#[derive(Debug, Clone, Default)]
pub struct EnvScan {
    pub(crate) live: std::collections::HashMap<u32, ProcessIdentity>,
    pub(crate) orphaned_children: HashSet<u32>,
    pub(crate) api_session_ids: std::collections::HashMap<u32, String>,
    /// Processes whose originator tag matched the requested tool prefix.
    pub tagged: Vec<TaggedProcess>,
    /// PIDs of every process that declared itself a daemon.
    pub declared_daemons: HashSet<u32>,
}

/// The environment refresh both queries need, requested once.
fn env_refresh() -> ProcessRefreshKind {
    ProcessRefreshKind::nothing()
        .without_tasks()
        .with_environ(UpdateKind::Always)
        .with_cmd(UpdateKind::Always)
}

/// Read `key=` out of an environment block, returning the value.
fn env_value<'a>(environ: &'a [std::ffi::OsString], key: &str) -> Option<&'a str> {
    environ.iter().find_map(|entry| {
        let entry = entry.to_str()?;
        let rest = entry.strip_prefix(key)?;
        rest.strip_prefix('=')
    })
}

/// Parse an originator value like `"CLUD:12345"` into `(tool, parent_pid)`.
///
/// Kept byte-compatible with `running_process::originator::parse_originator_value`
/// — the tool half may itself contain colons, so the *last* colon separates.
pub fn parse_originator_value(value: &str) -> Option<(&str, u32)> {
    let colon = value.rfind(':')?;
    if colon == 0 || colon == value.len() - 1 {
        return None;
    }
    let pid = value[colon + 1..].parse::<u32>().ok()?;
    Some((&value[..colon], pid))
}

/// Whether a `RUNNING_PROCESS_IS_DAEMON` value counts as a declaration.
///
/// The marker is written as `1`, but the usual truthy spellings are accepted so
/// a consumer setting it by hand is not silently ignored. A stray `…=0` must
/// never exempt a process from reaping.
pub fn is_truthy_marker(value: &str) -> bool {
    !matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "" | "0" | "false" | "no" | "off"
    )
}

/// Could the process now holding `parent_start_time` be `child`'s originator?
///
/// A parent that started *after* its claimed child is a recycled PID, not the
/// originator. `None` means the PID is gone entirely.
pub fn parent_is_plausible(parent_start_time: Option<u64>, child_start_time: u64) -> bool {
    match parent_start_time {
        Some(parent) => parent <= child_start_time,
        None => false,
    }
}

/// One host environment pass answering both env questions.
///
/// Replaces a `find_processes_by_originator` + `find_declared_daemon_pids`
/// pair, which cost two full-host PEB walks for the same processes.
pub fn scan_env(tool: &str) -> EnvScan {
    let mut system = System::new();
    system.refresh_processes_specifics(ProcessesToUpdate::All, true, env_refresh());
    classify_scan(&system, tool)
}

/// A shared, age-bounded cache for [`scan_env`] (#548).
///
/// The daemon has two independent consumers of the host environment pass — the
/// proc sampler (annotating `ProcSnapshot` rows) and the periodic orphan sweep
/// (finding reap candidates) — running on different threads and different
/// cadences. Before this they each did their own full-host `ReadProcessMemory`
/// walk of every process's PEB, which is the single most expensive thing the
/// daemon does and is `O(host processes)` rather than `O(clud activity)`.
///
/// This is deliberately **not** "the sampler owns it and the sweep borrows".
/// Ownership would break in the case that matters most: since #720 a parked
/// sampler skips its scan entirely, so on a fully idle daemon the sweep is the
/// *only* scanner. Either party refreshes when it finds the cache older than
/// its own tolerance, and both read the same value — so the steady state is
/// one pass per whichever cadence is tighter, never two.
///
/// # Staleness is safe on the reap path
///
/// The sweep's candidates come from this cache, so they can be up to one
/// tolerance window old. That cannot cause a wrong kill:
/// `process_tree::kill_tree_filtered_automatic` takes its **own fresh**
/// topology snapshot at kill time and re-derives every target's
/// `(pid, start_time)` from it, requiring exact equality and rejecting
/// `UNKNOWN_START_TIME` — for the root *and*, since #688, every descendant.
/// A stale entry can therefore only name a candidate that fails that gate, so
/// the failure mode is a *missed* orphan, which the next sweep catches.
pub struct EnvScanCache {
    inner: Mutex<Option<CachedScan>>,
    /// Full host passes actually performed. The number #548 exists to drive
    /// down, and the only way a test can tell sharing from duplication.
    passes: AtomicU64,
}

struct CachedScan {
    at: Instant,
    scan: Arc<EnvScan>,
}

impl EnvScanCache {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(None),
            passes: AtomicU64::new(0),
        }
    }

    /// A scan no older than `max_age`, refreshing only if the cached one is
    /// older than that (or absent).
    ///
    /// `max_age` is the *caller's* tolerance, not a property of the cache: the
    /// sampler wants ~its scan interval, the sweep wants ~its sweep interval.
    /// Whoever is tighter ends up doing the refresh, and the other reuses it.
    pub fn get(&self, tool: &str, max_age: Duration) -> Arc<EnvScan> {
        // Held across the scan on purpose: two threads arriving on a cold
        // cache should produce one pass, not two. The pass is the expensive
        // thing this type exists to avoid duplicating, so a brief wait is the
        // cheaper side of the trade.
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(cached) = guard.as_ref() {
            if cached.at.elapsed() <= max_age {
                return Arc::clone(&cached.scan);
            }
        }
        self.passes.fetch_add(1, Ordering::Relaxed);
        let scan = Arc::new(scan_env(tool));
        *guard = Some(CachedScan {
            at: Instant::now(),
            scan: Arc::clone(&scan),
        });
        scan
    }

    /// How many full host passes this cache has performed.
    pub fn passes(&self) -> u64 {
        self.passes.load(Ordering::Relaxed)
    }

    /// Seed the cache without scanning. Test-only.
    #[cfg(test)]
    pub fn seed_for_test(&self, scan: EnvScan, at: Instant) {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(CachedScan {
            at,
            scan: Arc::new(scan),
        });
    }
}

impl Default for EnvScanCache {
    fn default() -> Self {
        Self::new()
    }
}

/// The pure half of [`scan_env`], over an already-refreshed snapshot.
pub(crate) const ORIGIN_START_ENV: &str = "CLUD_ORIGIN_START_TIME";

fn origin_alive(
    parent: Option<ProcessIdentity>,
    recorded: Option<u64>,
    observed: Option<u64>,
    child_start: u64,
) -> bool {
    let Some(parent) = parent else {
        return false;
    };
    match recorded.zip(observed) {
        Some((recorded, observed)) => recorded == observed,
        None => parent_is_plausible(Some(parent.start_time), child_start),
    }
}

fn classify_scan(system: &System, tool: &str) -> EnvScan {
    let prefix = format!("{tool}:");
    let daemon_key = running_process::DAEMON_MARKER_ENV_VAR;
    let mut origins = std::collections::HashMap::new();
    let originator_key = running_process::ORIGINATOR_ENV_VAR;

    let mut scan = EnvScan::default();
    for (pid, process) in system.processes() {
        // Tasks share their leader's environment and cannot be reaped separately.
        if process.thread_kind().is_some() {
            continue;
        }

        let Some(identity) = ProcessIdentity::observe_in(system, pid.as_u32()) else {
            continue;
        };
        scan.live.insert(pid.as_u32(), identity);
        let environ = process.environ();

        if env_value(environ, daemon_key).is_some_and(is_truthy_marker) {
            scan.declared_daemons.insert(pid.as_u32());
        }

        let Some(originator) = env_value(environ, originator_key) else {
            continue;
        };
        if !originator.starts_with(&prefix) {
            continue;
        }
        let Some((_tool, parent_pid)) = parse_originator_value(originator) else {
            continue;
        };

        if let Some(session) = env_value(environ, crate::daemon::API_SESSION_ENV) {
            scan.api_session_ids
                .insert(pid.as_u32(), session.to_owned());
        }
        let start_time = process.start_time();
        let origin = ProcessIdentity::observe_in(system, parent_pid);
        let recorded = env_value(environ, ORIGIN_START_ENV)
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|start| *start != UNKNOWN_START_TIME);
        let observed = if recorded.is_some() {
            *origins.entry(parent_pid).or_insert_with(|| {
                origin.and_then(|identity| origin_start(parent_pid, identity.start_time))
            })
        } else {
            None
        };
        let parent_alive = origin_alive(origin, recorded, observed, start_time);
        if !parent_alive || crate::platform::process_identity::orphaned_parent(system, process) {
            scan.orphaned_children.insert(pid.as_u32());
        }
        let cmd = process.cmd();
        let command = if cmd.is_empty() {
            process.name().to_string_lossy().into_owned()
        } else {
            cmd.iter()
                .map(|part| part.to_string_lossy())
                .collect::<Vec<_>>()
                .join(" ")
        };

        scan.tagged.push(TaggedProcess {
            pid: pid.as_u32(),
            start_time,
            name: process.name().to_string_lossy().into_owned(),
            command,
            originator: originator.to_string(),
            parent_pid,
            parent_alive,
        });
    }
    scan
}

/// Incremental "did this PID declare itself a daemon?" lookup for a bounded
/// candidate set (#673 Phase 1b / 7c).
///
/// The Windows job-object reaper asks this question about the handful of PIDs
/// inside *its own job*, at up to 5 Hz. Answering it with a full-host
/// environment scan cost 442 `ReadProcessMemory` PEB reads per ask. This cache
/// reads the environment only for identities it has never resolved, so a steady
/// session pays for single digits of new PIDs per tick and zero when nothing
/// spawned.
///
/// # Staleness is safe here, and only here
///
/// The daemon marker is **subtractive**: it can only remove a process from the
/// reap set. A stale `true` spares something that has since exited — harmless,
/// because the kill path re-verifies identity anyway. Never build the analogous
/// cache for *kill-target enrollment*, which is additive: a stale entry there
/// would name a process that must not be killed.
///
/// Entries are keyed by `(pid, creation_time)`, so a recycled PID is a cache
/// miss rather than a wrong answer. [`DaemonMarkerCache::retain_identities`]
/// is the eviction rule, and callers must run it every pass.
#[derive(Debug, Default)]
pub struct DaemonMarkerCache {
    declared: HashMap<(u32, u64), bool>,
    env_reads: u64,
}

impl DaemonMarkerCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of cached identities. The bound this cache promises is that it
    /// never exceeds the live candidate set the caller last retained.
    pub fn len(&self) -> usize {
        self.declared.len()
    }

    pub fn is_empty(&self) -> bool {
        self.declared.is_empty()
    }

    /// Environment blocks this cache has actually read, cumulatively.
    ///
    /// The measurement #673 exists to drive toward zero in steady state: the
    /// full-host scan it replaced read 442 of them on *every* process exit.
    pub fn env_reads(&self) -> u64 {
        self.env_reads
    }

    /// Resolve every candidate, reading the environment only for identities
    /// that are not already cached, then return the declared-daemon PIDs among
    /// them.
    ///
    /// `candidates` are `(pid, creation_time)` pairs. An entry whose creation
    /// time is [`UNKNOWN_START_TIME`] is never cached — an unkeyable identity
    /// would otherwise pin a wrong answer onto a recycled PID forever — and is
    /// instead resolved fresh each pass.
    pub fn declared_daemons_among(&mut self, candidates: &[(u32, u64)]) -> HashSet<u32> {
        self.retain_identities(candidates);

        let mut declared: HashSet<u32> = candidates
            .iter()
            .filter(|identity| self.declared.get(*identity).copied().unwrap_or(false))
            .map(|(pid, _)| *pid)
            .collect();

        let unresolved: Vec<(u32, u64)> = candidates
            .iter()
            .copied()
            .filter(|identity| !self.declared.contains_key(identity))
            .collect();
        if unresolved.is_empty() {
            return declared;
        }

        let pids: Vec<Pid> = unresolved
            .iter()
            .map(|(pid, _)| Pid::from_u32(*pid))
            .collect();
        let mut system = System::new();
        system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&pids),
            true,
            ProcessRefreshKind::nothing().with_environ(UpdateKind::Always),
        );

        let key = running_process::DAEMON_MARKER_ENV_VAR;
        for (pid, start_time) in unresolved {
            // A PID that is already gone is not a daemon, and caching a miss
            // for it would pin an answer onto whatever gets the number next.
            let Some(process) = system.process(Pid::from_u32(pid)) else {
                continue;
            };
            self.env_reads += 1;
            let is_daemon = env_value(process.environ(), key).is_some_and(is_truthy_marker);
            if is_daemon {
                declared.insert(pid);
            }
            // Only keyed identities are cached. An unkeyable one is answered
            // for this pass and re-read next pass; there is no key that could
            // survive a PID recycle.
            if start_time != UNKNOWN_START_TIME {
                self.declared.insert((pid, start_time), is_daemon);
            }
        }
        declared
    }

    /// Drop every cached identity that is not in `live`.
    ///
    /// This is the cache's eviction rule. The caller already enumerates its
    /// candidate set every pass, so pruning to it is free and keeps the map
    /// bounded by the live candidate count rather than by session age.
    pub fn retain_identities(&mut self, live: &[(u32, u64)]) {
        if live.is_empty() {
            self.declared.clear();
            return;
        }
        let live: HashSet<(u32, u64)> = live.iter().copied().collect();
        self.declared.retain(|identity, _| live.contains(identity));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn originator_value_splits_on_the_last_colon() {
        assert_eq!(parse_originator_value("CLUD:12345"), Some(("CLUD", 12345)));
        assert_eq!(
            parse_originator_value("MY:TOOL:99999"),
            Some(("MY:TOOL", 99999))
        );
        assert_eq!(parse_originator_value("TOOL:0"), Some(("TOOL", 0)));
        assert_eq!(
            parse_originator_value("TOOL:4294967295"),
            Some(("TOOL", u32::MAX))
        );
    }

    #[test]
    fn malformed_originator_values_are_rejected() {
        for bad in [
            "",
            ":",
            "CLUD",
            "CLUD:",
            ":12345",
            "CLUD:abc",
            "TOOL:-1",
            "TOOL:4294967296",
        ] {
            assert!(
                parse_originator_value(bad).is_none(),
                "{bad:?} must not parse"
            );
        }
    }

    /// A stray `RUNNING_PROCESS_IS_DAEMON=0` must not exempt a process from
    /// reaping, so only truthy spellings count as a declaration.
    #[test]
    fn only_truthy_marker_values_declare_a_daemon() {
        for declared in ["1", "true", "yes", "on", "TRUE", " 1 "] {
            assert!(is_truthy_marker(declared), "{declared:?} should declare");
        }
        for not_declared in ["", "0", "false", "no", "off", "  ", "FALSE"] {
            assert!(!is_truthy_marker(not_declared), "{not_declared:?} must not");
        }
    }

    #[test]
    fn a_recycled_windows_parent_in_the_same_second_is_not_the_recorded_parent() {
        let observed = Some(ProcessIdentity::new(42, 100));
        assert!(origin_alive(
            observed,
            Some(100_000_001),
            Some(100_000_001),
            100
        ));
        assert!(!origin_alive(
            observed,
            Some(100_000_001),
            Some(100_000_002),
            100
        ));
    }

    /// Replicates the 0.30 `is_parent_alive` rule exactly: a "parent" that
    /// started after its claimed child is a recycled PID, not the originator.
    #[test]
    fn a_parent_that_started_after_its_child_is_not_the_originator() {
        assert!(parent_is_plausible(Some(100), 200));
        assert!(parent_is_plausible(Some(200), 200));
        assert!(!parent_is_plausible(Some(201), 200));
        assert!(!parent_is_plausible(None, 200));
    }

    #[test]
    fn env_value_reads_the_named_key_only() {
        let environ: Vec<std::ffi::OsString> = ["A=1", "RUNNING_PROCESS_ORIGINATOR=CLUD:7", "B=2"]
            .iter()
            .map(std::ffi::OsString::from)
            .collect();
        assert_eq!(
            env_value(&environ, "RUNNING_PROCESS_ORIGINATOR"),
            Some("CLUD:7")
        );
        assert_eq!(env_value(&environ, "RUNNING_PROCESS_IS_DAEMON"), None);
        // A key that is a prefix of a present key must not match it.
        assert_eq!(env_value(&environ, "RUNNING_PROCESS"), None);
    }

    #[test]
    fn env_value_accepts_an_empty_value() {
        let environ = vec![std::ffi::OsString::from("RUNNING_PROCESS_IS_DAEMON=")];
        assert_eq!(env_value(&environ, "RUNNING_PROCESS_IS_DAEMON"), Some(""));
        assert!(!is_truthy_marker(""));
    }

    #[test]
    fn cache_evicts_identities_that_are_no_longer_live() {
        let mut cache = DaemonMarkerCache::new();
        cache.declared.insert((10, 100), true);
        cache.declared.insert((11, 101), false);

        cache.retain_identities(&[(10, 100)]);
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.declared.get(&(10, 100)), Some(&true));

        cache.retain_identities(&[]);
        assert!(cache.is_empty());
    }

    /// The cache is keyed by identity, so a recycled PID misses rather than
    /// inheriting the previous occupant's answer.
    #[test]
    fn a_recycled_pid_is_a_cache_miss_not_a_wrong_answer() {
        let mut cache = DaemonMarkerCache::new();
        cache.declared.insert((10, 100), true);
        cache.retain_identities(&[(10, 200)]);
        assert!(
            cache.is_empty(),
            "the identity at pid 10 changed, so its cached answer must be dropped"
        );
    }

    /// The whole point of the cache: a second ask about identities it already
    /// resolved must not read any environment block. Exercised through the real
    /// query so the fast path is the one under test.
    #[test]
    fn a_repeated_ask_about_known_identities_is_served_from_cache() {
        let pid = std::process::id();
        let start_time = crate::process_identity::start_time_of(pid);
        let mut cache = DaemonMarkerCache::new();

        // This process was not spawned as a daemon.
        assert!(!cache
            .declared_daemons_among(&[(pid, start_time)])
            .contains(&pid));
        assert_eq!(cache.len(), 1, "self identity is now cached");
        assert_eq!(cache.env_reads(), 1);

        assert!(!cache
            .declared_daemons_among(&[(pid, start_time)])
            .contains(&pid));
        assert_eq!(cache.len(), 1);
        assert_eq!(
            cache.env_reads(),
            1,
            "a repeated ask must not read any environment block"
        );
    }

    /// A candidate set that never contained a daemon yields an empty answer,
    /// and the cache stays bounded by that set.
    #[test]
    fn cache_is_bounded_by_the_candidate_set() {
        let pid = std::process::id();
        let start_time = crate::process_identity::start_time_of(pid);
        let mut cache = DaemonMarkerCache::new();

        for _ in 0..50 {
            let _ = cache.declared_daemons_among(&[(pid, start_time)]);
        }
        assert_eq!(cache.len(), 1);

        // A pass with no candidates clears it outright.
        assert!(cache.declared_daemons_among(&[]).is_empty());
        assert!(cache.is_empty());
    }

    /// Dead PIDs are never cached: caching a miss for a PID that no longer
    /// exists would pin an answer onto whatever gets the number next.
    #[test]
    fn a_dead_pid_is_not_cached() {
        let mut cache = DaemonMarkerCache::new();
        let answer = cache.declared_daemons_among(&[(u32::MAX, 12_345)]);
        assert!(answer.is_empty());
        assert!(cache.is_empty());
    }

    /// The scan must not blow up on a host with nothing tagged, and it must
    /// never report an undeclared self as a daemon.
    #[test]
    fn scan_is_callable_and_excludes_an_undeclared_self() {
        let scan = scan_env("__NONEXISTENT_TOOL_TEST__");
        assert!(scan.tagged.is_empty());
        assert!(!scan.declared_daemons.contains(&std::process::id()));
    }

    /// One pass answers both questions: the tagged set and the daemon set come
    /// out of the same snapshot, which is the whole point of #673 Phase 7b.
    #[test]
    fn one_pass_answers_both_questions() {
        let scan = scan_env("CLUD");
        // Whatever the host happens to be running, the two populations are
        // reported together and every tagged row carries an identity.
        for tagged in &scan.tagged {
            assert_eq!(tagged.identity().pid, tagged.pid);
            assert_eq!(tagged.identity().start_time, tagged.start_time);
        }
    }

    // The Linux-only per-thread scan test lives in `platform/unix/process_scan.rs`.

    // #548: the shared cache. These use `seed_for_test` so they assert the
    // sharing policy without paying for a real host PEB walk per case.

    fn tagged(pid: u32) -> TaggedProcess {
        TaggedProcess {
            pid,
            start_time: 1_700_000_000,
            name: "x.exe".into(),
            command: "x".into(),
            originator: "CLUD:1".into(),
            parent_pid: 1,
            parent_alive: false,
        }
    }

    fn seeded(cache: &EnvScanCache, age: Duration) {
        let mut scan = EnvScan::default();
        scan.tagged.push(tagged(4321));
        cache.seed_for_test(scan, Instant::now() - age);
    }

    /// The property the issue is about: two consumers on different cadences
    /// share one pass instead of taking one each.
    #[test]
    fn a_second_consumer_within_tolerance_reuses_the_pass() {
        let cache = EnvScanCache::new();
        seeded(&cache, Duration::from_secs(5));

        // Sampler-shaped tolerance (30 s) and sweep-shaped tolerance (60 s):
        // both accept a 5 s-old scan, so neither triggers a host pass.
        let a = cache.get("CLUD", Duration::from_secs(30));
        let b = cache.get("CLUD", Duration::from_secs(60));

        assert_eq!(cache.passes(), 0, "a fresh-enough cache must not rescan");
        assert_eq!(a.tagged.len(), 1);
        assert_eq!(b.tagged[0].pid, 4321);
        assert!(Arc::ptr_eq(&a, &b), "both consumers see the same value");
    }

    /// The tolerance is the *caller's*, not the cache's: a consumer with a
    /// tighter window than the cached age must get a fresh pass even though a
    /// laxer consumer would have been satisfied.
    #[test]
    fn a_tighter_tolerance_than_the_cached_age_forces_a_pass() {
        let cache = EnvScanCache::new();
        seeded(&cache, Duration::from_secs(45));

        // Sweep-shaped (60 s) is satisfied by a 45 s-old scan...
        let _ = cache.get("CLUD", Duration::from_secs(60));
        assert_eq!(cache.passes(), 0);

        // ...sampler-shaped (30 s) is not.
        let _ = cache.get("CLUD", Duration::from_secs(30));
        assert_eq!(cache.passes(), 1);
    }

    #[test]
    fn a_cold_cache_scans_once_and_then_serves_from_memory() {
        let cache = EnvScanCache::new();
        let _ = cache.get("__NONEXISTENT_TOOL_TEST__", Duration::from_secs(60));
        assert_eq!(cache.passes(), 1);
        let _ = cache.get("__NONEXISTENT_TOOL_TEST__", Duration::from_secs(60));
        assert_eq!(cache.passes(), 1, "the second call must be served cached");
    }

    /// A zero tolerance means "I need it now" and must always rescan — this is
    /// the shape an explicit `clud slay` would use if it ever shared the cache
    /// (today it bypasses it entirely).
    #[test]
    fn a_zero_tolerance_always_rescans() {
        let cache = EnvScanCache::new();
        seeded(&cache, Duration::ZERO);
        let _ = cache.get("__NONEXISTENT_TOOL_TEST__", Duration::ZERO);
        assert_eq!(cache.passes(), 1);
    }
}

use std::fs;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::types::ENV_BACKLOG_BYTES;

/// Merge the session initiator's environment over the daemon's own (#933).
///
/// Layering, lowest first:
///
/// 1. **the daemon's environment** — kept as the floor because it carries keys
///    only the daemon knows (state dir, wire markers, anything a future
///    daemon-side policy adds). Dropping it would trade one silent divergence
///    for another.
/// 2. **the client's environment** — highest precedence, because it is by
///    definition "what the session initiator sees". This is what makes a tool
///    installed after the daemon started visible to the session.
///
/// `PATH` is a straight client-wins replacement rather than a union of the
/// two. #933 leaves that open and notes replacement is the more predictable
/// reading, and it is also the one that actually fixes the reported bug: a
/// union would keep the daemon's stale entries on the front of the path,
/// where a shadowing old binary still wins the lookup.
///
/// An empty `client_env` means the request came from a client older than this
/// field, so the daemon's environment is used unchanged — the previous
/// behaviour, rather than an empty environment.
fn session_base(client_env: &[(String, String)]) -> Vec<(String, String)> {
    session_base_from(std::env::vars().collect(), client_env)
}

/// Merge an admission-time login baseline with the session initiator's
/// environment. An empty baseline means a spec from before #933's remaining
/// login-baseline slice, so preserve the legacy daemon-env fallback.
fn session_base_from(
    login_env: Vec<(String, String)>,
    client_env: &[(String, String)],
) -> Vec<(String, String)> {
    let mut base: Vec<(String, String)> = if login_env.is_empty() {
        std::env::vars().collect()
    } else {
        login_env
    };
    // A daemon may have been started by an unsafe launch. Its ambient marker
    // must never become the default for a later, unrelated client session.
    base.retain(|(key, _)| key != crate::runner::UNSAFE_MODE_ENV);
    merge_env(base, client_env)
}

/// The merge itself, with the daemon side passed in.
///
/// A parameter rather than a read of `std::env::vars()` so the rule is a pure
/// function: the tests below assert it without mutating this process's
/// environment, which every test in the binary shares. An earlier draft did
/// mutate it, and a concurrent test changing the environment between two
/// reads made the comparison fail for reasons that had nothing to do with the
/// merge.
fn merge_env(
    mut daemon: Vec<(String, String)>,
    client_env: &[(String, String)],
) -> Vec<(String, String)> {
    if client_env.is_empty() {
        return daemon;
    }
    for (key, value) in client_env {
        match daemon.iter_mut().find(|(existing, _)| existing == key) {
            Some(slot) => slot.1 = value.clone(),
            None => daemon.push((key.clone(), value.clone())),
        }
    }
    daemon
}

/// The child environment for a daemon-launched session: the daemon's own
/// environment with the session initiator's layered over it, then every policy
/// layer [`crate::runner::apply_child_env_policy`] owns.
///
/// An empty `client_env` is the documented fallback — the daemon's own
/// environment unchanged — and is what a call site with no session initiator in
/// hand (the API turn controller, diagnostics) passes. The zero-argument
/// `child_env()` this module also used to expose is gone: it had exactly one
/// non-test caller, and that caller wanted the client env (#1209).
pub(crate) fn child_env_from(client_env: &[(String, String)]) -> Vec<(String, String)> {
    child_env_with_base(session_base(client_env))
}

/// The worker path supplies the daemon's persisted login baseline captured at
/// admission. Keeping it in the spec means a periodic refresh changes future
/// sessions only; it can never mutate a live worker's environment.
pub(super) fn child_env_from_login_base(
    login_env: Vec<(String, String)>,
    client_env: &[(String, String)],
) -> Vec<(String, String)> {
    child_env_with_base(session_base_from(login_env, client_env))
}

/// The daemon half of the merged builder (#1209): compute the base, then
/// hand it to the single policy owner.
///
/// A named seam rather than an inlined call so the guard test below can
/// pin "the daemon path really is the same function" against a synthetic
/// base, without mutating this process's environment. This module used to
/// re-implement the IN_CLUD/originator tag, session temp (#509),
/// completion guard (#753), nounset (#1066) and `activate_rm` layers
/// itself, and the Windows UTF-8 stdio pair had already drifted into the
/// runner only, so Windows daemon sessions lost UTF-8 stdio.
fn child_env_with_base(base: Vec<(String, String)>) -> Vec<(String, String)> {
    crate::runner::apply_child_env_policy(base)
}

/// Replace `path` with the JSON encoding of `value` without ever leaving the
/// name unbound.
///
/// The temp file is renamed **over** the target in one step. An earlier
/// version deleted the target first and then renamed, which opened a window
/// where the file simply did not exist. Any concurrent reader landing in that
/// window got `NotFound`, which the API session store translated into a
/// `404 session not found` for a session that was very much alive. The
/// per-session turn controller rewrites the record for every provider event
/// while HTTP handlers read it unlocked, so the window was hit on the loaded
/// Windows unit lane (#1160). `rename` replacing an existing file is atomic
/// with respect to the namespace on POSIX and NTFS alike: a reader sees the
/// old bytes or the new bytes, never a missing file.
///
/// If the direct replace fails anyway (Windows can refuse when another
/// process holds the target without `FILE_SHARE_DELETE`), fall back to the
/// old delete-then-rename so the write still lands; that path is the
/// exception, not the default.
pub(super) fn write_json_file<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("missing parent"))?;
    fs::create_dir_all(parent)?;
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err.to_string()))?;
    // The temp name is unique per write. A fixed `<name>.tmp` was shared by
    // every writer of the file: the session worker and the daemon's Ctrl+C
    // thread both rewrite `sessions/<id>.json`. One writer's `O_TRUNC` open
    // of the shared temp could land after the other had filled it but before
    // it renamed it, so the rename published the just-truncated inode. When
    // the truncating writer was a worker the Ctrl+C fast path then killed,
    // the session file stayed empty for good.
    let temp_path = unique_temp_path(path);
    let result = (|| {
        let mut file = fs::File::create(&temp_path)?;
        io::Write::write_all(&mut file, &bytes)?;
        file.sync_all()?;
        drop(file);
        match fs::rename(&temp_path, path) {
            Ok(()) => Ok(()),
            Err(_) => {
                let _ = fs::remove_file(path);
                fs::rename(&temp_path, path)
            }
        }
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

/// `<file name>.<pid>.<seq>.tmp` beside `path`: distinct across processes and
/// across concurrent writes within one process.
fn unique_temp_path(path: &Path) -> std::path::PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(format!("{name}.{}.{seq}.tmp", std::process::id()))
}

pub(super) fn read_json_file<T: for<'de> Deserialize<'de>>(path: &Path) -> io::Result<T> {
    let bytes = fs::read(path)?;
    serde_json::from_slice(&bytes).map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}

/// Mint a session id.
///
/// Daemon-side and unique per call, which is load-bearing beyond naming
/// (#305). The spawn-storm that issue describes needs two `Create` requests
/// to race on *the same* session, and nothing can arrange that: the id is
/// minted here rather than supplied by the client — `WorkerLaunchSpec` has no
/// id field to carry one — so per-session spawn serialization would lock a
/// key that is unique by construction. `new_session_ids_never_collide` pins
/// that; if a client-supplied id is ever added, revisit #305 phase 2.
pub(super) fn new_session_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    let sequence = COUNTER.fetch_add(1, Ordering::AcqRel);
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("sess-{millis}-{sequence}")
}

pub(super) fn terminal_dimensions() -> (u16, u16) {
    if let Some((width, height)) = terminal_size::terminal_size() {
        (height.0, width.0)
    } else {
        (24, 32767)
    }
}

/// Resolve the attach-replay backlog cap in bytes. Precedence: explicit CLI
/// flag (`--backlog-size`) > `CLUD_BACKLOG_BYTES` env var > compiled default.
/// Returns `None` when no override was set, so the worker spec stays
/// wire-compatible with older daemons.
pub(super) fn resolve_backlog_bytes(cli: Option<&str>) -> Option<usize> {
    if let Some(raw) = cli {
        return parse_byte_size(raw);
    }
    if let Ok(raw) = std::env::var(ENV_BACKLOG_BYTES) {
        return parse_byte_size(&raw);
    }
    None
}

/// Parse a human-friendly byte count: `256`, `256k`, `1mb`, `2MiB`, etc.
/// Returns `None` when the input is unparseable or non-positive so we fall
/// back to the compiled default instead of misconfiguring the cap.
pub(super) fn parse_byte_size(raw: &str) -> Option<usize> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let lower = trimmed.to_ascii_lowercase();
    let (num_part, mult) = if let Some(rest) = lower
        .strip_suffix("kib")
        .or_else(|| lower.strip_suffix("kb"))
        .or_else(|| lower.strip_suffix("k"))
    {
        (rest, 1024usize)
    } else if let Some(rest) = lower
        .strip_suffix("mib")
        .or_else(|| lower.strip_suffix("mb"))
        .or_else(|| lower.strip_suffix("m"))
    {
        (rest, 1024 * 1024)
    } else if let Some(rest) = lower
        .strip_suffix("gib")
        .or_else(|| lower.strip_suffix("gb"))
        .or_else(|| lower.strip_suffix("g"))
    {
        (rest, 1024 * 1024 * 1024)
    } else if let Some(rest) = lower.strip_suffix("b") {
        (rest, 1usize)
    } else {
        (lower.as_str(), 1usize)
    };
    let n: usize = num_part.trim().parse().ok()?;
    if n == 0 {
        return None;
    }
    n.checked_mul(mult)
}

#[cfg(test)]
mod tests {
    //! Issue #25: configurable attach-replay backlog cap.
    use super::*;

    /// Concurrent writers of one JSON state file must never publish an empty
    /// or partial file. A shared `<name>.tmp` let one writer's truncating
    /// open race the other's rename (session snapshots written by both the
    /// worker and the daemon's Ctrl+C thread).
    #[test]
    fn concurrent_write_json_file_never_publishes_partial_content() {
        let tmp = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let path = tmp.path().join("sessions").join("s.json");
        let payload: Vec<u64> = (0..2048).collect();
        write_json_file(&path, &payload).unwrap();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let writers: Vec<_> = (0..4)
            .map(|_| {
                let path = path.clone();
                let payload = payload.clone();
                let stop = std::sync::Arc::clone(&stop);
                std::thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        // A failed replace (Windows sharing violation) is
                        // tolerated; a corrupt published file is not.
                        let _ = write_json_file(&path, &payload);
                    }
                })
            })
            .collect();
        for _ in 0..500 {
            if let Ok(bytes) = fs::read(&path) {
                let parsed: Vec<u64> = serde_json::from_slice(&bytes)
                    .unwrap_or_else(|e| panic!("partial JSON ({} bytes): {e}", bytes.len()));
                assert_eq!(parsed, payload);
            }
        }
        stop.store(true, Ordering::Relaxed);
        for w in writers {
            w.join().unwrap();
        }
        let leftovers: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path() != path)
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );
    }

    /// Issue #753: the daemon builds its own child env and historically
    /// drifted from `runner::child_env`. Assert the completion suppression
    /// reaches daemon-launched sessions too, exactly once.
    #[test]
    fn child_env_suppresses_git_bash_completions() {
        if !crate::platform::host::WINDOWS {
            return;
        }
        use crate::shell::completion_guard::{OPT_OUT_KEY, SUPPRESS_KEY};

        let guard = EnvGuard::unset(OPT_OUT_KEY);
        let env = child_env_from(&[]);
        drop(guard);

        let hits: Vec<_> = env.iter().filter(|(k, _)| k == SUPPRESS_KEY).collect();
        assert_eq!(
            hits.len(),
            1,
            "{SUPPRESS_KEY} must appear exactly once in the daemon child env"
        );
        assert_eq!(hits[0].1, "1");
    }

    /// The opt-out must reach the daemon path as well, and must not leave a
    /// stale inherited value behind.
    #[test]
    fn child_env_opt_out_drops_suppression() {
        if !crate::platform::host::WINDOWS {
            return;
        }
        use crate::shell::completion_guard::{OPT_OUT_KEY, SUPPRESS_KEY};

        let opt_out = EnvGuard::set(OPT_OUT_KEY, "1");
        let env = child_env_from(&[]);
        drop(opt_out);

        // Inherited ambient value (if any) is all that may remain; we must not
        // have injected one ourselves.
        let injected =
            std::env::var(SUPPRESS_KEY).is_err() && env.iter().any(|(k, _)| k == SUPPRESS_KEY);
        assert!(!injected, "{OPT_OUT_KEY}=1 must suppress the injection");
    }

    /// Issue #1066, same drift risk as the #753 pair above — the risk #1209
    /// closed by routing both paths through one builder: assert the daemon
    /// path arms nounset identically to `runner::child_env`, rather than
    /// assuming the two stayed in step.
    #[test]
    fn child_env_arms_nounset_exactly_like_the_runner() {
        use crate::shell::nounset::{BASH_ENV_KEY, OPT_OUT_KEY};

        let guard = EnvGuard::set_all(&[(OPT_OUT_KEY, None), (BASH_ENV_KEY, None)]);
        let daemon = child_env_from(&[]);
        let runner = crate::runner::child_env();
        drop(guard);

        let pick = |env: &[(String, String)]| -> Vec<String> {
            env.iter()
                .filter(|(key, _)| key == BASH_ENV_KEY)
                .map(|(_, value)| value.clone())
                .collect()
        };
        let (daemon, runner) = (pick(&daemon), pick(&runner));
        assert_eq!(
            daemon.len(),
            1,
            "{BASH_ENV_KEY} must appear exactly once in the daemon child env"
        );
        assert_eq!(
            daemon, runner,
            "the two builders must arm nounset the same way; they have drifted"
        );
    }

    /// Regression: the daemon builder used to strip a fixed key list before
    /// layering, so when the overrides came back empty — opted out, no home,
    /// unwritable state dir — nothing put the user's inherited `BASH_ENV`
    /// back, and opting out *deleted* their startup file instead of leaving
    /// the shell alone. The runner path, which layers with `push_or_replace`,
    /// never had the bug, which is precisely what made it invisible.
    #[test]
    fn opting_out_preserves_an_inherited_bash_env_in_both_builders() {
        use crate::shell::nounset::{BASH_ENV_KEY, OPT_OUT_KEY};

        let theirs = "/home/someone/their-bash-env.sh";
        // An ambient rm-shim dir (present in every clud-launched shell) makes the
        // opted-out builders chain `BASH_ENV` through the PATH-only file, which
        // is a different, intended contract. Pin it absent (#2358).
        let guard = EnvGuard::set_all(&[
            (OPT_OUT_KEY, Some("1")),
            (BASH_ENV_KEY, Some(theirs)),
            (crate::shim_session::RM_SHIM_DIR_KEY, None),
        ]);
        let daemon = child_env_from(&[]);
        let runner = crate::runner::child_env();
        drop(guard);

        for (label, env) in [("daemon", &daemon), ("runner", &runner)] {
            let got: Vec<&str> = env
                .iter()
                .filter(|(key, _)| key == BASH_ENV_KEY)
                .map(|(_, value)| value.as_str())
                .collect();
            assert_eq!(
                got,
                vec![theirs],
                "{label}: opting out must leave the user's {BASH_ENV_KEY} untouched"
            );
        }
    }

    /// #305 phase 2 proposes a per-session spawn mutex so concurrent
    /// `Create`s for one session cannot race. That collision is impossible
    /// while ids are minted per request, so this pins the property the
    /// conclusion rests on rather than the conclusion.
    ///
    /// Concurrent on purpose: a counter that looked unique single-threaded
    /// but handed the same value to two threads is exactly the failure that
    /// would resurrect the phase.
    #[test]
    fn new_session_ids_never_collide() {
        use std::collections::HashSet;

        const THREADS: usize = 8;
        const PER_THREAD: usize = 256;

        let handles: Vec<_> = (0..THREADS)
            .map(|_| {
                std::thread::spawn(|| {
                    (0..PER_THREAD)
                        .map(|_| new_session_id())
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        let ids: Vec<String> = handles
            .into_iter()
            .flat_map(|handle| handle.join().expect("id thread"))
            .collect();

        let unique: HashSet<&String> = ids.iter().collect();
        assert_eq!(
            unique.len(),
            THREADS * PER_THREAD,
            "session ids must be unique; a collision would make #305's \
             per-session spawn race reachable"
        );
    }

    /// The ids carry the millisecond they were minted in, so two created in
    /// the same millisecond still differ. Without the counter they would not,
    /// and a fast enough pair of launches would collide.
    #[test]
    fn ids_minted_in_the_same_millisecond_still_differ() {
        let first = new_session_id();
        let second = new_session_id();
        assert_ne!(first, second);
        assert!(first.starts_with("sess-"), "{first}");
    }

    #[test]
    fn parse_byte_size_raw_bytes() {
        assert_eq!(parse_byte_size("262144"), Some(262144));
        assert_eq!(parse_byte_size("1024b"), Some(1024));
        assert_eq!(parse_byte_size("  2048  "), Some(2048));
    }

    #[test]
    fn parse_byte_size_with_kb_suffix() {
        assert_eq!(parse_byte_size("256k"), Some(256 * 1024));
        assert_eq!(parse_byte_size("256kb"), Some(256 * 1024));
        assert_eq!(parse_byte_size("256KiB"), Some(256 * 1024));
        assert_eq!(parse_byte_size("256KB"), Some(256 * 1024));
    }

    #[test]
    fn parse_byte_size_with_mb_suffix() {
        assert_eq!(parse_byte_size("1m"), Some(1024 * 1024));
        assert_eq!(parse_byte_size("1MB"), Some(1024 * 1024));
        assert_eq!(parse_byte_size("1MiB"), Some(1024 * 1024));
        assert_eq!(parse_byte_size("2MB"), Some(2 * 1024 * 1024));
    }

    #[test]
    fn parse_byte_size_with_gb_suffix() {
        assert_eq!(parse_byte_size("1g"), Some(1024 * 1024 * 1024));
        assert_eq!(parse_byte_size("1gib"), Some(1024 * 1024 * 1024));
    }

    #[test]
    fn parse_byte_size_rejects_garbage() {
        assert_eq!(parse_byte_size(""), None);
        assert_eq!(parse_byte_size("abc"), None);
        assert_eq!(parse_byte_size("0"), None);
        assert_eq!(parse_byte_size("0k"), None);
        assert_eq!(parse_byte_size("-5"), None);
    }

    #[test]
    fn resolve_backlog_bytes_prefers_cli_over_env() {
        let guard = EnvGuard::set(ENV_BACKLOG_BYTES, "2mb");
        assert_eq!(resolve_backlog_bytes(Some("128k")), Some(128 * 1024));
        drop(guard);
    }

    #[test]
    fn resolve_backlog_bytes_falls_back_to_env() {
        let guard = EnvGuard::set(ENV_BACKLOG_BYTES, "512k");
        assert_eq!(resolve_backlog_bytes(None), Some(512 * 1024));
        drop(guard);
    }

    #[test]
    fn resolve_backlog_bytes_none_when_unset() {
        let guard = EnvGuard::unset(ENV_BACKLOG_BYTES);
        assert_eq!(resolve_backlog_bytes(None), None);
        drop(guard);
    }

    /// RAII env-var guard so tests that read `CLUD_BACKLOG_BYTES` don't
    /// contaminate each other or the outer process. Serial by mutex since
    /// `std::env` is process-global.
    struct EnvGuard {
        key: &'static str,
        prior: Option<String>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl EnvGuard {
        fn lock() -> std::sync::MutexGuard<'static, ()> {
            static M: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
            M.get_or_init(|| std::sync::Mutex::new(()))
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
        }

        fn set(key: &'static str, value: &str) -> Self {
            let lock = Self::lock();
            let prior = std::env::var(key).ok();
            std::env::set_var(key, value);
            Self {
                key,
                prior,
                _lock: lock,
            }
        }

        /// Several keys under one lock. `EnvGuard`'s mutex is not reentrant,
        /// so holding two single-key guards at once deadlocks — and the
        /// nounset cases below need the opt-out and an inherited `BASH_ENV`
        /// set together.
        fn set_all(vars: &[(&'static str, Option<&str>)]) -> MultiEnvGuard {
            let lock = Self::lock();
            let prior = vars
                .iter()
                .map(|(key, value)| {
                    let prior = std::env::var(*key).ok();
                    match value {
                        Some(value) => std::env::set_var(key, value),
                        None => std::env::remove_var(key),
                    }
                    (*key, prior)
                })
                .collect();
            MultiEnvGuard { prior, _lock: lock }
        }

        fn unset(key: &'static str) -> Self {
            let lock = Self::lock();
            let prior = std::env::var(key).ok();
            std::env::remove_var(key);
            Self {
                key,
                prior,
                _lock: lock,
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match self.prior.take() {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }

    struct MultiEnvGuard {
        prior: Vec<(&'static str, Option<String>)>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl Drop for MultiEnvGuard {
        fn drop(&mut self) {
            for (key, value) in self.prior.drain(..) {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    // -----------------------------------------------------------------
    // #933: the daemon's env is frozen at first-start; the initiator's is not.
    // -----------------------------------------------------------------

    fn pairs(entries: &[(&str, &str)]) -> Vec<(String, String)> {
        entries
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }

    fn value_of<'a>(env: &'a [(String, String)], key: &str) -> Option<&'a str> {
        env.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    /// The reported bug, as a unit: a daemon started yesterday hands today's
    /// agent yesterday's `PATH`, so a tool installed since is invisible to a
    /// daemon-hosted session while working fine in a foreground one.
    #[test]
    fn the_clients_path_wins_over_the_daemons_frozen_one() {
        let daemon = pairs(&[("PATH", "/from/yesterday")]);
        let client = pairs(&[("PATH", "/installed/today")]);

        let merged = merge_env(daemon, &client);

        assert_eq!(value_of(&merged, "PATH"), Some("/installed/today"));
        assert_eq!(
            merged.iter().filter(|(key, _)| key == "PATH").count(),
            1,
            "PATH must be replaced, not appended alongside the stale one"
        );
    }

    /// Replacement, not union. #933 leaves this open; a union would keep the
    /// daemon's stale entries on the path, where a shadowing old binary still
    /// wins the lookup — which is the bug, not a mitigation of it.
    #[test]
    fn path_is_replaced_rather_than_unioned() {
        let daemon = pairs(&[("PATH", "/stale/bin")]);
        let client = pairs(&[("PATH", "/fresh/bin")]);

        let merged = merge_env(daemon, &client);

        assert_eq!(value_of(&merged, "PATH"), Some("/fresh/bin"));
        assert!(
            !value_of(&merged, "PATH").unwrap().contains("/stale/bin"),
            "the stale entry survived into the merged PATH"
        );
    }

    /// A key only the daemon knows survives. Dropping the daemon's
    /// environment entirely would trade one silent divergence for another.
    #[test]
    fn daemon_only_keys_survive_the_merge() {
        let daemon = pairs(&[("CLUD_DAEMON_STATE_DIR", "/state"), ("PATH", "/old")]);
        let client = pairs(&[("PATH", "/new")]);

        let merged = merge_env(daemon, &client);

        assert_eq!(value_of(&merged, "CLUD_DAEMON_STATE_DIR"), Some("/state"));
    }

    /// A client older than this field ships nothing. That must mean "behave
    /// exactly as before", not "spawn with an empty environment" — the latter
    /// would break every session launched by a mismatched pair during a
    /// rolling upgrade.
    #[test]
    fn an_empty_client_env_leaves_the_daemon_env_untouched() {
        let daemon = pairs(&[("PATH", "/only"), ("HOME", "/root")]);

        assert_eq!(merge_env(daemon.clone(), &[]), daemon);
    }

    /// Keys the client sets that the daemon has never heard of must arrive: a
    /// venv or toolchain activated after the daemon started is this shape.
    #[test]
    fn client_only_keys_are_added() {
        let daemon = pairs(&[("PATH", "/p")]);
        let client = pairs(&[("VIRTUAL_ENV", "/proj/.venv")]);

        let merged = merge_env(daemon, &client);

        assert_eq!(value_of(&merged, "VIRTUAL_ENV"), Some("/proj/.venv"));
        assert_eq!(value_of(&merged, "PATH"), Some("/p"));
    }

    /// The admission-time login baseline, rather than the daemon process's
    /// inherited shell, is the floor. An export that belonged only to the
    /// auto-starting shell therefore cannot leak into a later session.
    #[test]
    fn login_baseline_prevents_stale_auto_starter_exports_from_leaking() {
        let login = pairs(&[("PATH", "/login/bin"), ("HOME", "/home/user")]);
        let client = pairs(&[("PATH", "/fresh/bin")]);

        let merged = session_base_from(login, &client);

        assert_eq!(value_of(&merged, "PATH"), Some("/fresh/bin"));
        assert_eq!(value_of(&merged, "HOME"), Some("/home/user"));
        assert_eq!(value_of(&merged, "VIRTUAL_ENV"), None);
    }

    #[test]
    fn daemon_unsafe_marker_only_comes_from_the_new_client() {
        let login = pairs(&[(crate::runner::UNSAFE_MODE_ENV, "1"), ("PATH", "/login")]);
        let safe = session_base_from(login.clone(), &pairs(&[("PATH", "/client")]));
        assert_eq!(value_of(&safe, crate::runner::UNSAFE_MODE_ENV), None);
        let unsafe_client =
            session_base_from(login, &pairs(&[(crate::runner::UNSAFE_MODE_ENV, "1")]));
        assert_eq!(
            value_of(&unsafe_client, crate::runner::UNSAFE_MODE_ENV),
            Some("1")
        );
        let child = child_env_with_base(unsafe_client);
        assert_eq!(value_of(&child, crate::runner::UNSAFE_MODE_ENV), Some("1"));
    }

    /// Later client entries win over earlier ones for the same key, so a
    /// duplicate in the shipped list cannot resurrect a stale value.
    #[test]
    fn the_last_client_value_for_a_key_wins() {
        let merged = merge_env(
            pairs(&[("PATH", "/daemon")]),
            &pairs(&[("PATH", "/first"), ("PATH", "/second")]),
        );

        assert_eq!(value_of(&merged, "PATH"), Some("/second"));
        assert_eq!(merged.iter().filter(|(k, _)| k == "PATH").count(), 1);
    }

    /// #933's duplicate-builder warning, asserted rather than assumed: with
    /// the client's env supplied, the daemon path must agree with the
    /// foreground path on the value the user actually has. The two builders
    /// have drifted before (#753, #1066). `PATH` is read ambiently rather than
    /// set, so this does not mutate the shared process environment.
    #[test]
    fn the_daemon_path_agrees_with_the_runner_once_the_client_env_is_supplied() {
        let client: Vec<(String, String)> = std::env::vars().collect();

        let daemon = child_env_from(&client);
        let runner = crate::runner::child_env();

        assert_eq!(
            value_of(&daemon, "PATH"),
            value_of(&runner, "PATH"),
            "daemon and foreground disagree on PATH with the client env supplied"
        );
    }

    /// RED for #1209: the two builders applied the same policy layers by
    /// hand, and had drifted - the Windows UTF-8 stdio pair existed only in
    /// `runner::child_env`, so Windows daemon sessions spawned without it.
    /// Asserted against `runner::child_env_policy_keys()`, which lists the
    /// Windows pair on every platform, so the assertion is the same sentence
    /// on a Linux lane (absent == absent) as on Windows (`1` == `1`) and
    /// fails on `main` on Windows.
    #[test]
    fn both_builders_agree_on_every_policy_key() {
        let guard = EnvGuard::set_all(&[
            (crate::shell::nounset::OPT_OUT_KEY, None),
            (crate::shell::completion_guard::OPT_OUT_KEY, None),
            ("CLUD_SESSION_ID", Some("builder-parity-test")),
        ]);
        let client: Vec<(String, String)> = std::env::vars().collect();
        let daemon = child_env_from(&client);
        let runner = crate::runner::child_env();
        drop(guard);

        for key in crate::runner::child_env_policy_keys() {
            assert_eq!(
                value_of(&daemon, key),
                value_of(&runner, key),
                "policy key {key} drifted between the daemon and runner child-env builders"
            );
        }
    }

    /// #1209: one builder, not two that happen to agree today. Compared
    /// against a synthetic base so the assertion does not depend on this
    /// process's environment. Paired with the forced-`windows_stdio` tests in
    /// `runner_execution.rs`: those prove the shared builder applies the
    /// Windows stdio layer, and this proves the daemon path is that builder,
    /// which together cover "daemon sessions get UTF-8 stdio" on a Linux lane.
    ///
    /// Holds the `EnvGuard` mutex without changing anything: both calls read
    /// the process environment through `nounset`/`session_tmp`, so a
    /// concurrent test flipping an opt-out between them would make the two
    /// sides differ for a reason that is not drift.
    #[test]
    fn the_daemon_path_is_the_shared_builder() {
        let _env_lock = EnvGuard::set_all(&[]);
        let home = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let home_path = home.path().to_string_lossy().to_string();
        let base = pairs(&[
            ("PATH", "/client/only/bin"),
            ("HOME", home_path.as_str()),
            ("USERPROFILE", home_path.as_str()),
            ("CLUD_SESSION_ID", "builder-parity-test"),
        ]);

        assert_eq!(
            child_env_with_base(base.clone()),
            crate::runner::apply_child_env_policy(base)
        );
    }

    /// #1160: a reader racing a writer must never observe the record missing.
    /// The old delete-then-rename left the name unbound between the two
    /// calls; on the Windows unit lane a concurrent `store.get` fell into that
    /// gap and the idempotent replay came back `404`.
    #[test]
    fn write_json_file_never_leaves_the_target_missing_for_a_concurrent_reader() {
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;

        let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let path = dir.path().join("record.json");
        write_json_file(&path, &0_u64).unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let reader_stop = Arc::clone(&stop);
        let reader_path = path.clone();
        let reader = std::thread::spawn(move || {
            let mut reads = 0_u64;
            while !reader_stop.load(Ordering::SeqCst) {
                match read_json_file::<u64>(&reader_path) {
                    Ok(_) => reads += 1,
                    Err(err) if err.kind() == io::ErrorKind::NotFound => {
                        panic!("reader observed the record missing after {reads} reads: {err}")
                    }
                    // A replace in flight may momentarily refuse the open on
                    // Windows; that is a retryable condition, not a missing
                    // file, and not what this test guards.
                    Err(_) => {}
                }
            }
            reads
        });

        for value in 1..=500_u64 {
            write_json_file(&path, &value).unwrap();
        }
        stop.store(true, Ordering::SeqCst);
        let reads = reader.join().expect("reader must never see NotFound");
        assert!(reads > 0, "the reader never got a look at the record");
        assert_eq!(read_json_file::<u64>(&path).unwrap(), 500);
    }
}
